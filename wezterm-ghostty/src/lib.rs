//! A terminal model for wezterm backed by [libghostty-vt].
//!
//! `Terminal` owns a libghostty-vt terminal and presents the parts of the
//! `wezterm_term::Terminal` API that wezterm's mux layer needs: feeding
//! program output, encoding input (keys, mouse, focus, paste), and reading
//! the screen back as wezterm `Line`s addressed by `StableRowIndex`.
//!
//! libghostty-vt does all of the parsing and state keeping. This crate only
//! translates: rows are converted into `Line`s lazily, when wezterm asks for
//! them, and cached until libghostty reports that they changed.
//!
//! [libghostty-vt]: https://lib.ghostty.org/vt/
use ghostty_vt_sys::*;
use std::cell::UnsafeCell;
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::ops::Range;
use std::sync::Arc;
use termwiz::input::KeyboardEncoding;
use termwiz::surface::{CursorShape, CursorVisibility, SequenceNo};
use url::Url;
use wezterm_term::color::ColorPalette;
use wezterm_term::{
    Alert, AlertHandler, Clipboard, ClipboardSelection, CursorPosition, DeviceControlHandler,
    DownloadHandler, KeyCode, KeyModifiers, Line, MouseButton, MouseEvent, MouseEventKind,
    Progress, SemanticZone, StableRowIndex, TerminalConfiguration, TerminalSize,
    VisibleRowIndex,
};

mod convert;
pub mod keys;
mod sniff;

use convert::HyperlinkCache;

const PRIMARY: usize = 0;
const ALTERNATE: usize = 1;

/// Dimensions of the terminal as wezterm's renderer expects them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dimensions {
    pub cols: usize,
    pub viewport_rows: usize,
    /// Total number of rows, scrollback plus viewport
    pub scrollback_rows: usize,
    pub physical_top: StableRowIndex,
    pub scrollback_top: StableRowIndex,
    pub dpi: u32,
    pub pixel_width: usize,
    pub pixel_height: usize,
    pub reverse_video: bool,
}

/// State that libghostty callbacks write to while `vt_write` runs.
///
/// It lives in its own heap allocation that is only accessed through a
/// raw pointer while libghostty may call back into us.
struct Callbacks {
    writer: Box<dyn Write + Send>,
    alerts: Vec<Alert>,
    title_changed: bool,
    pwd_changed: bool,
    progress: Option<Progress>,
    clipboard: Option<Arc<dyn Clipboard>>,
    size: TerminalSize,
    enq_answerback: String,
    xtversion: String,
}

/// Bookkeeping for one of the two screens (primary/alternate).
struct ScreenState {
    /// The stable row index of screen row 0 (the top of the scrollback).
    stable_offset: StableRowIndex,
    /// Tracks the bottom row of the active area; used to measure how many
    /// rows were pruned from the top of the scrollback between syncs.
    anchor: GhosttyTrackedGridRef,
    anchor_screen_y: u64,
    /// Number of history (scrollback) rows at the last sync
    history: usize,
    /// Lazily converted lines, by stable row
    lines: BTreeMap<StableRowIndex, Line>,
    /// Per-row change seqnos from partial (dirty row) updates
    row_seqno: BTreeMap<StableRowIndex, SequenceNo>,
    /// Ranges `[start, ∞)` that changed at the given seqno; both
    /// components are strictly increasing.
    suffix_marks: Vec<(StableRowIndex, SequenceNo)>,
    /// Every row is at least this new
    bulk_seqno: SequenceNo,
}

impl ScreenState {
    fn new() -> Self {
        Self {
            stable_offset: 0,
            anchor: std::ptr::null_mut(),
            anchor_screen_y: 0,
            history: 0,
            lines: BTreeMap::new(),
            row_seqno: BTreeMap::new(),
            suffix_marks: vec![],
            bulk_seqno: 0,
        }
    }

    fn seqno_of(&self, row: StableRowIndex) -> SequenceNo {
        let mut seqno = self.bulk_seqno;
        let idx = self.suffix_marks.partition_point(|(start, _)| *start <= row);
        if idx > 0 {
            seqno = seqno.max(self.suffix_marks[idx - 1].1);
        }
        if let Some(s) = self.row_seqno.get(&row) {
            seqno = seqno.max(*s);
        }
        seqno
    }

    fn mark_all(&mut self, seqno: SequenceNo) {
        self.bulk_seqno = seqno;
        self.suffix_marks.clear();
        self.row_seqno.clear();
        self.lines.clear();
    }

    fn mark_from(&mut self, start: StableRowIndex, seqno: SequenceNo) {
        while let Some((s, _)) = self.suffix_marks.last() {
            if *s >= start {
                self.suffix_marks.pop();
            } else {
                break;
            }
        }
        self.suffix_marks.push((start, seqno));
        let _ = self.row_seqno.split_off(&start);
        let _ = self.lines.split_off(&start);
    }

    fn mark_row(&mut self, row: StableRowIndex, seqno: SequenceNo) {
        self.row_seqno.insert(row, seqno);
        self.lines.remove(&row);
    }

    /// Discard bookkeeping for rows that no longer exist
    fn prune_below(&mut self, top: StableRowIndex) {
        self.lines = self.lines.split_off(&top);
        self.row_seqno = self.row_seqno.split_off(&top);
        let idx = self.suffix_marks.partition_point(|(start, _)| *start <= top);
        if idx > 1 {
            // Marks wholly below `top` are subsumed by the last one at or
            // below it, which still applies to every remaining row.
            let (_, seqno) = self.suffix_marks[idx - 1];
            self.suffix_marks.drain(0..idx);
            self.bulk_seqno = self.bulk_seqno.max(seqno);
        } else if idx == 1 {
            let (_, seqno) = self.suffix_marks.remove(0);
            self.bulk_seqno = self.bulk_seqno.max(seqno);
        }
    }

    fn free_anchor(&mut self) {
        if !self.anchor.is_null() {
            unsafe { ghostty_tracked_grid_ref_free(self.anchor) };
            self.anchor = std::ptr::null_mut();
        }
    }
}

pub struct Terminal {
    raw: GhosttyTerminal,
    render: GhosttyRenderState,
    row_iter: GhosttyRenderStateRowIterator,
    key_encoder: GhosttyKeyEncoder,
    key_event: GhosttyKeyEvent,
    mouse_encoder: GhosttyMouseEncoder,
    mouse_event: GhosttyMouseEvent,
    cb: Box<UnsafeCell<Callbacks>>,

    config: Arc<dyn TerminalConfiguration>,
    config_generation: Option<usize>,

    seqno: SequenceNo,
    size: TerminalSize,
    screens: [ScreenState; 2],
    active: usize,
    links: HyperlinkCache,

    title: String,
    current_dir: Option<Url>,
    user_vars: HashMap<String, String>,
    progress: Progress,
    palette: Option<ColorPalette>,
    colors_snapshot: Vec<u8>,
    cursor_shape: CursorShape,

    alert_handler: Option<Box<dyn AlertHandler>>,
    device_control_handler: Option<Box<dyn DeviceControlHandler>>,
    download_handler: Option<Arc<dyn DownloadHandler>>,

    focused: bool,
    lost_focus_seqno: SequenceNo,
    lost_focus_alerted_seqno: SequenceNo,
    current_mouse_buttons: Vec<MouseButton>,
    sniffer: sniff::OscSniffer,
}

// All of the raw libghostty handles are owned exclusively by this struct
// and libghostty-vt has no thread affinity, so it is safe to move a
// Terminal between threads.  Concurrent access is prevented by `&mut`.
unsafe impl Send for Terminal {}

fn gstr(s: &str) -> GhosttyString {
    GhosttyString {
        ptr: s.as_ptr(),
        len: s.len(),
    }
}

