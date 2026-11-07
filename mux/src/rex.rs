//! A domain that attaches wezterm to a [Rex](https://www.superlogical.com/rex)
//! terminal server.
//!
//! Rex keeps sessions, windows, split layouts and terminals on the server.
//! The domain shows one session: its Rex windows become tabs in one wezterm
//! window, and each window's split tree becomes the tab's panes. The server
//! owns all of it, so layout changes made anywhere (this client, another
//! Rex client, `rex` on the command line) show up here, and wezterm's own
//! split/new-tab/close actions are sent to the server as requests.
//!
//! Each terminal block is a [`RexPane`]: a libghostty-vt terminal restored
//! from the server's snapshot and kept current by its output stream. Keys,
//! mouse, wheel and paste go to the server as events, where the
//! authoritative terminal encodes them. The local terminal's own replies to
//! queries are dropped: the server's terminal already answered.
//!
//! The protocol lives in the `wezterm-rex` crate.
use crate::domain::{alloc_domain_id, Domain, DomainId, DomainState, SplitSource};
use crate::localpane::search_terminal;
use crate::pane::{
    alloc_pane_id, CachePolicy, CloseReason, ForEachPaneLogicalLine, LogicalLine, Pane, PaneId,
    Pattern, SearchResult, WithPaneLines,
};
use crate::renderable::{RenderableDimensions, StableCursorPosition};
use crate::tab::{
    PaneEntry, PaneNode, SplitDirection, SplitDirectionAndSize, SplitRequest, SplitSize, Tab, TabId,
};
use crate::window::WindowId;
use crate::{Mux, MuxNotification};
use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use portable_pty::CommandBuilder;
use rangeset::RangeSet;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use termwiz::input::KeyboardEncoding;
use termwiz::surface::{Line, SequenceNo};
use url::Url;
use wezterm_ghostty::Terminal;
use wezterm_rex::host::{EventSink, Reply};
use wezterm_rex::input::{self as rexinput, KeyAction, MouseAction};
use wezterm_rex::{
    Direction, Host, HostCommand, HostConfig, HostEvent, Input, StreamEvent, ViewLayoutNode,
    ViewResponse, Window as RexWindow,
};
use wezterm_term::color::ColorPalette;
use wezterm_term::{
    Alert, AlertHandler, Clipboard, DownloadHandler, KeyCode, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind, Progress, SemanticZone, StableRowIndex, TerminalConfiguration,
    TerminalSize,
};

/// How long to wait for the server to answer an attach, a new tab or a split.
const SERVER_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a block we asked to close stays hidden while the server catches
/// up, so a view sent in between doesn't bring its tab back.
const CLOSING_GRACE: Duration = Duration::from_secs(5);
/// Fresh-snapshot retries after a snapshot that couldn't be restored.
const RESTORE_RETRIES: u8 = 3;

async fn with_timeout<T>(what: &str, rx: flume::Receiver<T>) -> anyhow::Result<T> {
    let wait = async {
        rx.recv_async()
            .await
            .map_err(|_| anyhow!("Rex domain went away"))
    };
    let timeout = async {
        smol::Timer::after(SERVER_TIMEOUT).await;
        Err(anyhow!("timed out waiting for the Rex server to {what}"))
    };
    smol::future::or(wait, timeout).await
}

// ---------------------------------------------------------------------
// RexPane

/// Writes typed text (wezterm's `SendString`, composed IME text) to the
/// server's terminal.
struct RexWriter {
    host: flume::Sender<HostCommand>,
    session_id: String,
    block_id: String,
}