unsafe fn gstr_to_string(s: GhosttyString) -> String {
    if s.ptr.is_null() || s.len == 0 {
        return String::new();
    }
    String::from_utf8_lossy(std::slice::from_raw_parts(s.ptr, s.len)).into_owned()
}

fn rgb(t: termwiz::color::SrgbaTuple) -> GhosttyColorRgb {
    let (r, g, b, _) = t.to_srgb_u8();
    GhosttyColorRgb { r, g, b }
}

fn srgba(c: GhosttyColorRgb) -> termwiz::color::SrgbaTuple {
    termwiz::color::SrgbaTuple::from((c.r, c.g, c.b))
}

// ---------------------------------------------------------------------
// libghostty callbacks.  `userdata` is always a `*mut Callbacks`.

unsafe fn cbs<'a>(userdata: *mut std::ffi::c_void) -> &'a mut Callbacks {
    &mut *(userdata as *mut Callbacks)
}

unsafe extern "C" fn cb_write_pty(
    _t: GhosttyTerminal,
    ud: *mut std::ffi::c_void,
    data: *const u8,
    len: usize,
) {
    let cb = cbs(ud);
    let bytes = std::slice::from_raw_parts(data, len);
    if let Err(err) = cb.writer.write_all(bytes).and_then(|_| cb.writer.flush()) {
        log::error!("ghostty: error writing reply to pty: {err:#}");
    }
}

unsafe extern "C" fn cb_bell(_t: GhosttyTerminal, ud: *mut std::ffi::c_void) {
    cbs(ud).alerts.push(Alert::Bell);
}

unsafe extern "C" fn cb_title_changed(_t: GhosttyTerminal, ud: *mut std::ffi::c_void) {
    cbs(ud).title_changed = true;
}

unsafe extern "C" fn cb_pwd_changed(_t: GhosttyTerminal, ud: *mut std::ffi::c_void) {
    cbs(ud).pwd_changed = true;
}

unsafe extern "C" fn cb_enquiry(_t: GhosttyTerminal, ud: *mut std::ffi::c_void) -> GhosttyString {
    gstr(&cbs(ud).enq_answerback)
}

unsafe extern "C" fn cb_xtversion(
    _t: GhosttyTerminal,
    ud: *mut std::ffi::c_void,
) -> GhosttyString {
    gstr(&cbs(ud).xtversion)
}

unsafe extern "C" fn cb_size(
    _t: GhosttyTerminal,
    ud: *mut std::ffi::c_void,
    out: *mut GhosttySizeReportSize,
) -> bool {
    let size = cbs(ud).size;
    let cols = size.cols.max(1);
    let rows = size.rows.max(1);
    (*out).rows = rows as u16;
    (*out).columns = cols as u16;
    (*out).cell_width = (size.pixel_width / cols) as u32;
    (*out).cell_height = (size.pixel_height / rows) as u32;
    true
}

unsafe extern "C" fn cb_notification(
    _t: GhosttyTerminal,
    ud: *mut std::ffi::c_void,
    n: *const GhosttyTerminalDesktopNotification,
) {
    let title = gstr_to_string((*n).title);
    let body = gstr_to_string((*n).body);
    cbs(ud).alerts.push(Alert::ToastNotification {
        title: if title.is_empty() { None } else { Some(title) },
        body,
        focus: true,
    });
}

unsafe extern "C" fn cb_progress(
    _t: GhosttyTerminal,
    ud: *mut std::ffi::c_void,
    report: *const GhosttyTerminalProgressReport,
) {
    let pct = (*report).progress.clamp(0, 100) as u8;
    let progress = match (*report).state {
        GHOSTTY_TERMINAL_PROGRESS_STATE_SET => Progress::Percentage(pct),
        GHOSTTY_TERMINAL_PROGRESS_STATE_ERROR => Progress::Error(pct),
        GHOSTTY_TERMINAL_PROGRESS_STATE_INDETERMINATE => Progress::Indeterminate,
        GHOSTTY_TERMINAL_PROGRESS_STATE_PAUSE => Progress::Percentage(pct),
        _ => Progress::None,
    };
    cbs(ud).progress = Some(progress);
}

unsafe extern "C" fn cb_clipboard_write(
    _t: GhosttyTerminal,
    ud: *mut std::ffi::c_void,
    write: *const GhosttyClipboardWrite,
) {
    let cb = cbs(ud);
    let w = &*write;
    let mut result = GHOSTTY_CLIPBOARD_WRITE_RESULT_UNSUPPORTED;
    if let Some(clip) = cb.clipboard.as_ref() {
        let selection = match w.location {
            GHOSTTY_CLIPBOARD_LOCATION_STANDARD => ClipboardSelection::Clipboard,
            _ => ClipboardSelection::PrimarySelection,
        };
        let contents = if w.contents_len == 0 || w.contents.is_null() {
            &[][..]
        } else {
            std::slice::from_raw_parts(w.contents, w.contents_len)
        };
        let text = if contents.is_empty() {
            None
        } else {
            let pick = contents
                .iter()
                .find(|c| gstr_to_string(c.mime).starts_with("text/plain"))
                .unwrap_or(&contents[0]);
            Some(gstr_to_string(pick.data))
        };
        result = match clip.set_contents(selection, text) {
            Ok(_) => GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS,
            Err(err) => {
                log::error!("ghostty: failed to set clipboard: {err:#}");
                GHOSTTY_CLIPBOARD_WRITE_RESULT_IO_ERROR
            }
        };
    }
    if let Some(reply) = w.reply {
        let mut r: GhosttyClipboardWriteReply = std::mem::zeroed();
        r.size = std::mem::size_of::<GhosttyClipboardWriteReply>();
        r.result = result;
        reply(write, &r);
    }
}

unsafe extern "C" fn cb_unknown(
    _t: GhosttyTerminal,
    ud: *mut std::ffi::c_void,
    seq: *const GhosttyTerminalUnknownSequence,
) {
    if (*seq).tag != GHOSTTY_TERMINAL_UNKNOWN_SEQUENCE_OSC {
        return;
    }
    let osc = (*seq).value.osc;
    if osc.truncated {
        return;
    }
    let content = gstr_to_string(osc.content);
    let _ = ud;
    log::debug!("ghostty: unhandled OSC {content:?}");
}

// ---------------------------------------------------------------------