impl Write for RexWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let data = String::from_utf8_lossy(buf).into_owned();
        self.host
            .send(HostCommand::Input {
                session_id: self.session_id.clone(),
                block_id: self.block_id.clone(),
                input: Input::Write(data),
            })
            .map_err(|_| std::io::Error::other("Rex host stopped"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct RexAlertHandler {
    pane_id: PaneId,
}

impl AlertHandler for RexAlertHandler {
    fn alert(&mut self, alert: Alert) {
        let pane_id = self.pane_id;
        promise::spawn::spawn_into_main_thread(async move {
            let Some(mux) = Mux::try_get() else { return };
            if let Alert::WindowTitleChanged(title) = &alert {
                if let Some((_domain, window_id, _tab_id)) = mux.resolve_pane_id(pane_id) {
                    if let Some(mut window) = mux.get_window_mut(window_id) {
                        window.set_title(title);
                    }
                }
            }
            mux.notify(MuxNotification::Alert { pane_id, alert });
        })
        .detach();
    }
}

/// One Rex terminal block.
pub struct RexPane {
    pane_id: PaneId,
    domain_id: DomainId,
    session_id: String,
    block_id: String,
    terminal: Mutex<Terminal>,
    host: flume::Sender<HostCommand>,
    shared: Weak<Shared>,
    writer: Mutex<RexWriter>,
    label: Mutex<Option<String>>,
    /// Status of the data stream ("Restoring terminal…", ...), shown in the
    /// title while the stream isn't live.
    stream_status: Mutex<Option<String>>,
    exit_code: Mutex<Option<Option<i64>>>,
    /// Our latest size request, for the pixel geometry of server resizes.
    requested: Mutex<Option<TerminalSize>>,
    /// Rex key names whose press was sent, so only their releases follow.
    pressed: Mutex<HashSet<String>>,
    restore_failures: AtomicU8,
    mouse_buttons: Mutex<Vec<MouseButton>>,
    /// The block is gone on the server; nothing more to send.
    server_gone: AtomicBool,
    /// Closing this pane only detaches (the wezterm window closed, or the
    /// domain detached); the block keeps running on the server.
    keep_on_server: AtomicBool,
    dead: AtomicBool,
}

impl RexPane {
    fn new(
        domain_id: DomainId,
        session_id: &str,
        block_id: &str,
        label: Option<String>,
        size: TerminalSize,
        host: flume::Sender<HostCommand>,
        shared: Weak<Shared>,
    ) -> Self {
        let pane_id = alloc_pane_id();
        let mut terminal = Terminal::new(
            size,
            Arc::new(config::TermConfig::new()),
            "WezTerm",
            config::wezterm_version(),
            // The server's terminal answers queries; ours stays quiet.
            Box::new(std::io::sink()),
        );
        terminal.set_server_owned(true);
        terminal.set_notification_handler(Box::new(RexAlertHandler { pane_id }));
        Self {
            pane_id,
            domain_id,
            session_id: session_id.into(),
            block_id: block_id.into(),
            terminal: Mutex::new(terminal),
            writer: Mutex::new(RexWriter {
                host: host.clone(),
                session_id: session_id.into(),
                block_id: block_id.into(),
            }),
            host,
            shared,
            label: Mutex::new(label),
            stream_status: Mutex::new(Some("Restoring terminal…".into())),
            exit_code: Mutex::new(None),
            requested: Mutex::new(None),
            pressed: Mutex::new(HashSet::new()),
            restore_failures: AtomicU8::new(0),
            mouse_buttons: Mutex::new(vec![]),
            server_gone: AtomicBool::new(false),
            keep_on_server: AtomicBool::new(false),
            dead: AtomicBool::new(false),
        }
    }

    pub fn block_id(&self) -> &str {
        &self.block_id
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Input goes only to a live terminal: while the control connection or
    /// this pane's stream is down it is dropped here, never queued for later.
    fn send(&self, input: Input) {
        if self.server_gone.load(Ordering::Relaxed) || self.stream_status.lock().is_some() {
            return;
        }
        let connected = self
            .shared
            .upgrade()
            .map_or(false, |s| s.connected.load(Ordering::Relaxed));
        if !connected {
            return;
        }
        let _ = self.host.send(HostCommand::Input {
            session_id: self.session_id.clone(),
            block_id: self.block_id.clone(),
            input,
        });
    }

    fn request(&self, method: &str, mut payload: Value) {
        payload["session_id"] = json!(self.session_id);
        let _ = self.host.send(HostCommand::Request {
            method: method.into(),
            payload,
            reply: None,
        });
    }

    /// Apply stream output. Called on the Rex host thread.
    fn apply_stream(&self, event: StreamEvent) {
        let mut changed = true;
        match event {
            StreamEvent::Snapshot(bytes) => {
                let mut term = self.terminal.lock();
                match term.restore_snapshot(&bytes) {
                    Ok(()) => {
                        self.restore_failures.store(0, Ordering::Relaxed);
                        *self.stream_status.lock() = None;
                    }
                    Err(err) => {
                        log::error!("rex: block {}: {err:#}", self.block_id);
                        *self.stream_status.lock() = Some(format!("Restore failed: {err:#}"));
                        // Output after a bad snapshot would land on the
                        // wrong screen; start over from a fresh one.
                        if self.restore_failures.fetch_add(1, Ordering::Relaxed) < RESTORE_RETRIES {
                            let _ = self.host.send(HostCommand::RestartStream {
                                session_id: self.session_id.clone(),
                                block_id: self.block_id.clone(),
                            });
                        }
                    }
                }
            }
            StreamEvent::Output(bytes) => {
                if self.restore_failures.load(Ordering::Relaxed) > 0 {
                    // Waiting for a fresh snapshot.
                    return;
                }
                self.terminal.lock().advance_bytes(bytes);
            }
            StreamEvent::Resize {
                cols,
                rows,
                cell_width,
                cell_height,
            } => {
                let mut term = self.terminal.lock();
                let current = term.get_size();
                let requested = *self.requested.lock();
                let fallback = |px: usize, n: usize| if n > 0 { px / n } else { 0 };
                let (cw, ch) = match (cell_width, cell_height) {
                    (w, h) if w > 0 && h > 0 => (w as usize, h as usize),
                    _ => {
                        let base = requested.unwrap_or(current);
                        (
                            fallback(base.pixel_width, base.cols),
                            fallback(base.pixel_height, base.rows),
                        )
                    }
                };
                let size = TerminalSize {
                    cols: cols as usize,
                    rows: rows as usize,
                    pixel_width: cw * cols as usize,
                    pixel_height: ch * rows as usize,
                    dpi: requested.map(|s| s.dpi).unwrap_or(current.dpi),
                };
                if size != current {
                    term.resize(size);
                } else {
                    changed = false;
                }
            }
            StreamEvent::Theme(theme) => {
                self.terminal.lock().set_default_colors(
                    theme.foreground,
                    theme.background,
                    theme.cursor,
                    &theme.palette,
                );
            }
            StreamEvent::Exited(code) => {
                *self.exit_code.lock() = Some(code);
            }
            StreamEvent::Status { live, message } => {
                *self.stream_status.lock() = if live { None } else { Some(message) };
            }
            StreamEvent::Closed => {
                self.server_gone.store(true, Ordering::Relaxed);
                self.dead.store(true, Ordering::Relaxed);
                let pane_id = self.pane_id;
                promise::spawn::spawn_into_main_thread(async move {
                    if let Some(mux) = Mux::try_get() {
                        mux.remove_pane(pane_id);
                    }
                })
                .detach();
            }
        }
        if changed {
            Mux::notify_from_any_thread(MuxNotification::PaneOutput(self.pane_id));
        }
    }
}

fn mouse_button_name(button: MouseButton) -> Option<&'static str> {
    match button {
        MouseButton::Left => Some("left"),
        MouseButton::Middle => Some("middle"),
        MouseButton::Right => Some("right"),
        _ => None,
    }
}

#[async_trait(?Send)]
impl Pane for RexPane {
    fn pane_id(&self) -> PaneId {
        self.pane_id
    }

    fn get_cursor_position(&self) -> StableCursorPosition {
        let term = self.terminal.lock();
        let pos = term.cursor_pos();
        StableCursorPosition {
            x: pos.x,
            y: term.visible_row_to_stable_row(pos.y),
            shape: pos.shape,
            visibility: pos.visibility,
        }
    }

    fn get_keyboard_encoding(&self) -> KeyboardEncoding {
        // Keys always go to the server as key events (`key_down`/`key_up`);
        // its terminal does the kitty/xterm encoding. Reporting our mirror's
        // kitty flags would make the GUI encode keys itself and send bytes.
        KeyboardEncoding::Xterm
    }

    fn get_current_seqno(&self) -> SequenceNo {
        self.terminal.lock().current_seqno()
    }

    fn get_changed_since(
        &self,
        lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex> {
        let mut set = RangeSet::new();
        for range in self.terminal.lock().get_changed_stable_rows(lines, seqno) {
            set.add_range(range);
        }
        set
    }

    fn for_each_logical_line_in_stable_range_mut(
        &self,
        lines: Range<StableRowIndex>,
        for_line: &mut dyn ForEachPaneLogicalLine,
    ) {
        self.terminal
            .lock()
            .for_each_logical_line_in_stable_range_mut(lines, |range, lines| {
                for_line.with_logical_line_mut(range, lines)
            });
    }

    fn with_lines_mut(&self, lines: Range<StableRowIndex>, with_lines: &mut dyn WithPaneLines) {
        self.terminal.lock().with_lines_mut(lines, |first, lines| {
            with_lines.with_lines_mut(first, lines)
        })
    }

    fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        crate::pane::impl_get_lines_via_with_lines(self, lines)
    }

    fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
        crate::pane::impl_get_logical_lines_via_get_lines(self, lines)
    }

    fn get_dimensions(&self) -> RenderableDimensions {
        let d = self.terminal.lock().dimensions();
        RenderableDimensions {
            cols: d.cols,
            viewport_rows: d.viewport_rows,
            scrollback_rows: d.scrollback_rows,
            physical_top: d.physical_top,
            scrollback_top: d.scrollback_top,
            dpi: d.dpi,
            pixel_width: d.pixel_width,
            pixel_height: d.pixel_height,
            reverse_video: d.reverse_video,
        }
    }

    fn get_title(&self) -> String {
        let title = self.terminal.lock().get_title().to_string();
        let mut title = if title.is_empty() || title == "wezterm" {
            self.label
                .lock()
                .clone()
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| "rex".to_string())
        } else {
            title
        };
        if let Some(status) = self.stream_status.lock().as_ref() {
            title = format!("{title} ({status})");
        } else if let Some(code) = *self.exit_code.lock() {
            match code {
                Some(code) => title = format!("{title} (exited {code})"),
                None => title = format!("{title} (exited)"),
            }
        }
        title
    }

    fn get_progress(&self) -> Progress {
        self.terminal.lock().get_progress()
    }

    fn copy_user_vars(&self) -> HashMap<String, String> {
        self.terminal.lock().user_vars().clone()
    }

    fn send_paste(&self, text: &str) -> anyhow::Result<()> {
        Mux::get().record_input_for_current_identity();
        self.send(Input::Paste(text.to_string()));
        Ok(())
    }

    fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
        // Output arrives over the Rex data stream, not a pty.
        Ok(None)
    }

    fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
        Mux::get().record_input_for_current_identity();
        MutexGuard::map(self.writer.lock(), |writer| {
            let w: &mut dyn std::io::Write = writer;
            w
        })
    }

    fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
        // Ask the server. Our terminal follows when the server reports the
        // size it settled on (another client may own the block).
        *self.requested.lock() = Some(size);
        if self.server_gone.load(Ordering::Relaxed) || size.cols == 0 || size.rows == 0 {
            return Ok(());
        }
        let cell = |px: usize, n: usize| (px / n.max(1)).min(u16::MAX as usize) as u16;
        let mut cell_width = cell(size.pixel_width, size.cols);
        let mut cell_height = cell(size.pixel_height, size.rows);
        if cell_width == 0 || cell_height == 0 {
            // wezterm can ask for a size before its window knows its pixel
            // geometry; the server rejects zero cell metrics, so reuse the
            // cell size the terminal has now, or wait for a later resize.
            let current = self.terminal.lock().get_size();
            cell_width = cell(current.pixel_width, current.cols);
            cell_height = cell(current.pixel_height, current.rows);
            if cell_width == 0 || cell_height == 0 {
                return Ok(());
            }
        }
        let _ = self.host.send(HostCommand::Resize {
            session_id: self.session_id.clone(),
            block_id: self.block_id.clone(),
            cols: size.cols.min(u16::MAX as usize) as u16,
            rows: size.rows.min(u16::MAX as usize) as u16,
            cell_width,
            cell_height,
        });
        Ok(())
    }

    fn key_down(&self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()> {
        Mux::get().record_input_for_current_identity();
        if let Some(payload) = rexinput::key_payload(key, mods, KeyAction::Press) {
            if let Some(name) = payload["key"].as_str() {
                self.pressed.lock().insert(name.to_string());
            }
            self.send(Input::Key(payload));
        }
        Ok(())
    }

    fn key_up(&self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()> {
        // Only releases of presses we sent, matched by Rex's key name so a
        // shift change between press and release ('H' then 'h') still pairs.
        if let Some(payload) = rexinput::key_payload(key, mods, KeyAction::Release) {
            let sent = payload["key"]
                .as_str()
                .map_or(false, |name| self.pressed.lock().remove(name));
            if sent {
                self.send(Input::Key(payload));
            }
        }
        Ok(())
    }

    fn mouse_event(&self, event: MouseEvent) -> anyhow::Result<()> {
        Mux::get().record_input_for_current_identity();
        let (cols, rows) = {
            let d = self.terminal.lock().dimensions();
            (d.cols.max(1), d.viewport_rows.max(1))
        };
        let x = event.x.min(cols - 1);
        let y = (event.y.max(0) as usize).min(rows - 1);
        let wheel = match event.button {
            MouseButton::WheelUp(n) => Some((0, -(n.max(1) as i32))),
            MouseButton::WheelDown(n) => Some((0, n.max(1) as i32)),
            MouseButton::WheelLeft(n) => Some((-(n.max(1) as i32), 0)),
            MouseButton::WheelRight(n) => Some((n.max(1) as i32, 0)),
            _ => None,
        };
        if let Some((dx, dy)) = wheel {
            if event.kind == MouseEventKind::Press {
                // One event per line, at most 8 per turn, as rex-linux does.
                let steps = dx.abs().max(dy.abs()).min(8);
                for _ in 0..steps {
                    self.send(Input::Wheel(rexinput::wheel_payload(
                        x,
                        y,
                        dx.signum(),
                        dy.signum(),
                    )));
                }
            }
            return Ok(());
        }
        let action = match event.kind {
            MouseEventKind::Press => {
                self.mouse_buttons.lock().push(event.button);
                MouseAction::Press
            }
            MouseEventKind::Release => {
                self.mouse_buttons.lock().retain(|b| *b != event.button);
                MouseAction::Release
            }
            MouseEventKind::Move => MouseAction::Motion,
        };
        // Motion reports name the held button, if any (for drags).
        let button = match action {
            MouseAction::Motion => self
                .mouse_buttons
                .lock()
                .last()
                .copied()
                .and_then(mouse_button_name),
            _ => mouse_button_name(event.button),
        };
        if action != MouseAction::Motion && button.is_none() {
            return Ok(());
        }
        self.send(Input::Mouse(rexinput::mouse_payload(
            action,
            button,
            x,
            y,
            event.modifiers,
        )));
        Ok(())
    }

    fn perform_actions(&self, actions: Vec<termwiz::escape::Action>) {
        // wezterm-internal output (error messages); shown locally only.
        self.terminal.lock().perform_actions(actions)
    }

    fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }

    fn kill(&self) {
        let keep = self.keep_on_server.load(Ordering::Relaxed);
        let close = !keep && !self.server_gone.swap(true, Ordering::Relaxed);
        if close {
            self.request("block.close", json!({"block_id": self.block_id}));
        }
        self.dead.store(true, Ordering::Relaxed);
        // Forget this pane. If the block survives on the server (the close
        // was refused, or we only detached), a later view gets a fresh pane.
        if let Some(shared) = self.shared.upgrade() {
            let mut panes = shared.panes.lock();
            if panes.get(&self.block_id).map(|p| p.pane_id) == Some(self.pane_id) {
                panes.remove(&self.block_id);
            }
            drop(panes);
            if close {
                shared
                    .closing
                    .lock()
                    .insert(self.block_id.clone(), Instant::now());
            }
        }
        let _ = self.host.send(HostCommand::CloseStream {
            block_id: self.block_id.clone(),
        });
    }

    fn palette(&self) -> ColorPalette {
        self.terminal.lock().palette()
    }

    fn domain_id(&self) -> DomainId {
        self.domain_id
    }

    fn focus_changed(&self, focused: bool) {
        // Focus reports (DEC 1004) come from the server's terminal.
        if focused && !self.server_gone.load(Ordering::Relaxed) {
            self.request("session.focus_block", json!({"block_id": self.block_id}));
            // Size follows ownership: take the blocks this client shows,
            // as Rex's own clients do while focused. Later, because the
            // mux may hold its window lock while telling us.
            let pane_id = self.pane_id;
            let host = self.host.clone();
            let session_id = self.session_id.clone();
            promise::spawn::spawn_into_main_thread(async move {
                let Some(mux) = Mux::try_get() else { return };
                let Some((_, _, tab_id)) = mux.resolve_pane_id(pane_id) else {
                    return;
                };
                let Some(tab) = mux.get_tab(tab_id) else {
                    return;
                };
                let block_ids: Vec<String> = tab
                    .iter_panes()
                    .iter()
                    .filter_map(|pos| {
                        let pane = pos.pane.downcast_ref::<RexPane>()?;
                        (!pane.server_gone.load(Ordering::Relaxed)).then(|| pane.block_id.clone())
                    })
                    .collect();
                if !block_ids.is_empty() {
                    let _ = host.send(HostCommand::Claim {
                        session_id,
                        block_ids,
                    });
                }
            })
            .detach();
        }
    }

    fn has_unseen_output(&self) -> bool {
        self.terminal.lock().has_unseen_output()
    }

    fn can_close_without_prompting(&self, reason: CloseReason) -> bool {
        match reason {
            // Closing the wezterm window detaches the domain (the mux calls
            // `detach` before it removes the tabs), so the programs keep
            // running on the server and there is nothing to confirm.
            CloseReason::Window => true,
            // Closing a pane or tab ends its programs on the server.
            CloseReason::Pane | CloseReason::Tab => false,
        }
    }

    fn get_semantic_zones(&self) -> anyhow::Result<Vec<SemanticZone>> {
        self.terminal.lock().get_semantic_zones()
    }

    async fn search(
        &self,
        pattern: Pattern,
        range: Range<StableRowIndex>,
        limit: Option<u32>,
    ) -> anyhow::Result<Vec<SearchResult>> {
        let mut term = self.terminal.lock();
        search_terminal(&mut term, pattern, range, limit)
    }

    fn is_mouse_grabbed(&self) -> bool {
        self.terminal.lock().is_mouse_grabbed()
    }

    fn is_alt_screen_active(&self) -> bool {
        self.terminal.lock().is_alt_screen_active()
    }

    fn set_clipboard(&self, clipboard: &Arc<dyn Clipboard>) {
        self.terminal.lock().set_clipboard(clipboard);
    }

    fn set_download_handler(&self, handler: &Arc<dyn DownloadHandler>) {
        self.terminal.lock().set_download_handler(handler);
    }

    fn set_config(&self, config: Arc<dyn TerminalConfiguration>) {
        self.terminal.lock().set_config(config);
    }

    fn get_config(&self) -> Option<Arc<dyn TerminalConfiguration>> {
        Some(self.terminal.lock().get_config())
    }

    fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
        // A path on the server's machine.
        self.terminal.lock().get_current_dir().cloned()
    }

    fn tty_name(&self) -> Option<String> {
        None
    }
}

// ---------------------------------------------------------------------
// RexDomain

/// Turn a Rex window's tiled layout into a wezterm pane tree. Leaves are
/// terminal blocks; anything else is dropped and its split collapses.
struct TreeBuilder<'a> {
    window: &'a RexWindow,
    terminals: &'a HashSet<String>,
    /// Pane ids by block id; leaves without one get a new pane.
    panes: &'a HashMap<String, PaneId>,
    window_id: WindowId,
    tab_id: TabId,
    workspace: String,
    /// Blocks placed in this tree, with the pane id assigned to each.
    placed: Vec<(String, PaneId)>,
}

impl TreeBuilder<'_> {
    fn build(&mut self, size: TerminalSize) -> PaneNode {
        // Floating layers have no wezterm equivalent; the tiled layer is the
        // window's layout. Minimized layers are not drawn.
        let tiled = self
            .window
            .layers
            .iter()
            .filter(|l| l.state.as_deref() != Some("minimized"))
            .find(|l| l.kind == "tiled")
            .or_else(|| {
                self.window
                    .layers
                    .iter()
                    .find(|l| l.state.as_deref() != Some("minimized"))
            });
        match tiled.and_then(|l| l.layout.as_ref()) {
            Some(node) => self.node(node, size),
            None => PaneNode::Empty,
        }
    }

    fn node(&mut self, node: &ViewLayoutNode, size: TerminalSize) -> PaneNode {
        if let Some(block) = &node.block_id {
            if !self.terminals.contains(block) {
                return PaneNode::Empty;
            }
            let pane_id = self.panes.get(block).copied().unwrap_or_else(alloc_pane_id);
            self.placed.push((block.clone(), pane_id));
            return PaneNode::Leaf(PaneEntry {
                window_id: self.window_id,
                tab_id: self.tab_id,
                pane_id,
                title: String::new(),
                size,
                working_dir: None,
                is_active_pane: self.window.focused_block_id.as_deref() == Some(block),
                is_zoomed_pane: self.window.zoomed_block_id.as_deref() == Some(block),
                workspace: self.workspace.clone(),
                cursor_pos: StableCursorPosition::default(),
                physical_top: 0,
                top_row: 0,
                left_col: 0,
                tty_name: None,
            });
        }
        let Some(split) = &node.split else {
            return PaneNode::Empty;
        };
        let (Some(before), Some(after)) = (&split.before, &split.after) else {
            return PaneNode::Empty;
        };
        let direction = match split.direction {
            Direction::Vertical => SplitDirection::Vertical,
            // "Draw a direction you don't know as horizontal."
            _ => SplitDirection::Horizontal,
        };
        let (first, second) = split_sizes(size, direction, split.ratio);
        let left = self.node(before, first);
        let right = self.node(after, second);
        match (left, right) {
            (PaneNode::Empty, other) | (other, PaneNode::Empty) => other,
            (left, right) => PaneNode::Split {
                left: Box::new(left),
                right: Box::new(right),
                node: SplitDirectionAndSize {
                    direction,
                    first,
                    second,
                },
            },
        }
    }
}