impl Terminal {
    pub fn new(
        size: TerminalSize,
        config: Arc<dyn TerminalConfiguration>,
        term_program: &str,
        term_version: &str,
        writer: Box<dyn Write + Send>,
    ) -> Self {
        let null = std::ptr::null();
        let mut raw: GhosttyTerminal = std::ptr::null_mut();
        let mut render: GhosttyRenderState = std::ptr::null_mut();
        let mut row_iter: GhosttyRenderStateRowIterator = std::ptr::null_mut();
        let mut key_encoder: GhosttyKeyEncoder = std::ptr::null_mut();
        let mut key_event: GhosttyKeyEvent = std::ptr::null_mut();
        let mut mouse_encoder: GhosttyMouseEncoder = std::ptr::null_mut();
        let mut mouse_event: GhosttyMouseEvent = std::ptr::null_mut();
        unsafe {
            assert_eq!(
                ghostty_terminal_new(
                    null,
                    &mut raw,
                    size.cols.clamp(1, u16::MAX as usize) as u16,
                    size.rows.clamp(1, u16::MAX as usize) as u16,
                ),
                GHOSTTY_SUCCESS,
                "ghostty_terminal_new failed"
            );
            assert_eq!(ghostty_render_state_new(null, &mut render), GHOSTTY_SUCCESS);
            assert_eq!(
                ghostty_render_state_row_iterator_new(null, &mut row_iter),
                GHOSTTY_SUCCESS
            );
            assert_eq!(ghostty_key_encoder_new(null, &mut key_encoder), GHOSTTY_SUCCESS);
            assert_eq!(ghostty_key_event_new(null, &mut key_event), GHOSTTY_SUCCESS);
            assert_eq!(
                ghostty_mouse_encoder_new(null, &mut mouse_encoder),
                GHOSTTY_SUCCESS
            );
            assert_eq!(ghostty_mouse_event_new(null, &mut mouse_event), GHOSTTY_SUCCESS);
        }

        let cb = Box::new(UnsafeCell::new(Callbacks {
            writer,
            alerts: vec![],
            title_changed: false,
            pwd_changed: false,
            progress: None,
            clipboard: None,
            size,
            enq_answerback: String::new(),
            xtversion: format!("{term_program} {term_version}"),
        }));

        let mut term = Self {
            raw,
            render,
            row_iter,
            key_encoder,
            key_event,
            mouse_encoder,
            mouse_event,
            cb,
            config,
            config_generation: None,
            seqno: 1,
            size,
            screens: [ScreenState::new(), ScreenState::new()],
            active: PRIMARY,
            links: HyperlinkCache::default(),
            title: "wezterm".to_string(),
            current_dir: None,
            user_vars: HashMap::new(),
            progress: Progress::None,
            palette: None,
            colors_snapshot: vec![],
            cursor_shape: CursorShape::Default,
            alert_handler: None,
            device_control_handler: None,
            download_handler: None,
            focused: true,
            lost_focus_seqno: 0,
            lost_focus_alerted_seqno: 0,
            current_mouse_buttons: vec![],
            sniffer: sniff::OscSniffer::default(),
        };
        term.install_callbacks();
        term.apply_config();
        term.resize_ghostty();
        term.screens[PRIMARY].mark_all(term.seqno);
        term.sync();
        term
    }

    fn install_callbacks(&mut self) {
        let ud = self.cb.get() as *mut std::ffi::c_void;
        unsafe {
            let t = self.raw;
            ghostty_terminal_set(t, GHOSTTY_TERMINAL_OPT_USERDATA, ud);
            macro_rules! set_cb {
                ($opt:expr, $func:expr) => {
                    ghostty_terminal_set(t, $opt, $func as *const std::ffi::c_void);
                };
            }
            set_cb!(GHOSTTY_TERMINAL_OPT_WRITE_PTY, cb_write_pty);
            set_cb!(GHOSTTY_TERMINAL_OPT_BELL, cb_bell);
            set_cb!(GHOSTTY_TERMINAL_OPT_TITLE_CHANGED, cb_title_changed);
            set_cb!(GHOSTTY_TERMINAL_OPT_PWD_CHANGED, cb_pwd_changed);
            set_cb!(GHOSTTY_TERMINAL_OPT_ENQUIRY, cb_enquiry);
            set_cb!(GHOSTTY_TERMINAL_OPT_XTVERSION, cb_xtversion);
            set_cb!(GHOSTTY_TERMINAL_OPT_SIZE, cb_size);
            set_cb!(GHOSTTY_TERMINAL_OPT_DESKTOP_NOTIFICATION, cb_notification);
            set_cb!(GHOSTTY_TERMINAL_OPT_PROGRESS_REPORT, cb_progress);
            set_cb!(GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE, cb_clipboard_write);
            set_cb!(GHOSTTY_TERMINAL_OPT_UNKNOWN_SEQUENCE, cb_unknown);
            if log::log_enabled!(log::Level::Debug) {
                let max: usize = 4096;
                ghostty_terminal_set(
                    t,
                    GHOSTTY_TERMINAL_OPT_UNKNOWN_MAX_BYTES,
                    &max as *const usize as *const std::ffi::c_void,
                );
            }
        }
    }

    #[allow(clippy::mut_from_ref)]
    fn cb(&self) -> &mut Callbacks {
        // Only called when libghostty is not executing (so no callback can
        // be holding a reference), and the result is never held across a
        // libghostty call.
        unsafe { &mut *self.cb.get() }
    }

    fn get<T>(&self, what: GhosttyTerminalData, default: T) -> T {
        let mut v = default;
        unsafe {
            ghostty_terminal_get(self.raw, what, &mut v as *mut T as *mut std::ffi::c_void);
        }
        v
    }

    fn mode(&self, value: u16, ansi: bool) -> bool {
        let mut cfg = GhosttyTerminalModeConfig {
            mode: ghostty_mode_new(value, ansi),
            value: false,
        };
        unsafe {
            ghostty_terminal_get(
                self.raw,
                GHOSTTY_TERMINAL_DATA_MODE,
                &mut cfg as *mut _ as *mut std::ffi::c_void,
            );
        }
        cfg.value
    }

    fn apply_config(&mut self) {
        let generation = self.config.generation();
        if self.config_generation == Some(generation) {
            return;
        }
        self.config_generation = Some(generation);

        let palette = self.config.color_palette();
        let mut colors = [GhosttyColorRgb { r: 0, g: 0, b: 0 }; 256];
        for (i, c) in palette.colors.0.iter().enumerate() {
            colors[i] = rgb(*c);
        }
        let fg = rgb(palette.foreground);
        let bg = rgb(palette.background);
        let cursor = rgb(palette.cursor_bg);
        let lines: usize = self.config.scrollback_size();
        let unlimited_bytes: usize = usize::MAX;
        unsafe {
            let t = self.raw;
            ghostty_terminal_set(
                t,
                GHOSTTY_TERMINAL_OPT_COLOR_PALETTE,
                &colors as *const _ as *const std::ffi::c_void,
            );
            ghostty_terminal_set(t, GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND, &fg as *const _ as _);
            ghostty_terminal_set(t, GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND, &bg as *const _ as _);
            ghostty_terminal_set(t, GHOSTTY_TERMINAL_OPT_COLOR_CURSOR, &cursor as *const _ as _);
            ghostty_terminal_set(
                t,
                GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES,
                &unlimited_bytes as *const _ as _,
            );
            ghostty_terminal_set(
                t,
                GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES,
                &lines as *const _ as _,
            );
            let title_report = self.config.enable_title_reporting();
            ghostty_terminal_set(
                t,
                GHOSTTY_TERMINAL_OPT_TITLE_REPORT,
                &title_report as *const _ as _,
            );
        }
        self.cb().enq_answerback = self.config.enq_answerback();
        // Re-evaluate any palette overrides relative to the new defaults
        self.colors_snapshot.clear();
    }

    fn resize_ghostty(&mut self) {
        let cols = self.size.cols.clamp(1, u16::MAX as usize);
        let rows = self.size.rows.clamp(1, u16::MAX as usize);
        let cell_w = (self.size.pixel_width / cols) as u32;
        let cell_h = (self.size.pixel_height / rows) as u32;
        unsafe {
            ghostty_terminal_resize(self.raw, cols as u16, rows as u16, cell_w, cell_h);
        }
        self.cb().size = self.size;
    }

    fn rows(&self) -> usize {
        self.get::<u16>(GHOSTTY_TERMINAL_DATA_ROWS, 0) as usize
    }

    fn cols(&self) -> u16 {
        self.get::<u16>(GHOSTTY_TERMINAL_DATA_COLS, 0)
    }

    fn history_rows(&self) -> usize {
        self.get::<usize>(GHOSTTY_TERMINAL_DATA_SCROLLBACK_ROWS, 0)
    }

    fn active_screen(&self) -> usize {
        let s: GhosttyTerminalScreen =
            self.get(GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, GHOSTTY_TERMINAL_SCREEN_PRIMARY);
        if s == GHOSTTY_TERMINAL_SCREEN_ALTERNATE {
            ALTERNATE
        } else {
            PRIMARY
        }
    }