/// Divide `size` between the two sides of a split, leaving one cell for the
/// divider as wezterm does. `ratio` is the first side's share.
fn split_sizes(
    size: TerminalSize,
    direction: SplitDirection,
    ratio: f64,
) -> (TerminalSize, TerminalSize) {
    let ratio = if ratio.is_finite() {
        ratio.clamp(0.0, 1.0)
    } else {
        0.5
    };
    let split = |total: usize, px: usize| {
        let avail = total.saturating_sub(1).max(2);
        let first = ((avail as f64 * ratio).round() as usize).clamp(1, avail - 1);
        let second = avail - first;
        let cell = if total > 0 { px / total } else { 0 };
        (first, second, cell)
    };
    match direction {
        SplitDirection::Horizontal => {
            let (a, b, cell) = split(size.cols, size.pixel_width);
            (
                TerminalSize {
                    cols: a,
                    pixel_width: a * cell,
                    ..size
                },
                TerminalSize {
                    cols: b,
                    pixel_width: b * cell,
                    ..size
                },
            )
        }
        SplitDirection::Vertical => {
            let (a, b, cell) = split(size.rows, size.pixel_height);
            (
                TerminalSize {
                    rows: a,
                    pixel_height: a * cell,
                    ..size
                },
                TerminalSize {
                    rows: b,
                    pixel_height: b * cell,
                    ..size
                },
            )
        }
    }
}

#[derive(Default)]
struct State {
    connected: bool,
    status: String,
    selected: Option<String>,
    views: HashMap<String, ViewResponse>,
    sessions: Vec<wezterm_rex::Session>,
    window_id: Option<WindowId>,
    /// wezterm tabs by Rex window id.
    tabs: HashMap<String, TabId>,
    attach_waiters: Vec<flume::Sender<anyhow::Result<()>>>,
    tab_waiters: HashMap<String, Vec<flume::Sender<TabId>>>,
    pane_waiters: HashMap<String, Vec<flume::Sender<PaneId>>>,
    /// An attach is waiting for its first view.
    attaching: bool,
}

/// Shared with the Rex host thread, which applies stream output directly.
struct Shared {
    panes: Mutex<HashMap<String, Arc<RexPane>>>,
    /// Blocks we asked to close, and when.
    closing: Mutex<HashMap<String, Instant>>,
    /// The control connection is up; input is dropped otherwise.
    connected: AtomicBool,
    /// Bumped for each host, so a stopping host's late events are ignored.
    generation: AtomicU64,
}

pub struct RexDomain {
    id: DomainId,
    config: config::RexDomain,
    host: Mutex<Option<Host>>,
    shared: Arc<Shared>,
    state: Mutex<State>,
}

impl RexDomain {
    pub fn new(config: &config::RexDomain) -> Self {
        Self {
            id: alloc_domain_id(),
            config: config.clone(),
            host: Mutex::new(None),
            shared: Arc::new(Shared {
                panes: Mutex::new(HashMap::new()),
                closing: Mutex::new(HashMap::new()),
                connected: AtomicBool::new(false),
                generation: AtomicU64::new(0),
            }),
            state: Mutex::new(State::default()),
        }
    }

    pub fn connect_automatically(&self) -> bool {
        self.config.connect_automatically
    }

    fn endpoint(&self) -> String {
        self.config
            .endpoint
            .clone()
            .unwrap_or_else(wezterm_rex::local_endpoint)
    }

    fn host_sender(&self) -> anyhow::Result<flume::Sender<HostCommand>> {
        self.host
            .lock()
            .as_ref()
            .map(Host::sender)
            .ok_or_else(|| anyhow!("Rex domain {} is not attached", self.config.name))
    }

    fn start_host(&self) -> anyhow::Result<()> {
        let mut host = self.host.lock();
        if host.is_some() {
            return Ok(());
        }
        let shared = Arc::clone(&self.shared);
        let domain_id = self.id;
        let generation = shared.generation.fetch_add(1, Ordering::SeqCst) + 1;
        shared.connected.store(false, Ordering::SeqCst);
        let sink: EventSink = Arc::new(move |event| match event {
            // A host that is shutting down may still be mid-connect.
            _ if shared.generation.load(Ordering::SeqCst) != generation => {}
            // Output goes straight to the pane, on the host thread, as a
            // local pane's pty reader would.
            HostEvent::Stream { block_id, event } => {
                let pane = shared.panes.lock().get(&block_id).cloned();
                match pane {
                    Some(pane) => pane.apply_stream(event),
                    None => log::trace!("rex: output for unknown block {block_id}"),
                }
            }
            // Everything that touches tabs and windows runs on the main
            // thread.
            event => {
                if let HostEvent::Status { connected, .. } = &event {
                    // Set here, not on the main thread, so input stops at once.
                    shared.connected.store(*connected, Ordering::SeqCst);
                }
                promise::spawn::spawn_into_main_thread(async move {
                    let Some(mux) = Mux::try_get() else { return };
                    let Some(domain) = mux.get_domain(domain_id) else {
                        return;
                    };
                    if let Some(domain) = domain.downcast_ref::<RexDomain>() {
                        if domain.shared.generation.load(Ordering::SeqCst) == generation {
                            domain.handle_event(event);
                        }
                    }
                })
                .detach();
            }
        });
        *host = Some(Host::spawn(
            HostConfig {
                endpoint: self.endpoint(),
                session: self.config.session.clone(),
                client_name: "WezTerm".into(),
                client_version: config::wezterm_version().to_string(),
            },
            sink,
        )?);
        Ok(())
    }