    /// Point the active screen's anchor at the top row of the active area.
    /// (Not the bottom row: a tracked pin keeps libghostty from trimming
    /// trailing blank rows when the screen shrinks.)
    fn reanchor(&mut self) {
        let history = self.history_rows();
        let point = GhosttyPoint {
            tag: GHOSTTY_POINT_TAG_ACTIVE,
            value: GhosttyPointValue {
                coordinate: GhosttyPointCoordinate { x: 0, y: 0 },
            },
        };
        let screen = &mut self.screens[self.active];
        unsafe {
            if screen.anchor.is_null() {
                ghostty_terminal_grid_ref_track(self.raw, point, &mut screen.anchor);
            } else {
                ghostty_tracked_grid_ref_set(screen.anchor, self.raw, point);
            }
        }
        screen.anchor_screen_y = history as u64;
        screen.history = history;
    }

    /// Reconcile our bookkeeping with libghostty after it was mutated:
    /// track scrolling/pruning, record which rows changed and dispatch any
    /// alerts that were raised by callbacks.
    fn sync(&mut self) {
        self.seqno += 1;
        let seqno = self.seqno;

        let active = self.active_screen();
        let switched = active != self.active;
        if switched {
            self.active = active;
            // The other screen's anchor belongs to the screen that was
            // active when it was created; we just need a fresh one here.
            self.screens[active].free_anchor();
            self.screens[active].mark_all(seqno);
        }

        let rows = self.rows();
        let history = self.history_rows();

        // How many rows fell off the top of the scrollback?
        let (pruned, anchor_lost) = {
            let screen = &self.screens[active];
            if screen.anchor.is_null() || switched {
                (0i64, true)
            } else {
                let mut pt = GhosttyPointCoordinate { x: 0, y: 0 };
                let ok = unsafe {
                    ghostty_tracked_grid_ref_has_value(screen.anchor)
                        && ghostty_tracked_grid_ref_point(
                            screen.anchor,
                            GHOSTTY_POINT_TAG_SCREEN,
                            &mut pt,
                        ) == GHOSTTY_SUCCESS
                };
                if ok {
                    ((screen.anchor_screen_y as i64 - pt.y as i64).max(0), false)
                } else {
                    // Everything we knew about scrolled away
                    ((screen.history + rows) as i64, true)
                }
            }
        };

        let screen = &mut self.screens[active];
        let old_active_top = screen.stable_offset + screen.history as StableRowIndex;
        let scrolled = (history as i64 - screen.history as i64) + pruned;
        screen.stable_offset += pruned as StableRowIndex;
        let new_active_top = screen.stable_offset + history as StableRowIndex;

        unsafe {
            ghostty_render_state_update(self.render, self.raw);
        }
        let mut dirty: GhosttyRenderStateDirty = GHOSTTY_RENDER_STATE_DIRTY_FALSE;
        unsafe {
            ghostty_render_state_get(
                self.render,
                GHOSTTY_RENDER_STATE_DATA_DIRTY,
                &mut dirty as *mut _ as *mut std::ffi::c_void,
            );
        }

        if anchor_lost && !switched {
            screen.mark_all(seqno);
        } else if scrolled != 0 || dirty == GHOSTTY_RENDER_STATE_DIRTY_FULL {
            screen.mark_from(old_active_top.min(new_active_top), seqno);
        } else if dirty == GHOSTTY_RENDER_STATE_DIRTY_PARTIAL {
            unsafe {
                ghostty_render_state_get(
                    self.render,
                    GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
                    &mut self.row_iter as *mut _ as *mut std::ffi::c_void,
                );
                let mut y: StableRowIndex = 0;
                while ghostty_render_state_row_iterator_next(self.row_iter) {
                    let mut row_dirty = false;
                    ghostty_render_state_row_get(
                        self.row_iter,
                        GHOSTTY_RENDER_STATE_ROW_DATA_DIRTY,
                        &mut row_dirty as *mut _ as *mut std::ffi::c_void,
                    );
                    if row_dirty {
                        screen.mark_row(new_active_top + y, seqno);
                        let clean = false;
                        ghostty_render_state_row_set(
                            self.row_iter,
                            GHOSTTY_RENDER_STATE_ROW_OPT_DIRTY,
                            &clean as *const _ as *const std::ffi::c_void,
                        );
                    }
                    y += 1;
                }
            }
        }
        unsafe {
            let clean = GHOSTTY_RENDER_STATE_DIRTY_FALSE;
            ghostty_render_state_set(
                self.render,
                GHOSTTY_RENDER_STATE_OPT_DIRTY,
                &clean as *const _ as *const std::ffi::c_void,
            );
        }
        let top = screen.stable_offset;
        screen.prune_below(top);

        self.reanchor();
        self.update_cursor_shape();
        self.check_colors();
        self.dispatch_callback_events();
        self.trigger_unseen_output_notif();
    }

    fn update_cursor_shape(&mut self) {
        let mut cursor: GhosttyRenderStateCursor = unsafe { std::mem::zeroed() };
        cursor.size = std::mem::size_of::<GhosttyRenderStateCursor>();
        let res = unsafe {
            ghostty_render_state_get(
                self.render,
                GHOSTTY_RENDER_STATE_DATA_CURSOR,
                &mut cursor as *mut _ as *mut std::ffi::c_void,
            )
        };
        if res != GHOSTTY_SUCCESS {
            return;
        }
        self.cursor_shape = match (cursor.visual_style, cursor.blinking) {
            (GHOSTTY_CURSOR_STYLE_BLOCK, false) => CursorShape::Default,
            (GHOSTTY_CURSOR_STYLE_BLOCK_HOLLOW, false) => CursorShape::Default,
            (GHOSTTY_CURSOR_STYLE_BLOCK, true) | (GHOSTTY_CURSOR_STYLE_BLOCK_HOLLOW, true) => {
                CursorShape::BlinkingBlock
            }
            (GHOSTTY_CURSOR_STYLE_BAR, true) => CursorShape::BlinkingBar,
            (GHOSTTY_CURSOR_STYLE_BAR, false) => CursorShape::SteadyBar,
            (GHOSTTY_CURSOR_STYLE_UNDERLINE, true) => CursorShape::BlinkingUnderline,
            (GHOSTTY_CURSOR_STYLE_UNDERLINE, false) => CursorShape::SteadyUnderline,
            _ => CursorShape::Default,
        };
    }

    /// Detect dynamic color changes (OSC 4/10/11/12 and friends)
    fn check_colors(&mut self) {
        let mut snap = vec![0u8; 256 * 3 + 9 + 256 * 3 + 9];
        unsafe {
            let base = snap.as_mut_ptr();
            let t = self.raw;
            ghostty_terminal_get(t, GHOSTTY_TERMINAL_DATA_COLOR_PALETTE, base as _);
            ghostty_terminal_get(t, GHOSTTY_TERMINAL_DATA_COLOR_FOREGROUND, base.add(768) as _);
            ghostty_terminal_get(t, GHOSTTY_TERMINAL_DATA_COLOR_BACKGROUND, base.add(771) as _);
            ghostty_terminal_get(t, GHOSTTY_TERMINAL_DATA_COLOR_CURSOR, base.add(774) as _);
            let d = base.add(777);
            ghostty_terminal_get(t, GHOSTTY_TERMINAL_DATA_COLOR_PALETTE_DEFAULT, d as _);
            ghostty_terminal_get(
                t,
                GHOSTTY_TERMINAL_DATA_COLOR_FOREGROUND_DEFAULT,
                d.add(768) as _,
            );
            ghostty_terminal_get(
                t,
                GHOSTTY_TERMINAL_DATA_COLOR_BACKGROUND_DEFAULT,
                d.add(771) as _,
            );
            ghostty_terminal_get(t, GHOSTTY_TERMINAL_DATA_COLOR_CURSOR_DEFAULT, d.add(774) as _);
        }
        if snap == self.colors_snapshot {
            return;
        }
        let first_time = self.colors_snapshot.is_empty();
        let (current, defaults) = snap.split_at(777);
        let at = |buf: &[u8], i: usize| GhosttyColorRgb {
            r: buf[i * 3],
            g: buf[i * 3 + 1],
            b: buf[i * 3 + 2],
        };
        let same = |i: usize| current[i * 3..i * 3 + 3] == defaults[i * 3..i * 3 + 3];

        if current == defaults {
            self.palette = None;
        } else {
            let mut palette = self.config.color_palette();
            for i in 0..256 {
                if !same(i) {
                    palette.colors.0[i] = srgba(at(current, i));
                }
            }
            if !same(256) {
                palette.foreground = srgba(at(current, 256));
            }
            if !same(257) {
                palette.background = srgba(at(current, 257));
            }
            if !same(258) {
                let c = srgba(at(current, 258));
                palette.cursor_bg = c;
                palette.cursor_border = c;
            }
            self.palette = Some(palette);
        }
        self.colors_snapshot = snap;
        if !first_time {
            self.screens[self.active].mark_all(self.seqno);
            if let Some(handler) = self.alert_handler.as_mut() {
                handler.alert(Alert::PaletteChanged);
            }
        }
    }

    fn dispatch_callback_events(&mut self) {
        let cb = self.cb();
        let mut alerts = std::mem::take(&mut cb.alerts);
        let title_changed = std::mem::take(&mut cb.title_changed);
        let pwd_changed = std::mem::take(&mut cb.pwd_changed);
        let progress = cb.progress.take();

        if title_changed {
            let s: GhosttyString = self.get(
                GHOSTTY_TERMINAL_DATA_TITLE,
                GhosttyString {
                    ptr: std::ptr::null(),
                    len: 0,
                },
            );
            let title = unsafe { gstr_to_string(s) };
            if title != self.title {
                self.title = title.clone();
                alerts.push(Alert::WindowTitleChanged(title));
            }
        }

        if pwd_changed {
            let s: GhosttyString = self.get(
                GHOSTTY_TERMINAL_DATA_PWD,
                GhosttyString {
                    ptr: std::ptr::null(),
                    len: 0,
                },
            );
            let pwd = unsafe { gstr_to_string(s) };
            let url = Url::parse(&pwd)
                .ok()
                .or_else(|| Url::from_directory_path(&pwd).ok());
            if url.is_some() && url != self.current_dir {
                self.current_dir = url;
                alerts.push(Alert::CurrentWorkingDirectoryChanged);
            }
        }

        if let Some(progress) = progress {
            if progress != self.progress {
                self.progress = progress.clone();
                alerts.push(Alert::Progress(progress));
            }
        }

        for alert in &alerts {
            if let Alert::SetUserVar { name, value } = alert {
                self.user_vars.insert(name.clone(), value.clone());
            }
        }

        if let Some(handler) = self.alert_handler.as_mut() {
            for alert in alerts {
                handler.alert(alert);
            }
        }
    }

    fn trigger_unseen_output_notif(&mut self) {
        if self.has_unseen_output() && self.lost_focus_seqno > self.lost_focus_alerted_seqno {
            self.lost_focus_alerted_seqno = self.seqno;
            if let Some(handler) = self.alert_handler.as_mut() {
                handler.alert(Alert::OutputSinceFocusLost);
            }
        }
    }

    // -----------------------------------------------------------------
    // Output from the program

    /// Feed bytes that the program wrote to the pty
    pub fn advance_bytes<B: AsRef<[u8]>>(&mut self, bytes: B) {
        let bytes = bytes.as_ref();
        if bytes.is_empty() {
            return;
        }
        for (name, value) in self.sniffer.scan(bytes) {
            self.cb().alerts.push(Alert::SetUserVar { name, value });
        }
        unsafe {
            ghostty_terminal_vt_write(self.raw, bytes.as_ptr(), bytes.len());
        }
        self.sync();
    }

    /// Apply termwiz actions by re-encoding them as escape sequences.
    /// Prefer `advance_bytes`, which avoids the round trip.
    pub fn perform_actions(&mut self, actions: Vec<termwiz::escape::Action>) {
        let mut buf = String::new();
        for action in actions {
            use std::fmt::Write;
            write!(buf, "{}", action).ok();
        }
        self.advance_bytes(buf.as_bytes());
    }

    // -----------------------------------------------------------------
    // Configuration and handlers

    pub fn set_config(&mut self, config: Arc<dyn TerminalConfiguration>) {
        self.config = config;
        self.apply_config();
        self.sync();
    }

    pub fn get_config(&self) -> Arc<dyn TerminalConfiguration> {
        Arc::clone(&self.config)
    }

    pub fn set_clipboard(&mut self, clipboard: &Arc<dyn Clipboard>) {
        self.cb().clipboard.replace(Arc::clone(clipboard));
    }

    pub fn set_download_handler(&mut self, handler: &Arc<dyn DownloadHandler>) {
        self.download_handler.replace(Arc::clone(handler));
    }

    pub fn set_device_control_handler(&mut self, handler: Box<dyn DeviceControlHandler>) {
        // libghostty-vt doesn't expose DCS passthrough (eg: tmux control
        // mode), so this is retained but currently unused.
        self.device_control_handler.replace(handler);
    }

    pub fn set_notification_handler(&mut self, handler: Box<dyn AlertHandler>) {
        self.alert_handler.replace(handler);
    }

    pub fn enable_conpty_quirks(&mut self) {}

    // -----------------------------------------------------------------
    // Simple state accessors

    pub fn current_seqno(&self) -> SequenceNo {
        self.seqno
    }

    pub fn get_title(&self) -> &str {
        &self.title
    }

    pub fn get_progress(&self) -> Progress {
        self.progress.clone()
    }

    pub fn get_current_dir(&self) -> Option<&Url> {
        self.current_dir.as_ref()
    }

    pub fn user_vars(&self) -> &HashMap<String, String> {
        &self.user_vars
    }

    pub fn palette(&self) -> ColorPalette {
        self.palette
            .clone()
            .unwrap_or_else(|| self.config.color_palette())
    }

    pub fn is_mouse_grabbed(&self) -> bool {
        self.get::<bool>(GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING, false)
    }

    pub fn is_alt_screen_active(&self) -> bool {
        self.active_screen() == ALTERNATE
    }

    pub fn bracketed_paste_enabled(&self) -> bool {
        self.mode(2004, false)
    }

    pub fn get_reverse_video(&self) -> bool {
        self.mode(5, false)
    }

    pub fn get_keyboard_encoding(&self) -> KeyboardEncoding {
        let flags: GhosttyKittyKeyFlags = self.get(GHOSTTY_TERMINAL_DATA_KITTY_KEYBOARD_FLAGS, 0);
        if flags != 0 {
            KeyboardEncoding::Kitty(termwiz::escape::csi::KittyKeyboardFlags::from_bits_truncate(
                flags as u16,
            ))
        } else {
            KeyboardEncoding::Xterm
        }
    }

    pub fn get_size(&self) -> TerminalSize {
        self.size
    }

    pub fn has_unseen_output(&self) -> bool {
        !self.focused && self.seqno > self.lost_focus_seqno
    }

    /// Returns the 0-based cursor position relative to the top left of
    /// the active area
    pub fn cursor_pos(&self) -> CursorPosition {
        let x = self.get::<u16>(GHOSTTY_TERMINAL_DATA_CURSOR_X, 0) as usize;
        let y = self.get::<u16>(GHOSTTY_TERMINAL_DATA_CURSOR_Y, 0) as VisibleRowIndex;
        let visible = self.get::<bool>(GHOSTTY_TERMINAL_DATA_CURSOR_VISIBLE, true);
        CursorPosition {
            x,
            y,
            shape: self.cursor_shape,
            visibility: if visible {
                CursorVisibility::Visible
            } else {
                CursorVisibility::Hidden
            },
            seqno: self.seqno,
        }
    }