    fn handle_event(&self, event: HostEvent) {
        match event {
            HostEvent::Status { connected, message } => {
                log::info!("rex {}: {message}", self.config.name);
                let mut state = self.state.lock();
                state.connected = connected;
                state.status = message.clone();
                if !connected {
                    // An attach in progress fails rather than waiting out
                    // the retries.
                    for waiter in state.attach_waiters.drain(..) {
                        let _ = waiter.send(Err(anyhow!("{message}")));
                    }
                }
            }
            HostEvent::Connected { client_id, .. } => {
                log::debug!("rex {}: connected as {client_id}", self.config.name);
            }
            HostEvent::Sessions(sessions) => {
                self.state.lock().sessions = sessions;
            }
            HostEvent::Selected(selected) => {
                let view = {
                    let mut state = self.state.lock();
                    state.selected = selected.clone();
                    selected
                        .as_ref()
                        .and_then(|id| state.views.get(id).cloned())
                };
                match (selected, view) {
                    (None, _) => {
                        let waiters: Vec<_> = self.state.lock().attach_waiters.drain(..).collect();
                        for waiter in waiters {
                            let _ = waiter.send(Err(anyhow!(
                                "no Rex session to show; create one with `rex` or name one with `session = ...`"
                            )));
                        }
                        // Nothing left to follow.
                        let _ = self.detach();
                    }
                    (Some(id), Some(view)) => self.show(&id, &view),
                    (Some(_), None) => {}
                }
            }
            HostEvent::View { session_id, view } => {
                let selected = {
                    let mut state = self.state.lock();
                    state.views.insert(session_id.clone(), view.clone());
                    state.selected.as_deref() == Some(session_id.as_str())
                };
                if selected {
                    self.show(&session_id, &view);
                }
            }
            HostEvent::SessionRemoved { session_id } => {
                let selected = {
                    let mut state = self.state.lock();
                    state.views.remove(&session_id);
                    state.selected.as_deref() == Some(session_id.as_str())
                };
                if selected {
                    log::info!("rex {}: session ended, detaching", self.config.name);
                    let _ = self.detach();
                }
            }
            HostEvent::BlockClosed { block_id } => {
                let pane = self.shared.panes.lock().remove(&block_id);
                if let Some(pane) = pane {
                    pane.server_gone.store(true, Ordering::Relaxed);
                    pane.dead.store(true, Ordering::Relaxed);
                    Mux::get().remove_pane(pane.pane_id);
                }
            }
            HostEvent::OwnerChanged { .. } => {}
            HostEvent::Notice(message) => {
                log::warn!("rex {}: {message}", self.config.name);
                let pane_id = self.shared.panes.lock().values().next().map(|p| p.pane_id);
                if let Some(pane_id) = pane_id {
                    Mux::get().notify(MuxNotification::Alert {
                        pane_id,
                        alert: Alert::ToastNotification {
                            title: Some(format!("Rex: {}", self.config.name)),
                            body: message,
                            focus: false,
                        },
                    });
                }
            }
            HostEvent::Stream { .. } => {}
        }
    }

    /// Make the wezterm window match the session's view.
    fn show(&self, session_id: &str, view: &ViewResponse) {
        let mux = Mux::get();
        let window_id = match self.state.lock().window_id {
            Some(id) => id,
            None => return,
        };
        let Some(workspace) = mux
            .get_window(window_id)
            .map(|w| w.get_workspace().to_string())
        else {
            // The user closed our window: stop following the session.
            log::info!("rex {}: window closed, detaching", self.config.name);
            let _ = self.detach();
            return;
        };
        let Ok(host) = self.host_sender() else { return };

        // A size to start new tabs at; wezterm fits them to the window.
        let default_size = mux
            .get_active_tab_for_window(window_id)
            .map(|tab| tab.get_size())
            .unwrap_or(TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
                dpi: 0,
            });

        let mut terminals = wezterm_rex::terminal_blocks(view);
        {
            // Blocks we just closed stay hidden until the server catches up.
            let mut closing = self.shared.closing.lock();
            closing.retain(|block, at| terminals.contains(block) && at.elapsed() < CLOSING_GRACE);
            terminals.retain(|block| !closing.contains_key(block));
        }
        let labels: HashMap<String, Option<String>> = view
            .windows
            .iter()
            .flat_map(|w| &w.layers)
            .flat_map(|l| &l.blocks)
            .filter_map(|b| Some((b.block_id.clone()?, b.label.clone())))
            .collect();

        let mut live_blocks = HashSet::new();
        let mut live_tabs = HashSet::new();
        let mut active_tab = None;
        let mut new_tabs = vec![];

        for window in &view.windows {
            let Some(rex_window_id) = window.window_id.clone() else {
                continue;
            };
            let existing = self
                .state
                .lock()
                .tabs
                .get(&rex_window_id)
                .and_then(|id| mux.get_tab(*id));
            let (tab, is_new) = match existing {
                Some(tab) => (tab, false),
                None => (Arc::new(Tab::new(&default_size)), true),
            };
            let size = if is_new { default_size } else { tab.get_size() };

            let pane_ids: HashMap<String, PaneId> = self
                .shared
                .panes
                .lock()
                .iter()
                .map(|(block, pane)| (block.clone(), pane.pane_id))
                .collect();
            let mut builder = TreeBuilder {
                window,
                terminals: &terminals,
                panes: &pane_ids,
                window_id,
                tab_id: tab.tab_id(),
                workspace: workspace.clone(),
                placed: vec![],
            };
            let tree = builder.build(size);
            if matches!(tree, PaneNode::Empty) {
                // Nothing wezterm can show (no terminal blocks).
                continue;
            }

            // Panes for blocks we haven't seen yet.
            let mut by_pane_id: HashMap<PaneId, Arc<dyn Pane>> = HashMap::new();
            for (block, pane_id) in &builder.placed {
                let existing = self.shared.panes.lock().get(block).cloned();
                let pane = match existing {
                    Some(pane) => pane,
                    None => {
                        let pane = Arc::new(RexPane::new(
                            self.id,
                            session_id,
                            block,
                            labels.get(block).cloned().flatten(),
                            size,
                            host.clone(),
                            Arc::downgrade(&self.shared),
                        ));
                        // The tree builder allocated this id for the block;
                        // the pane allocated its own, so map by block.
                        self.shared
                            .panes
                            .lock()
                            .insert(block.clone(), Arc::clone(&pane));
                        let as_pane: Arc<dyn Pane> = pane.clone();
                        let _ = mux.add_pane(&as_pane);
                        let _ = host.send(HostCommand::OpenStream {
                            session_id: session_id.to_string(),
                            block_id: block.clone(),
                        });
                        pane
                    }
                };
                *pane.label.lock() = labels.get(block).cloned().flatten();
                let pane: Arc<dyn Pane> = pane;
                by_pane_id.insert(*pane_id, pane);
                live_blocks.insert(block.clone());
            }

            tab.sync_with_pane_tree(size, tree, |entry| {
                Arc::clone(
                    by_pane_id
                        .get(&entry.pane_id)
                        .expect("every leaf was given a pane"),
                )
            });
            tab.set_title(window.label.as_deref().unwrap_or(""));
            live_tabs.insert(tab.tab_id());

            if view.active_window_id.as_deref() == Some(rex_window_id.as_str()) {
                active_tab = Some(tab.tab_id());
            }
            if is_new {
                mux.add_tab_no_panes(&tab);
                new_tabs.push(Arc::clone(&tab));
            }
            self.state
                .lock()
                .tabs
                .insert(rex_window_id.clone(), tab.tab_id());

            for (block, _) in &builder.placed {
                self.resolve_pane_waiters(block);
            }
            self.resolve_tab_waiters(&rex_window_id, tab.tab_id());
        }

        for tab in new_tabs {
            if let Err(err) = mux.add_tab_to_window(&tab, window_id) {
                log::error!("rex: adding tab: {err:#}");
            }
        }