    // -----------------------------------------------------------------
    // Geometry

    pub fn resize(&mut self, size: TerminalSize) {
        self.size = size;
        // Tracked pins influence how libghostty reflows; don't let our
        // bookkeeping affect the result.  Everything is invalidated below.
        for screen in self.screens.iter_mut() {
            screen.free_anchor();
        }
        self.resize_ghostty();
        self.seqno += 1;
        let seqno = self.seqno;
        for screen in self.screens.iter_mut() {
            screen.mark_all(seqno);
        }
        self.sync();
    }

    pub fn visible_row_to_stable_row(&self, row: VisibleRowIndex) -> StableRowIndex {
        let screen = &self.screens[self.active];
        screen.stable_offset + screen.history as StableRowIndex + row as StableRowIndex
    }

    pub fn dimensions(&self) -> Dimensions {
        let screen = &self.screens[self.active];
        let rows = self.rows();
        Dimensions {
            cols: self.cols() as usize,
            viewport_rows: rows,
            scrollback_rows: screen.history + rows,
            physical_top: screen.stable_offset + screen.history as StableRowIndex,
            scrollback_top: screen.stable_offset,
            dpi: self.size.dpi,
            pixel_width: self.size.pixel_width,
            pixel_height: self.size.pixel_height,
            reverse_video: self.get_reverse_video(),
        }
    }

    /// Clamp a stable range to the rows that currently exist
    fn stable_range(&self, range: &Range<StableRowIndex>) -> Range<StableRowIndex> {
        let screen = &self.screens[self.active];
        let top = screen.stable_offset;
        let end = top + (screen.history + self.rows()) as StableRowIndex;
        let start = range.start.clamp(top, end);
        let stop = range.end.clamp(start, end);
        start..stop
    }

    // -----------------------------------------------------------------
    // Line access

    fn read_line(&mut self, row: StableRowIndex) -> Option<Line> {
        let screen = &self.screens[self.active];
        let y = row - screen.stable_offset;
        if y < 0 {
            return None;
        }
        let y = y as usize;
        let history = screen.history;
        let point = if y >= history {
            GhosttyPoint {
                tag: GHOSTTY_POINT_TAG_ACTIVE,
                value: GhosttyPointValue {
                    coordinate: GhosttyPointCoordinate {
                        x: 0,
                        y: (y - history) as u32,
                    },
                },
            }
        } else {
            GhosttyPoint {
                tag: GHOSTTY_POINT_TAG_HISTORY,
                value: GhosttyPointValue {
                    coordinate: GhosttyPointCoordinate { x: 0, y: y as u32 },
                },
            }
        };
        let seqno = screen.seqno_of(row);
        let cols = self.cols();
        let mut gref = GhosttyGridRef {
            node: std::ptr::null_mut(),
            x: 0,
            y: 0,
        };
        unsafe {
            if ghostty_terminal_grid_ref(self.raw, point, &mut gref) != GHOSTTY_SUCCESS {
                return None;
            }
            Some(convert::read_row(gref, cols, seqno, &mut self.links))
        }
    }

    fn ensure_cached(&mut self, range: Range<StableRowIndex>) {
        for row in range {
            if !self.screens[self.active].lines.contains_key(&row) {
                if let Some(line) = self.read_line(row) {
                    self.screens[self.active].lines.insert(row, line);
                }
            }
        }
    }

    /// Returns the rows in `range` that changed after `seqno`
    pub fn get_changed_stable_rows(
        &self,
        range: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> Vec<Range<StableRowIndex>> {
        let range = self.stable_range(&range);
        let screen = &self.screens[self.active];
        if range.is_empty() {
            return vec![];
        }
        if screen.bulk_seqno > seqno {
            return vec![range];
        }
        let mut result = vec![];
        if let Some((start, _)) = screen.suffix_marks.iter().find(|(_, s)| *s > seqno) {
            let start = (*start).max(range.start);
            if start < range.end {
                result.push(start..range.end);
            }
        }
        for (row, s) in screen.row_seqno.range(range.clone()) {
            if *s > seqno {
                result.push(*row..*row + 1);
            }
        }
        result
    }

    /// Returns copies of the lines in the requested range, along with the
    /// stable index of the first returned line
    pub fn get_lines(&mut self, range: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        let range = self.stable_range(&range);
        self.ensure_cached(range.clone());
        let lines = self.screens[self.active]
            .lines
            .range(range.clone())
            .map(|(_, l)| l.clone())
            .collect();
        (range.start, lines)
    }

    /// Calls `func` with mutable references to the lines in `range`
    pub fn with_lines_mut<F>(&mut self, range: Range<StableRowIndex>, func: F)
    where
        F: FnOnce(StableRowIndex, &mut [&mut Line]),
    {
        let range = self.stable_range(&range);
        self.ensure_cached(range.clone());
        let mut lines: Vec<&mut Line> = self.screens[self.active]
            .lines
            .range_mut(range.clone())
            .map(|(_, l)| l)
            .collect();
        func(range.start, &mut lines);
    }

    fn logical_line_bounds(&mut self, range: &Range<StableRowIndex>) -> Range<StableRowIndex> {
        // Avoid pathological cases with enormous wrapped lines
        const MAX_LOGICAL_LINE_LEN: usize = 1024;
        let mut range = self.stable_range(range);
        let top = self.screens[self.active].stable_offset;
        let mut back_len = 0;
        while range.start > top {
            self.ensure_cached(range.start - 1..range.start);
            let screen = &self.screens[self.active];
            let Some(prior) = screen.lines.get(&(range.start - 1)) else {
                break;
            };
            if !prior.last_cell_was_wrapped() || prior.len() + back_len > MAX_LOGICAL_LINE_LEN {
                break;
            }
            back_len += prior.len();
            range.start -= 1;
        }
        // Extend forwards so that the final logical line is complete
        let end_limit = self.stable_range(&(range.end..StableRowIndex::MAX)).end;
        while range.end > range.start && range.end < end_limit {
            self.ensure_cached(range.end - 1..range.end);
            let wrapped = self.screens[self.active]
                .lines
                .get(&(range.end - 1))
                .map(|l| l.last_cell_was_wrapped())
                .unwrap_or(false);
            if !wrapped {
                break;
            }
            range.end += 1;
        }
        self.ensure_cached(range.clone());
        range
    }

    /// Groups the physical lines in `range` into logical (unwrapped)
    /// lines and calls `f` with each group.  `f` returns false to stop.
    pub fn for_each_logical_line_in_stable_range_mut<F>(
        &mut self,
        range: Range<StableRowIndex>,
        mut f: F,
    ) where
        F: FnMut(Range<StableRowIndex>, &mut [&mut Line]) -> bool,
    {
        let range = self.logical_line_bounds(&range);
        let mut lines: Vec<(StableRowIndex, &mut Line)> = self.screens[self.active]
            .lines
            .range_mut(range)
            .map(|(k, l)| (*k, l))
            .collect();
        let mut idx = 0;
        while idx < lines.len() {
            let mut end = idx;
            while end + 1 < lines.len() && lines[end].1.last_cell_was_wrapped() {
                end += 1;
            }
            let first = lines[idx].0;
            let last = lines[end].0;
            let mut group: Vec<&mut Line> =
                lines[idx..=end].iter_mut().map(|(_, l)| &mut **l).collect();
            if !f(first..last + 1, &mut group) {
                return;
            }
            idx = end + 1;
        }
    }

    /// Immutable variant of `for_each_logical_line_in_stable_range_mut`
    pub fn for_each_logical_line_in_stable_range<F>(
        &mut self,
        range: Range<StableRowIndex>,
        mut f: F,
    ) where
        F: FnMut(Range<StableRowIndex>, &[&Line]) -> bool,
    {
        self.for_each_logical_line_in_stable_range_mut(range, |r, lines| {
            let lines: Vec<&Line> = lines.iter().map(|l| &**l).collect();
            f(r, &lines)
        });
    }

    pub fn get_semantic_zones(&mut self) -> anyhow::Result<Vec<SemanticZone>> {
        let top = self.screens[self.active].stable_offset;
        let all = self.stable_range(&(top..StableRowIndex::MAX));
        self.ensure_cached(all.clone());

        let mut current_zone: Option<SemanticZone> = None;
        let mut zones = vec![];
        for (stable_row, line) in self.screens[self.active].lines.range_mut(all) {
            let stable_row = *stable_row;
            for zone_range in line.semantic_zone_ranges() {
                let new_zone = match current_zone.as_ref() {
                    None => true,
                    Some(zone) => zone.semantic_type != zone_range.semantic_type,
                };
                if new_zone {
                    if let Some(zone) = current_zone.take() {
                        zones.push(zone);
                    }
                    current_zone.replace(SemanticZone {
                        start_x: zone_range.range.start as usize,
                        start_y: stable_row,
                        end_x: zone_range.range.end as usize,
                        end_y: stable_row,
                        semantic_type: zone_range.semantic_type,
                    });
                }
                if let Some(zone) = current_zone.as_mut() {
                    zone.end_x = zone_range.range.end as usize;
                    zone.end_y = stable_row;
                }
            }
        }
        if let Some(zone) = current_zone.take() {
            zones.push(zone);
        }
        Ok(zones)
    }

    // -----------------------------------------------------------------
    // Scrollback management

    pub fn erase_scrollback(&mut self) {
        self.advance_bytes(b"\x1b[3J");
    }

    /// Clear the scrollback and the viewport, keeping the line holding
    /// the cursor, which is moved to the top of the screen
    pub fn erase_scrollback_and_viewport(&mut self) {
        let x = self.get::<u16>(GHOSTTY_TERMINAL_DATA_CURSOR_X, 0) as usize;
        let y = self.get::<u16>(GHOSTTY_TERMINAL_DATA_CURSOR_Y, 0) as usize;
        let mut seq = String::from("\x1b[3J");
        if y > 0 {
            // Scroll the cursor line to the top, erase what remains below
            // it, then drop anything that the scroll pushed into history
            seq.push_str(&format!("\x1b[{y}S\x1b[2;1H\x1b[J\x1b[3J"));
        } else {
            seq.push_str("\x1b[2;1H\x1b[J");
        }
        seq.push_str(&format!("\x1b[1;{}H", x + 1));
        self.advance_bytes(seq.as_bytes());
    }

    // -----------------------------------------------------------------
    // Input

    fn write_to_pty(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let cb = self.cb();
        cb.writer.write_all(bytes)?;
        cb.writer.flush()?;
        Ok(())
    }

    fn encode_key(
        &mut self,
        key: &KeyCode,
        mods: KeyModifiers,
        action: GhosttyKeyAction,
    ) -> anyhow::Result<Vec<u8>> {
        let Some(desc) = keys::describe_key(key, mods) else {
            log::debug!("ghostty: no key mapping for {key:?}");
            return Ok(vec![]);
        };
        unsafe {
            ghostty_key_encoder_set_from_terminal(self.key_encoder, self.raw);
            // wezterm has always sent ESC-prefixed sequences for Alt
            let alt_esc = true;
            ghostty_key_encoder_set(
                self.key_encoder,
                GHOSTTY_KEY_ENCODER_OPT_ALT_ESC_PREFIX,
                &alt_esc as *const _ as *const std::ffi::c_void,
            );
            let ev = self.key_event;
            ghostty_key_event_set_action(ev, action);
            ghostty_key_event_set_key(ev, desc.key);
            ghostty_key_event_set_mods(ev, keys::mods_to_ghostty(mods));
            ghostty_key_event_set_consumed_mods(ev, desc.consumed);
            ghostty_key_event_set_composing(ev, false);
            ghostty_key_event_set_unshifted_codepoint(ev, desc.unshifted);
            match &desc.utf8 {
                Some(s) => ghostty_key_event_set_utf8(ev, gstr(s)),
                None => ghostty_key_event_set_utf8(
                    ev,
                    GhosttyString {
                        ptr: std::ptr::null(),
                        len: 0,
                    },
                ),
            };

            let mut buf = vec![0u8; 128];
            let mut written = 0usize;
            let mut res = ghostty_key_encoder_encode(
                self.key_encoder,
                ev,
                buf.as_mut_ptr(),
                buf.len(),
                &mut written,
            );
            if res == GHOSTTY_OUT_OF_SPACE {
                buf.resize(written, 0);
                res = ghostty_key_encoder_encode(
                    self.key_encoder,
                    ev,
                    buf.as_mut_ptr(),
                    buf.len(),
                    &mut written,
                );
            }
            // Don't retain a pointer to `desc.utf8` past this call
            ghostty_key_event_set_utf8(
                ev,
                GhosttyString {
                    ptr: std::ptr::null(),
                    len: 0,
                },
            );
            if res != GHOSTTY_SUCCESS {
                anyhow::bail!("ghostty key encoding failed with {res}");
            }
            buf.truncate(written);
            Ok(buf)
        }
    }

    pub fn key_down(&mut self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()> {
        let bytes = self.encode_key(&key, mods, GHOSTTY_KEY_ACTION_PRESS)?;
        if self.config.debug_key_events() {
            log::info!("ghostty: key_down {key:?} {mods:?} -> {bytes:?}");
        }
        self.write_to_pty(&bytes)
    }

    pub fn key_up(&mut self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()> {
        let bytes = self.encode_key(&key, mods, GHOSTTY_KEY_ACTION_RELEASE)?;
        self.write_to_pty(&bytes)
    }

    pub fn send_paste(&mut self, text: &str) -> anyhow::Result<()> {
        let bracketed = self.bracketed_paste_enabled();
        let canon = if bracketed {
            wezterm_term::config::NewlineCanon::None
        } else {
            self.config.canonicalize_pasted_newlines()
        };
        let text = canon.canonicalize(text);
        // De-fang any embedded bracketed paste delimiters
        let text = text.replace("\x1b[200~", "").replace("\x1b[201~", "");
        let mut buf = String::with_capacity(text.len() + 12);
        if bracketed {
            buf.push_str("\x1b[200~");
        }
        buf.push_str(&text);
        if bracketed {
            buf.push_str("\x1b[201~");
        }
        self.write_to_pty(buf.as_bytes())
    }

    pub fn focus_changed(&mut self, focused: bool) {
        if focused == self.focused {
            return;
        }
        if !focused {
            // Let the application know that held buttons were released
            for button in std::mem::take(&mut self.current_mouse_buttons) {
                self.mouse_event(MouseEvent {
                    kind: MouseEventKind::Release,
                    button,
                    modifiers: KeyModifiers::NONE,
                    x: 0,
                    y: 0,
                    x_pixel_offset: 0,
                    y_pixel_offset: 0,
                })
                .ok();
            }
        }
        if self.mode(1004, false) {
            let mut buf = [0u8; 16];
            let mut written = 0usize;
            let ev = if focused {
                GHOSTTY_FOCUS_GAINED
            } else {
                GHOSTTY_FOCUS_LOST
            };
            if unsafe { ghostty_focus_encode(ev, buf.as_mut_ptr(), buf.len(), &mut written) }
                == GHOSTTY_SUCCESS
            {
                self.write_to_pty(&buf[..written]).ok();
            }
        }
        self.focused = focused;
        if !focused {
            self.lost_focus_seqno = self.seqno;
        }
    }

    pub fn mouse_event(&mut self, event: MouseEvent) -> anyhow::Result<()> {
        // Track pressed buttons so we can release them on focus loss
        match (event.kind, event.button) {
            (_, MouseButton::WheelUp(_))
            | (_, MouseButton::WheelDown(_))
            | (_, MouseButton::WheelLeft(_))
            | (_, MouseButton::WheelRight(_))
            | (_, MouseButton::None) => {}
            (MouseEventKind::Press, b) => {
                if !self.current_mouse_buttons.contains(&b) {
                    self.current_mouse_buttons.push(b);
                }
            }
            (MouseEventKind::Release, b) => self.current_mouse_buttons.retain(|x| *x != b),
            _ => {}
        }

        if !self.is_mouse_grabbed() {
            // Alternate scroll mode: on the alternate screen, translate the
            // wheel into cursor keys so that pagers and editors scroll
            if self.is_alt_screen_active() && self.mode(1007, false) {
                let (key, n) = match event.button {
                    MouseButton::WheelUp(_) => (KeyCode::UpArrow, 1),
                    MouseButton::WheelDown(_) => (KeyCode::DownArrow, 1),
                    _ => return Ok(()),
                };
                if event.kind == MouseEventKind::Press {
                    let speed = self.config.alternate_buffer_wheel_scroll_speed() as usize;
                    for _ in 0..n * speed.max(1) {
                        self.key_down(key.clone(), KeyModifiers::NONE)?;
                    }
                }
            }
            return Ok(());
        }

        let cols = self.size.cols.max(1);
        let rows = self.size.rows.max(1);
        let cell_w = (self.size.pixel_width / cols).max(1);
        let cell_h = (self.size.pixel_height / rows).max(1);

        let (action, button) = match (event.kind, event.button) {
            (MouseEventKind::Press, MouseButton::Left) => {
                (GHOSTTY_MOUSE_ACTION_PRESS, Some(GHOSTTY_MOUSE_BUTTON_LEFT))
            }
            (MouseEventKind::Press, MouseButton::Middle) => {
                (GHOSTTY_MOUSE_ACTION_PRESS, Some(GHOSTTY_MOUSE_BUTTON_MIDDLE))
            }
            (MouseEventKind::Press, MouseButton::Right) => {
                (GHOSTTY_MOUSE_ACTION_PRESS, Some(GHOSTTY_MOUSE_BUTTON_RIGHT))
            }
            (MouseEventKind::Press, MouseButton::WheelUp(_)) => {
                (GHOSTTY_MOUSE_ACTION_PRESS, Some(GHOSTTY_MOUSE_BUTTON_FOUR))
            }
            (MouseEventKind::Press, MouseButton::WheelDown(_)) => {
                (GHOSTTY_MOUSE_ACTION_PRESS, Some(GHOSTTY_MOUSE_BUTTON_FIVE))
            }
            (MouseEventKind::Press, MouseButton::WheelLeft(_)) => {
                (GHOSTTY_MOUSE_ACTION_PRESS, Some(GHOSTTY_MOUSE_BUTTON_SIX))
            }
            (MouseEventKind::Press, MouseButton::WheelRight(_)) => {
                (GHOSTTY_MOUSE_ACTION_PRESS, Some(GHOSTTY_MOUSE_BUTTON_SEVEN))
            }
            (MouseEventKind::Release, MouseButton::Left) => {
                (GHOSTTY_MOUSE_ACTION_RELEASE, Some(GHOSTTY_MOUSE_BUTTON_LEFT))
            }
            (MouseEventKind::Release, MouseButton::Middle) => {
                (GHOSTTY_MOUSE_ACTION_RELEASE, Some(GHOSTTY_MOUSE_BUTTON_MIDDLE))
            }
            (MouseEventKind::Release, MouseButton::Right) => {
                (GHOSTTY_MOUSE_ACTION_RELEASE, Some(GHOSTTY_MOUSE_BUTTON_RIGHT))
            }
            (MouseEventKind::Release, _) => return Ok(()),
            (MouseEventKind::Move, MouseButton::Left) => {
                (GHOSTTY_MOUSE_ACTION_MOTION, Some(GHOSTTY_MOUSE_BUTTON_LEFT))
            }
            (MouseEventKind::Move, MouseButton::Middle) => {
                (GHOSTTY_MOUSE_ACTION_MOTION, Some(GHOSTTY_MOUSE_BUTTON_MIDDLE))
            }
            (MouseEventKind::Move, MouseButton::Right) => {
                (GHOSTTY_MOUSE_ACTION_MOTION, Some(GHOSTTY_MOUSE_BUTTON_RIGHT))
            }
            (MouseEventKind::Move, _) => (GHOSTTY_MOUSE_ACTION_MOTION, None),
            (MouseEventKind::Press, MouseButton::None) => return Ok(()),
        };

        let x = (event.x * cell_w) as isize + event.x_pixel_offset;
        let y = (event.y.max(0) as usize * cell_h) as isize + event.y_pixel_offset;

        let mut size = GhosttyMouseEncoderSize {
            size: std::mem::size_of::<GhosttyMouseEncoderSize>(),
            screen_width: self.size.pixel_width.max(cols * cell_w) as u32,
            screen_height: self.size.pixel_height.max(rows * cell_h) as u32,
            cell_width: cell_w as u32,
            cell_height: cell_h as u32,
            padding_top: 0,
            padding_bottom: 0,
            padding_right: 0,
            padding_left: 0,
        };
        let any_pressed = !self.current_mouse_buttons.is_empty();

        let mut buf = [0u8; 64];
        let mut written = 0usize;
        let res = unsafe {
            let enc = self.mouse_encoder;
            ghostty_mouse_encoder_set_from_terminal(enc, self.raw);
            ghostty_mouse_encoder_set(
                enc,
                GHOSTTY_MOUSE_ENCODER_OPT_SIZE,
                &mut size as *mut _ as *const std::ffi::c_void,
            );
            ghostty_mouse_encoder_set(
                enc,
                GHOSTTY_MOUSE_ENCODER_OPT_ANY_BUTTON_PRESSED,
                &any_pressed as *const _ as *const std::ffi::c_void,
            );
            let ev = self.mouse_event;
            ghostty_mouse_event_set_action(ev, action);
            match button {
                Some(b) => ghostty_mouse_event_set_button(ev, b),
                None => ghostty_mouse_event_clear_button(ev),
            };
            ghostty_mouse_event_set_mods(ev, keys::mods_to_ghostty(event.modifiers));
            ghostty_mouse_event_set_position(
                ev,
                GhosttyMousePosition {
                    x: x.max(0) as f32,
                    y: y.max(0) as f32,
                },
            );
            ghostty_mouse_encoder_encode(enc, ev, buf.as_mut_ptr(), buf.len(), &mut written)
        };
        if res != GHOSTTY_SUCCESS {
            anyhow::bail!("ghostty mouse encoding failed with {res}");
        }
        self.write_to_pty(&buf[..written])
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        unsafe {
            for screen in self.screens.iter_mut() {
                screen.free_anchor();
            }
            ghostty_mouse_event_free(self.mouse_event);
            ghostty_mouse_encoder_free(self.mouse_encoder);
            ghostty_key_event_free(self.key_event);
            ghostty_key_encoder_free(self.key_encoder);
            ghostty_render_state_row_iterator_free(self.row_iter);
            ghostty_render_state_free(self.render);
            ghostty_terminal_free(self.raw);
        }
    }
}

#[cfg(test)]
mod test;