        // Tabs for Rex windows that are gone. Their panes go with them,
        // without closing anything on the server.
        let stale_tabs: Vec<(String, TabId)> = self
            .state
            .lock()
            .tabs
            .iter()
            .filter(|(_, tab_id)| !live_tabs.contains(tab_id))
            .map(|(w, t)| (w.clone(), *t))
            .collect();
        for (rex_window_id, tab_id) in stale_tabs {
            self.state.lock().tabs.remove(&rex_window_id);
            if let Some(tab) = mux.get_tab(tab_id) {
                for pos in tab.iter_panes_ignoring_zoom() {
                    let Some(pane) = pos.pane.downcast_ref::<RexPane>() else {
                        continue;
                    };
                    if live_blocks.contains(&pane.block_id) {
                        // Moved to another Rex window: take it out of this
                        // tab without killing it.
                        tab.remove_pane(pane.pane_id);
                    } else {
                        pane.server_gone.store(true, Ordering::Relaxed);
                    }
                }
            }
            mux.remove_tab(tab_id);
        }

        // Panes whose blocks left every layout.
        let stale_panes: Vec<Arc<RexPane>> = {
            let mut panes = self.shared.panes.lock();
            let gone: Vec<String> = panes
                .keys()
                .filter(|b| !live_blocks.contains(*b))
                .cloned()
                .collect();
            gone.into_iter().filter_map(|b| panes.remove(&b)).collect()
        };
        for pane in stale_panes {
            pane.server_gone.store(true, Ordering::Relaxed);
            pane.dead.store(true, Ordering::Relaxed);
            let _ = host.send(HostCommand::CloseStream {
                block_id: pane.block_id.clone(),
            });
            mux.remove_pane(pane.pane_id);
        }

        if let Some(tab_id) = active_tab {
            if let Some(mut window) = mux.get_window_mut(window_id) {
                if let Some(idx) = window.get_tab_idx_for_id(tab_id) {
                    window.set_active_tab_idx_without_saving(idx);
                }
            }
        }
        if let Some(label) = &view.label {
            if let Some(mut window) = mux.get_window_mut(window_id) {
                window.set_title(label);
            }
        }
        mux.notify(MuxNotification::WindowInvalidated(window_id));

        let mut state = self.state.lock();
        for waiter in state.attach_waiters.drain(..) {
            let _ = waiter.send(Ok(()));
        }
    }

    fn resolve_tab_waiters(&self, rex_window_id: &str, tab_id: TabId) {
        if let Some(waiters) = self.state.lock().tab_waiters.remove(rex_window_id) {
            for waiter in waiters {
                let _ = waiter.send(tab_id);
            }
        }
    }

    fn resolve_pane_waiters(&self, block_id: &str) {
        let pane_id = match self.shared.panes.lock().get(block_id) {
            Some(pane) => pane.pane_id,
            None => return,
        };
        if let Some(waiters) = self.state.lock().pane_waiters.remove(block_id) {
            for waiter in waiters {
                let _ = waiter.send(pane_id);
            }
        }
    }

    /// Remove our tabs without touching the server.
    fn clear_tabs(&self) {
        let mux = Mux::get();
        let tabs: Vec<TabId> = self.state.lock().tabs.drain().map(|(_, t)| t).collect();
        let panes: Vec<Arc<RexPane>> = self.shared.panes.lock().drain().map(|(_, p)| p).collect();
        for pane in &panes {
            pane.keep_on_server.store(true, Ordering::Relaxed);
        }
        for tab_id in tabs {
            mux.remove_tab(tab_id);
        }
        for pane in panes {
            pane.dead.store(true, Ordering::Relaxed);
            mux.remove_pane(pane.pane_id);
        }
    }

    fn selected_session(&self) -> anyhow::Result<String> {
        self.state
            .lock()
            .selected
            .clone()
            .ok_or_else(|| anyhow!("Rex domain {} shows no session", self.config.name))
    }

    async fn call(&self, method: &str, payload: Value) -> anyhow::Result<Value> {
        let (reply, rx) = flume::bounded(1);
        let reply: Reply = reply;
        self.host_sender()?
            .send(HostCommand::Request {
                method: method.into(),
                payload,
                reply: Some(reply),
            })
            .map_err(|_| anyhow!("Rex host stopped"))?;
        with_timeout("answer", rx)
            .await?
            .map_err(|e| anyhow!("{e}"))
    }

    /// The pane for `block_id`, once a view shows it.
    async fn pane_for_block(&self, block_id: &str) -> anyhow::Result<Arc<dyn Pane>> {
        let ready = self.shared.panes.lock().get(block_id).map(|p| p.pane_id);
        let pane_id = match ready {
            Some(id) => id,
            None => {
                let (tx, rx) = flume::bounded(1);
                self.state
                    .lock()
                    .pane_waiters
                    .entry(block_id.into())
                    .or_default()
                    .push(tx);
                with_timeout("show the new pane", rx).await?
            }
        };
        Mux::get()
            .get_pane(pane_id)
            .ok_or_else(|| anyhow!("Rex pane {pane_id} went away"))
    }
}

#[async_trait(?Send)]
impl Domain for RexDomain {
    async fn spawn(
        &self,
        _size: TerminalSize,
        command: Option<CommandBuilder>,
        _command_dir: Option<String>,
        window: WindowId,
    ) -> anyhow::Result<Arc<Tab>> {
        let ours = self.state.lock().window_id;
        if ours != Some(window) {
            bail!(
                "Rex domain {} shows its session in one window; open new tabs there",
                self.config.name
            );
        }
        if command.is_some() {
            log::warn!("rex: new tabs run the server's login shell; the command is ignored");
        }
        let session_id = self.selected_session()?;
        let reply = self
            .call(
                "session.new_window",
                json!({"session_id": session_id, "layout": wezterm_rex::host::shell_layout(), "focus": true}),
            )
            .await?;
        let rex_window_id = reply["window_id"]
            .as_str()
            .context("Rex didn't name the new window")?
            .to_string();
        let ready = self.state.lock().tabs.get(&rex_window_id).copied();
        let tab_id = match ready {
            Some(id) => id,
            None => {
                let (tx, rx) = flume::bounded(1);
                self.state
                    .lock()
                    .tab_waiters
                    .entry(rex_window_id)
                    .or_default()
                    .push(tx);
                with_timeout("show the new tab", rx).await?
            }
        };
        Mux::get()
            .get_tab(tab_id)
            .ok_or_else(|| anyhow!("Rex tab {tab_id} went away"))
    }

    async fn split_pane(
        &self,
        source: SplitSource,
        _tab: TabId,
        pane_id: PaneId,
        request: SplitRequest,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        if let SplitSource::MovePane(_) = source {
            bail!("panes can't be moved into a Rex session");
        }
        let anchor = self
            .shared
            .panes
            .lock()
            .values()
            .find(|p| p.pane_id == pane_id)
            .map(|p| (p.session_id.clone(), p.block_id.clone()))
            .ok_or_else(|| anyhow!("pane {pane_id} is not a Rex pane"))?;
        let after = request.target_is_second;
        let mut payload = json!({
            "session_id": anchor.0,
            "anchor_id": anchor.1,
            "direction": match request.direction {
                SplitDirection::Horizontal => "horizontal",
                SplitDirection::Vertical => "vertical",
            },
            "side": if after { "after" } else { "before" },
            "layout": wezterm_rex::host::shell_layout(),
            "focus": true,
        });
        if let SplitSize::Percent(new_share) = request.size {
            // Rex's ratio is the share of the side before the divider.
            let new_share = (new_share as f64 / 100.0).clamp(0.05, 0.95);
            payload["ratio"] = json!(if after { 1.0 - new_share } else { new_share });
        }
        let reply = self.call("session.new_split", payload).await?;
        let block_id = reply["block_ids"]
            .as_array()
            .and_then(|ids| ids.iter().find_map(Value::as_str))
            .context("Rex didn't name the new block")?
            .to_string();
        self.pane_for_block(&block_id).await
    }

    async fn spawn_pane(
        &self,
        _size: TerminalSize,
        _command: Option<CommandBuilder>,
        _command_dir: Option<String>,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        bail!("Rex panes are created on the server; use a new tab or a split")
    }

    async fn move_pane_to_new_tab(
        &self,
        _pane_id: PaneId,
        _window_id: Option<WindowId>,
        _workspace_for_new_window: Option<String>,
    ) -> anyhow::Result<Option<(Arc<Tab>, WindowId)>> {
        bail!("Rex layouts are kept by the server; move panes with Rex")
    }

    fn detachable(&self) -> bool {
        true
    }

    fn domain_id(&self) -> DomainId {
        self.id
    }

    fn domain_name(&self) -> &str {
        &self.config.name
    }

    async fn domain_label(&self) -> String {
        let state = self.state.lock();
        let session = state
            .selected
            .as_ref()
            .and_then(|id| state.sessions.iter().find(|s| &s.session_id == id))
            .map(|s| s.label.clone());
        match session {
            Some(label) => format!("Rex {}: {label}", self.config.name),
            None => format!("Rex {}", self.config.name),
        }
    }

    async fn attach(&self, window_id: Option<WindowId>) -> anyhow::Result<()> {
        let mux = Mux::get();
        if self.state.lock().attaching {
            // Another attach is under way: share its outcome rather than
            // reporting an empty domain (which would make wezterm spawn).
            let (tx, rx) = flume::bounded(1);
            self.state.lock().attach_waiters.push(tx);
            return with_timeout("attach", rx).await?;
        }
        if self.host.lock().is_some() {
            let window = self.state.lock().window_id;
            if window.map_or(false, |id| mux.get_window(id).is_some()) {
                return Ok(());
            }
            // Our window is gone; start over.
            self.detach()?;
        }
        // Keep a fresh window alive (and unannounced) until it has tabs.
        let mut builder = None;
        let created = window_id.is_none();
        let window_id = match window_id {
            Some(id) => id,
            None => {
                let b = mux.new_empty_window(None, None);
                let id = *b;
                builder = Some(b);
                id
            }
        };
        let (tx, rx) = flume::bounded(1);
        {
            let mut state = self.state.lock();
            state.window_id = Some(window_id);
            state.attaching = true;
            state.attach_waiters.push(tx);
        }
        if let Err(err) = self.start_host() {
            let mut state = self.state.lock();
            state.window_id = None;
            state.attaching = false;
            for waiter in state.attach_waiters.drain(..) {
                let _ = waiter.send(Err(anyhow!("{err:#}")));
            }
            drop(state);
            drop(builder);
            if created {
                mux.kill_window(window_id);
            }
            return Err(err);
        }
        let result = match with_timeout("attach", rx).await {
            Ok(result) => result,
            Err(err) => Err(err),
        };
        self.state.lock().attaching = false;
        // Attaching never changes the server. With no terminal to show,
        // wezterm would otherwise ask us to spawn one.
        let result = result.and_then(|()| {
            if self.state.lock().tabs.is_empty() {
                Err(anyhow!(
                    "the Rex session has no terminal windows; open one with `rex` first"
                ))
            } else {
                Ok(())
            }
        });
        if let Err(err) = &result {
            log::error!("rex {}: attach failed: {err:#}", self.config.name);
            let _ = self.detach();
        }
        drop(builder);
        if result.is_err() && created {
            mux.kill_window(window_id);
        }
        result.with_context(|| format!("attaching Rex domain {}", self.config.name))
    }

    fn detach(&self) -> anyhow::Result<()> {
        // Ignore anything the old host still reports.
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
        self.shared.connected.store(false, Ordering::SeqCst);
        self.clear_tabs();
        self.shared.closing.lock().clear();
        // Dropping the host closes the control and data connections. The
        // server keeps everything running.
        self.host.lock().take();
        let mut state = self.state.lock();
        state.window_id = None;
        state.views.clear();
        state.selected = None;
        state.connected = false;
        for waiter in state.attach_waiters.drain(..) {
            let _ = waiter.send(Err(anyhow!("detached")));
        }
        state.tab_waiters.clear();
        state.pane_waiters.clear();
        Ok(())
    }

    fn state(&self) -> DomainState {
        if self.host.lock().is_some() {
            DomainState::Attached
        } else {
            DomainState::Detached
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn size(cols: usize, rows: usize) -> TerminalSize {
        TerminalSize {
            cols,
            rows,
            pixel_width: cols * 10,
            pixel_height: rows * 20,
            dpi: 96,
        }
    }

    #[test]
    fn split_sizes_leave_a_divider() {
        let (a, b) = split_sizes(size(81, 24), SplitDirection::Horizontal, 0.5);
        assert_eq!((a.cols, b.cols), (40, 40));
        assert_eq!(a.rows, 24);
        assert_eq!(a.pixel_width, 400);
        let (a, b) = split_sizes(size(80, 25), SplitDirection::Vertical, 0.25);
        assert_eq!(a.rows + b.rows, 24);
        assert_eq!(a.rows, 6);
        let (a, b) = split_sizes(size(80, 24), SplitDirection::Horizontal, f64::NAN);
        assert!(a.cols >= 1 && b.cols >= 1);
        let (a, b) = split_sizes(size(2, 24), SplitDirection::Horizontal, 0.9);
        assert!(a.cols >= 1 && b.cols >= 1);
    }
}
