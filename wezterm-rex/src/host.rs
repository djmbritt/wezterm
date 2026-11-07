//! One worker per Rex host: the control connection, terminal data streams,
//! and reconnects.
//!
//! The worker runs on its own thread with a single-threaded Tokio runtime.
//! wezterm talks to it with [`HostCommand`]s and hears back through an
//! [`EventSink`] callback, which is called on the worker thread.
//!
//! This is a trimmed port of rex-linux's `src/connection.rs`
//! (<https://github.com/shuv1337/rex-rune>, MIT), keeping its rules:
//!
//! * Attaching never creates, destroys or reconfigures anything on the
//!   server. Creating a session is an explicit command.
//! * Input that failed to send is never replayed after a reconnect.
//! * Presses claim a block (`session.own_block`); releases, motion and wheel
//!   turns don't.
use crate::transport::{self, Client};
use crate::wire::{self, ApcParser, Part, SnapshotBuffer};
use anyhow::{Context, Result};
use rexrs::api::session;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const TERMINAL_CREATOR: &str = "com.superlogical.terminal";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone, Debug)]
pub struct HostConfig {
    /// `unix:///path.sock`, `https://…`, `http://…`, `wss://…` or `ws://…`
    pub endpoint: String,
    /// The session to show, by exact label or id. `None` picks the
    /// server's default (or first) session.
    pub session: Option<String>,
    pub client_name: String,
    pub client_version: String,
}

/// What the worker reports. Delivered on the worker thread.
#[derive(Debug)]
pub enum HostEvent {
    /// Connection state, for display.
    Status {
        connected: bool,
        message: String,
    },
    /// Connected and identified. `client_id` is how the server names this
    /// client, e.g. as a block's owner.
    Connected {
        client_id: String,
        api_revision: u32,
    },
    Sessions(Vec<session::Session>),
    /// The session this client shows changed (`None`: there is none).
    Selected(Option<String>),
    /// A newer view (windows, layouts, focus) of an attached session.
    View {
        session_id: String,
        view: session::ViewResponse,
    },
    SessionRemoved {
        session_id: String,
    },
    BlockClosed {
        block_id: String,
    },
    OwnerChanged {
        block_id: String,
        owner: Option<String>,
    },
    /// Something the user should know about (a failed action, ...).
    Notice(String),
    Stream {
        block_id: String,
        event: StreamEvent,
    },
}

#[derive(Debug)]
pub enum StreamEvent {
    /// A complete libghostty snapshot. Replace the terminal with it.
    Snapshot(Vec<u8>),
    /// Live terminal output following the snapshot.
    Output(Vec<u8>),
    /// The server's terminal changed size.
    Resize {
        cols: u16,
        rows: u16,
        cell_width: u32,
        cell_height: u32,
    },
    /// The server's terminal theme (default colours).
    Theme(Theme),
    /// The terminal's program exited; the block stays until closed.
    Exited(Option<i64>),
    /// Stream state, for display. `live` is false while reconnecting.
    Status { live: bool, message: String },
    /// The terminal is gone on the server.
    Closed,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Theme {
    pub foreground: Option<wire::Rgb>,
    pub background: Option<wire::Rgb>,
    pub cursor: Option<wire::Rgb>,
    pub palette: Vec<(u8, wire::Rgb)>,
}

impl Theme {
    pub fn from_metadata(value: &Value) -> Self {
        let palette = value["palette"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter_map(|entry| {
                let (index, color) = entry.split_once('=')?;
                Some((index.parse::<u8>().ok()?, wire::parse_color(Some(color))?))
            })
            .collect();
        Self {
            foreground: wire::parse_color(value["foreground"].as_str()),
            background: wire::parse_color(value["background"].as_str()),
            cursor: wire::parse_color(value["cursor"].as_str()),
            palette,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Input {
    /// A `client.key` payload (see `input::key_payload`).
    Key(Value),
    /// Bracketed-paste aware paste, encoded by the server.
    Paste(String),
    /// A `block.event` mouse payload.
    Mouse(Value),
    /// A `block.event` wheel payload.
    Wheel(Value),
    /// Raw text typed into the terminal (`terminal.write`).
    Write(String),
}

impl Input {
    /// Presses claim a block; releases, motion and wheel turns don't.
    fn claims(&self) -> bool {
        match self {
            Input::Key(key) => key["action"] != "release",
            Input::Mouse(mouse) => mouse["action"] == "press",
            Input::Wheel(_) => false,
            Input::Paste(_) | Input::Write(_) => true,
        }
    }
}

pub type Reply = flume::Sender<std::result::Result<Value, String>>;

pub enum HostCommand {
    /// Show (attach) this session, by id.
    Select(String),
    /// Create a session and show it. Explicit user action only.
    CreateSession {
        label: Option<String>,
    },
    /// Any control method, e.g. `session.new_split`. The session's view is
    /// re-read afterwards when the payload names one.
    Request {
        method: String,
        payload: Value,
        reply: Option<Reply>,
    },
    Input {
        session_id: String,
        block_id: String,
        input: Input,
    },
    /// Ask for this size; the server confirms with `StreamEvent::Resize`.
    Resize {
        session_id: String,
        block_id: String,
        cols: u16,
        rows: u16,
        cell_width: u16,
        cell_height: u16,
    },
    OpenStream {
        session_id: String,
        block_id: String,
    },
    CloseStream {
        block_id: String,
    },
    /// Drop a stream and open it again from a fresh snapshot (after a
    /// snapshot that couldn't be restored).
    RestartStream {
        session_id: String,
        block_id: String,
    },
    /// Take these blocks (`session.own_block`), e.g. the visible ones when
    /// the client is focused, so their size follows this client.
    Claim {
        session_id: String,
        block_ids: Vec<String>,
    },
    Reconnect,
    Shutdown,
}

pub type EventSink = Arc<dyn Fn(HostEvent) + Send + Sync>;

/// A running host worker. Dropping it shuts the worker down.
pub struct Host {
    commands: flume::Sender<HostCommand>,
}

impl Host {
    pub fn spawn(config: HostConfig, sink: EventSink) -> Result<Self> {
        transport::validate_endpoint(&config.endpoint)?;
        let (commands, receiver) = flume::unbounded();
        std::thread::Builder::new()
            .name(format!("rex-host {}", config.endpoint))
            .spawn(move || {
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(anyhow::Error::from)
                    .and_then(|runtime| {
                        tokio::task::LocalSet::new()
                            .block_on(&runtime, run_host(config, receiver, sink.clone()))
                    });
                if let Err(error) = result {
                    sink(HostEvent::Status {
                        connected: false,
                        message: format!("{error:#}"),
                    });
                }
            })
            .context("starting Rex host worker")?;
        Ok(Self { commands })
    }

    pub fn send(&self, command: HostCommand) -> bool {
        self.commands.send(command).is_ok()
    }

    pub fn sender(&self) -> flume::Sender<HostCommand> {
        self.commands.clone()
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.commands.send(HostCommand::Shutdown);
    }
}

struct TicketRequest {
    session_id: String,
    block_id: String,
    reply: tokio::sync::oneshot::Sender<std::result::Result<Ticket, String>>,
}

struct Ticket {
    token: String,
    /// From the bootstrap pool rather than `data.connect`.
    bundled: bool,
}

struct PendingTicket {
    token: String,
    expires: Instant,
}

/// Per-connection protocol state.
#[derive(Default)]
struct ControlState {
    bootstrap_seq: u64,
    list_seq: u64,
    view_seq: HashMap<String, u64>,
    revisions: HashMap<String, u64>,
    tickets: HashMap<(String, String), PendingTicket>,
}

impl ControlState {
    fn add_tickets(&mut self, tickets: Vec<session::DataTicket>, started: Instant) {
        for ticket in tickets {
            if ticket.data_ref_id != "terminal"
                || ticket
                    .subprotocol
                    .as_deref()
                    .is_some_and(|p| p != wire::PROTOCOL)
            {
                continue;
            }
            if let Some(expires) = started.checked_add(Duration::from_millis(ticket.expires_in_ms))
            {
                self.tickets.insert(
                    (ticket.session_id, ticket.block_id),
                    PendingTicket {
                        token: ticket.ticket,
                        expires,
                    },
                );
            }
        }
    }

    fn take_ticket(&mut self, session: &str, block: &str) -> Option<Ticket> {
        let ticket = self.tickets.remove(&(session.into(), block.into()))?;
        (Instant::now() < ticket.expires).then_some(Ticket {
            token: ticket.token,
            bundled: true,
        })
    }

    /// Events older than the state we already loaded are skipped.
    fn includes(&self, event: &rexrs::Event) -> bool {
        let mut floor = self.bootstrap_seq;
        if matches!(
            event.name.as_str(),
            "session_created" | "session_moved" | "session_destroyed" | "session_label_changed"
        ) {
            floor = floor.max(self.list_seq);
        }
        if event.name != "session_destroyed" {
            if let Some(id) = &event.session_id {
                floor = floor.max(self.view_seq.get(id).copied().unwrap_or(0));
            }
        }
        event.seq > floor
    }
}

/// State that survives reconnects.
struct Worker {
    config: HostConfig,
    sink: EventSink,
    client_id: String,
    selected: Option<String>,
    owners: HashMap<String, Option<String>>,
    sizes: HashMap<String, (String, u16, u16, u16, u16)>,
    streams: HashMap<String, Stream>,
    tickets: flume::Sender<TicketRequest>,
    /// A disconnect notice was shown for this outage.
    told_disconnected: bool,
}

struct Stream {
    stop: flume::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        self.task.abort();
    }
}

impl Worker {
    fn emit(&self, event: HostEvent) {
        (self.sink)(event)
    }

    fn note_view(&mut self, view: &session::ViewResponse) {
        for block in view
            .windows
            .iter()
            .flat_map(|w| &w.layers)
            .flat_map(|l| &l.blocks)
        {
            if let Some(id) = &block.block_id {
                self.owners.insert(id.clone(), block.owner.clone());
            }
        }
    }

    fn apply_view(&mut self, state: &mut ControlState, id: &str, view: session::ViewResponse) {
        if state
            .revisions
            .get(id)
            .is_some_and(|old| *old > view.revision)
        {
            return;
        }
        state.revisions.insert(id.into(), view.revision);
        self.note_view(&view);
        self.emit(HostEvent::View {
            session_id: id.into(),
            view,
        });
    }

    fn owns(&self, block: &str) -> bool {
        self.owners.get(block).and_then(|o| o.as_deref()) == Some(self.client_id.as_str())
    }

    fn open_stream(&mut self, session_id: String, block_id: String) {
        if self.streams.contains_key(&block_id) {
            return;
        }
        let (stop, stopped) = flume::bounded(1);
        let task = tokio::task::spawn_local(run_stream(
            self.config.endpoint.clone(),
            session_id,
            block_id.clone(),
            self.tickets.clone(),
            self.sink.clone(),
            stopped,
        ));
        self.streams.insert(block_id, Stream { stop, task });
    }
}

async fn request(control: &mut Client, method: &str, payload: Value) -> Result<Value> {
    Ok(
        tokio::time::timeout(REQUEST_TIMEOUT, control.call_raw(method, payload))
            .await
            .context("Rex request timed out; it was not retried")??,
    )
}

async fn attach(
    control: &mut Client,
    worker: &mut Worker,
    state: &mut ControlState,
    id: &str,
) -> Result<()> {
    let reply = tokio::time::timeout(
        REQUEST_TIMEOUT,
        control.call_with_seq(session::Attach::new(id)),
    )
    .await??;
    state.view_seq.insert(id.into(), reply.seq);
    worker.apply_view(state, id, reply.value.view);
    Ok(())
}

async fn refresh(
    control: &mut Client,
    worker: &mut Worker,
    state: &mut ControlState,
    id: &str,
) -> Result<()> {
    let reply = tokio::time::timeout(
        REQUEST_TIMEOUT,
        control.call_with_seq(session::View::new(id)),
    )
    .await??;
    state.view_seq.insert(id.into(), reply.seq);
    worker.apply_view(state, id, reply.value);
    Ok(())
}

async fn list(
    control: &mut Client,
    worker: &mut Worker,
    state: &mut ControlState,
) -> Result<Vec<session::Session>> {
    let reply = tokio::time::timeout(
        REQUEST_TIMEOUT,
        control.call_with_seq(session::AttachAll::new("all")),
    )
    .await??;
    state.list_seq = reply.seq;
    let sessions = reply.value.sessions;
    worker.emit(HostEvent::Sessions(sessions.clone()));
    Ok(sessions)
}

/// Pick a session by exact id or label. An ambiguous label picks nothing.
pub fn select_session(sessions: &[session::Session], wanted: Option<&str>) -> Option<String> {
    let Some(wanted) = wanted else {
        return sessions.first().map(|s| s.session_id.clone());
    };
    let mut matches = sessions
        .iter()
        .filter(|s| wanted == s.session_id || wanted == s.label);
    let first = matches.next()?;
    matches.next().is_none().then(|| first.session_id.clone())
}

fn is_session_id(value: &str) -> bool {
    value.starts_with("session:")
}

/// A new terminal running the server user's login shell.
pub fn shell_layout() -> Value {
    json!({"block": {"flavor": "com.superlogical.terminal.shell"}})
}

async fn run_host(
    config: HostConfig,
    commands: flume::Receiver<HostCommand>,
    sink: EventSink,
) -> Result<()> {
    let (tickets, ticket_requests) = flume::unbounded::<TicketRequest>();
    let mut worker = Worker {
        selected: None,
        config,
        sink,
        client_id: String::new(),
        owners: HashMap::new(),
        sizes: HashMap::new(),
        told_disconnected: false,
        streams: HashMap::new(),
        tickets,
    };
    let mut token: Option<String> = None;
    let mut retry = 0u32;

    loop {
        let result: Result<bool> = connected(
            &mut worker,
            &mut token,
            &commands,
            &ticket_requests,
            &mut retry,
        )
        .await;

        match result {
            Ok(true) => return Ok(()),
            Ok(false) => worker.emit(HostEvent::Status {
                connected: false,
                message: "Reconnecting…".into(),
            }),
            Err(error) => worker.emit(HostEvent::Status {
                connected: false,
                message: format!("Disconnected · {error:#}"),
            }),
        }

        retry += 1;
        let delay = tokio::time::sleep(Duration::from_secs((1u64 << retry.min(3)).min(8)));
        tokio::pin!(delay);
        loop {
            tokio::select! {
                _ = &mut delay => break,
                request = ticket_requests.recv_async() => {
                    if let Ok(request) = request {
                        let _ = request.reply.send(Err("Rex host is reconnecting".into()));
                    }
                }
                command = commands.recv_async() => match command {
                    Ok(HostCommand::Shutdown) | Err(_) => return Ok(()),
                    Ok(HostCommand::Reconnect) => break,
                    Ok(command) => keep_while_disconnected(&mut worker, command),
                }
            }
        }
        // Input queued before or during the failure is dropped, never replayed.
        while let Ok(command) = commands.try_recv() {
            match command {
                HostCommand::Shutdown => return Ok(()),
                command => keep_while_disconnected(&mut worker, command),
            }
        }
    }
}

/// Commands that make sense to remember across a disconnect. Everything
/// else (input, requests) is dropped with a notice.
fn keep_while_disconnected(worker: &mut Worker, command: HostCommand) {
    match command {
        HostCommand::Select(id) => worker.selected = Some(id),
        HostCommand::Resize {
            session_id,
            block_id,
            cols,
            rows,
            cell_width,
            cell_height,
        } => {
            worker
                .sizes
                .insert(block_id, (session_id, cols, rows, cell_width, cell_height));
        }
        HostCommand::OpenStream {
            session_id,
            block_id,
        } => worker.open_stream(session_id, block_id),
        HostCommand::CloseStream { block_id } => {
            worker.streams.remove(&block_id);
            worker.sizes.remove(&block_id);
        }
        HostCommand::RestartStream {
            session_id,
            block_id,
        } => {
            worker.streams.remove(&block_id);
            worker.open_stream(session_id, block_id);
        }
        HostCommand::Request { reply, .. } => {
            if let Some(reply) = reply {
                let _ = reply.send(Err("Rex host is disconnected".into()));
            }
            tell_disconnected(worker);
        }
        HostCommand::Input { .. } | HostCommand::CreateSession { .. } => {
            tell_disconnected(worker);
        }
        // Ownership is claimed again on the next focus.
        HostCommand::Claim { .. } => {}
        HostCommand::Reconnect | HostCommand::Shutdown => {}
    }
}

/// Say once per outage that input isn't being sent.
fn tell_disconnected(worker: &mut Worker) {
    if !worker.told_disconnected {
        worker.told_disconnected = true;
        worker.emit(HostEvent::Notice(
            "Not sent while disconnected from Rex. Try again after reconnecting.".into(),
        ));
    }
}

/// One control connection, from HELLO until it fails. Returns Ok(true) on
/// shutdown, Ok(false) to reconnect.
async fn connected(
    worker: &mut Worker,
    token: &mut Option<String>,
    commands: &flume::Receiver<HostCommand>,
    ticket_requests: &flume::Receiver<TicketRequest>,
    retry: &mut u32,
) -> Result<bool> {
    let mut control = transport::control(
        &worker.config.endpoint,
        token.as_deref(),
        &worker.config.client_name,
        &worker.config.client_version,
    )
    .await?;
    let identity = control
        .welcome()
        .client
        .as_ref()
        .context("Rex client identity")?;
    *token = Some(identity.reconnect_token.clone());
    worker.client_id = identity.id.clone();
    worker.emit(HostEvent::Connected {
        client_id: worker.client_id.clone(),
        api_revision: control.welcome().api_revision,
    });

    let mut state = ControlState::default();
    // What to show: the session we showed before a reconnect, else the
    // configured one.
    let wanted = worker
        .selected
        .clone()
        .or_else(|| worker.config.session.clone());
    let mut request =
        session::Bootstrap::new("all").data_tickets([session::BootstrapDataTicket::new(
            TERMINAL_CREATOR,
            "terminal",
        )
        .subprotocol(wire::PROTOCOL)]);
    if let Some(wanted) = &wanted {
        // An explicit list: a known id is attached right away; a label is
        // resolved after listing. Never let the server pick for us.
        let ids: Vec<String> = Some(wanted.clone())
            .filter(|w| is_session_id(w))
            .into_iter()
            .collect();
        request = request.attach_session_ids(ids);
    }
    let started = Instant::now();
    let reply = match tokio::time::timeout(REQUEST_TIMEOUT, control.call_with_seq(request)).await {
        Ok(Ok(reply)) => reply,
        error => {
            // Setup may be partly applied. Never reuse this identity.
            *token = None;
            let _ = tokio::time::timeout(Duration::from_secs(2), control.close()).await;
            return match error {
                Ok(Err(error)) => Err(error.into()),
                Err(error) => Err(error.into()),
                Ok(Ok(_)) => unreachable!(),
            };
        }
    };
    state.bootstrap_seq = reply.seq;
    state.list_seq = reply.seq;
    let boot = reply.value;
    let sessions = boot.attach_all.sessions;
    worker.emit(HostEvent::Sessions(sessions.clone()));
    let server_selected = boot
        .selected_session
        .as_ref()
        .map(|s| s.session.session_id.clone());
    for attached in boot
        .attached_sessions
        .into_iter()
        .chain(boot.selected_session.map(|s| session::AttachedSession {
            session: s.session,
            view: s.view,
        }))
    {
        state
            .view_seq
            .insert(attached.session.session_id.clone(), reply.seq);
        worker.apply_view(&mut state, &attached.session.session_id, attached.view);
    }
    state.add_tickets(boot.data_tickets, started);

    let selected = match &wanted {
        Some(wanted) => select_session(&sessions, Some(wanted)),
        None => server_selected.or_else(|| select_session(&sessions, None)),
    };
    if let (Some(wanted), None) = (&wanted, &selected) {
        worker.emit(HostEvent::Notice(format!(
            "No single Rex session is named {wanted:?}"
        )));
    }
    if let Some(id) = &selected {
        if !state.view_seq.contains_key(id) {
            attach(&mut control, worker, &mut state, id).await?;
        }
    }
    worker.selected = selected.clone();
    worker.emit(HostEvent::Selected(selected));

    let queued: Vec<_> = control.drain_events().collect();
    for event in queued {
        handle_event(event, &mut control, worker, &mut state).await?;
    }

    worker.emit(HostEvent::Status {
        connected: true,
        message: "Connected".into(),
    });
    *retry = 0;
    worker.told_disconnected = false;

    // Sizes asked for while disconnected (or before a reconnect). A block
    // that closed meanwhile is refused; that's not a broken connection.
    let sizes: Vec<_> = worker
        .sizes
        .iter()
        .map(|(b, s)| (b.clone(), s.clone()))
        .collect();
    for (block, (session, cols, rows, w, h)) in sizes {
        if let Err(error) = resize(&mut control, &session, &block, cols, rows, w, h).await {
            if !matches!(
                error.downcast_ref::<rexrs::Error>(),
                Some(rexrs::Error::Api(_))
            ) {
                return Err(error);
            }
            worker.sizes.remove(&block);
        }
    }

    loop {
        tokio::select! {
            event = control.next_event() => {
                handle_event(event?, &mut control, worker, &mut state).await?;
            }
            request = ticket_requests.recv_async() => {
                if let Ok(request) = request {
                    serve_ticket(&mut control, &mut state, request).await?;
                }
            }
            command = commands.recv_async() => {
                let Ok(command) = command else {
                    let _ = control.close().await;
                    return Ok(true);
                };
                match command {
                    HostCommand::Shutdown => {
                        let _ = control.close().await;
                        return Ok(true);
                    }
                    HostCommand::Reconnect => return Ok(false),
                    command => {
                        if let Err(error) = handle_command(command, &mut control, worker, &mut state).await {
                            worker.emit(HostEvent::Notice(format!("{error:#}")));
                            // An API error is the server saying no; anything
                            // else means the connection is in trouble.
                            if !matches!(error.downcast_ref::<rexrs::Error>(), Some(rexrs::Error::Api(_))) {
                                return Err(error);
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn serve_ticket(
    control: &mut Client,
    state: &mut ControlState,
    request: TicketRequest,
) -> Result<()> {
    if let Some(ticket) = state.take_ticket(&request.session_id, &request.block_id) {
        let _ = request.reply.send(Ok(ticket));
        return Ok(());
    }
    let result = request_raw_ticket(control, &request.session_id, &request.block_id).await;
    let _ = request.reply.send(
        result
            .as_ref()
            .map_err(|e| format!("{e:#}"))
            .and_then(|token| {
                token
                    .clone()
                    .ok_or_else(|| "Missing data ticket".to_string())
            })
            .map(|token| Ticket {
                token,
                bundled: false,
            }),
    );
    // A refused ticket isn't a broken connection.
    match result {
        Err(error)
            if !matches!(
                error.downcast_ref::<rexrs::Error>(),
                Some(rexrs::Error::Api(_))
            ) =>
        {
            Err(error)
        }
        _ => Ok(()),
    }
}

async fn request_raw_ticket(
    control: &mut Client,
    session: &str,
    block: &str,
) -> Result<Option<String>> {
    let reply = request(
        control,
        "data.connect",
        json!({"session_id": session, "block_id": block, "data_ref_id": "terminal", "subprotocol": wire::PROTOCOL}),
    )
    .await?;
    Ok(reply["ticket"].as_str().map(str::to_owned))
}

#[allow(clippy::too_many_arguments)]
async fn resize(
    control: &mut Client,
    session: &str,
    block: &str,
    cols: u16,
    rows: u16,
    cell_width: u16,
    cell_height: u16,
) -> Result<()> {
    // Never claims: the server applies the size now if we own the block or
    // nobody does, and otherwise once we become the owner.
    request(
        control,
        &format!("{TERMINAL_CREATOR}.resize"),
        json!({"session_id": session, "block_id": block, "args": {
            "columns": cols, "rows": rows,
            "cell_width_px": cell_width, "cell_height_px": cell_height,
        }}),
    )
    .await?;
    Ok(())
}

async fn claim(
    control: &mut Client,
    worker: &mut Worker,
    session: &str,
    blocks: &[&str],
) -> Result<()> {
    let result = request(
        control,
        "session.own_block",
        json!({"session_id": session, "block_ids": blocks, "mode": "take"}),
    )
    .await?;
    for b in result["blocks"].as_array().into_iter().flatten() {
        if let Some(id) = b["block_id"].as_str() {
            let owner = b["owner"].as_str().map(str::to_owned);
            worker.owners.insert(id.into(), owner.clone());
            worker.emit(HostEvent::OwnerChanged {
                block_id: id.into(),
                owner,
            });
        }
    }
    Ok(())
}

async fn handle_event(
    event: rexrs::Event,
    control: &mut Client,
    worker: &mut Worker,
    state: &mut ControlState,
) -> Result<()> {
    if !state.includes(&event) {
        return Ok(());
    }
    match event.name.as_str() {
        "session_created" | "session_moved" | "session_label_changed" => {
            list(control, worker, state).await?;
        }
        "session_destroyed" => {
            if let Some(id) = event.payload["session_id"]
                .as_str()
                .or(event.session_id.as_deref())
            {
                state.view_seq.remove(id);
                state.revisions.remove(id);
                worker.emit(HostEvent::SessionRemoved {
                    session_id: id.into(),
                });
                if worker.selected.as_deref() == Some(id) {
                    worker.selected = None;
                    worker.emit(HostEvent::Selected(None));
                }
            }
        }
        "session_view_changed" => {
            if let Some(id) = event.session_id.as_deref() {
                let known = state.revisions.get(id).copied();
                let newer = event.payload["revision"]
                    .as_u64()
                    .map_or(true, |new| known.map_or(true, |old| new > old));
                if state.view_seq.contains_key(id) && newer {
                    refresh(control, worker, state, id).await?;
                }
            }
        }
        "block_closed" => {
            if let Some(block) = event.payload["block_id"].as_str() {
                worker.streams.remove(block);
                worker.sizes.remove(block);
                worker.owners.remove(block);
                worker.emit(HostEvent::BlockClosed {
                    block_id: block.into(),
                });
            }
            if let Some(id) = event.session_id.as_deref() {
                if state.view_seq.contains_key(id) {
                    refresh(control, worker, state, id).await?;
                }
            }
        }
        "block_owner_changed" => {
            if let Some(block) = event.payload["block_id"].as_str() {
                let owner = event.payload["owner"].as_str().map(str::to_owned);
                worker.owners.insert(block.into(), owner.clone());
                worker.emit(HostEvent::OwnerChanged {
                    block_id: block.into(),
                    owner,
                });
            }
        }
        _ => {}
    }
    Ok(())
}

async fn handle_command(
    command: HostCommand,
    control: &mut Client,
    worker: &mut Worker,
    state: &mut ControlState,
) -> Result<()> {
    match command {
        HostCommand::Select(id) => {
            attach(control, worker, state, &id).await?;
            worker.selected = Some(id.clone());
            worker.emit(HostEvent::Selected(Some(id)));
        }
        HostCommand::CreateSession { label } => {
            let mut payload = json!({"initial_windows": [{"layout": shell_layout()}]});
            if let Some(label) = label.filter(|l| !l.trim().is_empty()) {
                payload["label"] = json!(label);
            }
            let created = request(control, "session.create", payload).await?;
            let id = created["session_id"]
                .as_str()
                .context("new session ID")?
                .to_owned();
            list(control, worker, state).await?;
            attach(control, worker, state, &id).await?;
            worker.selected = Some(id.clone());
            worker.emit(HostEvent::Selected(Some(id)));
        }
        HostCommand::Request {
            method,
            payload,
            reply,
        } => {
            let session_id = payload["session_id"].as_str().map(str::to_owned);
            let result = request(control, &method, payload).await;
            if let Some(reply) = reply {
                let _ = reply.send(
                    result
                        .as_ref()
                        .map(Value::clone)
                        .map_err(|e| format!("{e:#}")),
                );
            }
            result?;
            if let Some(id) = session_id {
                if state.view_seq.contains_key(&id) {
                    match refresh(control, worker, state, &id).await {
                        // Closing a session's last window ends it.
                        Err(error)
                            if matches!(
                                error.downcast_ref::<rexrs::Error>(),
                                Some(rexrs::Error::Api(rexrs::ApiError::SessionNotFound { .. }))
                            ) => {}
                        result => result?,
                    }
                }
            }
        }
        HostCommand::Input {
            session_id,
            block_id,
            input,
        } => {
            if input.claims() && !worker.owns(&block_id) {
                claim(control, worker, &session_id, &[block_id.as_str()]).await?;
            }
            match input {
                Input::Key(key) => {
                    request(
                        control,
                        "client.key",
                        json!({"session_id": session_id, "block_id": block_id, "key": key}),
                    )
                    .await?;
                }
                Input::Paste(text) => {
                    block_event(
                        control,
                        &session_id,
                        &block_id,
                        "paste",
                        json!({"text": text}),
                    )
                    .await?;
                }
                Input::Mouse(payload) => {
                    block_event(control, &session_id, &block_id, "mouse", payload).await?;
                }
                Input::Wheel(payload) => {
                    block_event(control, &session_id, &block_id, "wheel", payload).await?;
                }
                Input::Write(data) => {
                    request(
                        control,
                        &format!("{TERMINAL_CREATOR}.write"),
                        json!({"session_id": session_id, "block_id": block_id, "args": {"data": data}}),
                    )
                    .await?;
                }
            }
        }
        HostCommand::Resize {
            session_id,
            block_id,
            cols,
            rows,
            cell_width,
            cell_height,
        } => {
            let size = (session_id.clone(), cols, rows, cell_width, cell_height);
            if worker.sizes.get(&block_id) == Some(&size) {
                return Ok(());
            }
            worker.sizes.insert(block_id.clone(), size);
            resize(
                control,
                &session_id,
                &block_id,
                cols,
                rows,
                cell_width,
                cell_height,
            )
            .await?;
        }
        HostCommand::OpenStream {
            session_id,
            block_id,
        } => worker.open_stream(session_id, block_id),
        HostCommand::CloseStream { block_id } => {
            worker.streams.remove(&block_id);
            worker.sizes.remove(&block_id);
        }
        HostCommand::RestartStream {
            session_id,
            block_id,
        } => {
            worker.streams.remove(&block_id);
            worker.open_stream(session_id, block_id);
        }
        HostCommand::Claim {
            session_id,
            block_ids,
        } => {
            let wanted: Vec<&str> = block_ids
                .iter()
                .map(String::as_str)
                .filter(|b| !worker.owns(b))
                .collect();
            if !wanted.is_empty() {
                claim(control, worker, &session_id, &wanted).await?;
            }
        }
        HostCommand::Reconnect | HostCommand::Shutdown => unreachable!("handled by the caller"),
    }
    Ok(())
}

async fn block_event(
    control: &mut Client,
    session: &str,
    block: &str,
    kind: &str,
    payload: Value,
) -> Result<()> {
    request(
        control,
        "block.event",
        json!({"session_id": session, "block_id": block, "event": {"type": kind, "payload": payload}}),
    )
    .await?;
    Ok(())
}

/// One terminal's data stream, from first snapshot through resumes and
/// fresh restarts, until stopped or the terminal closes.
async fn run_stream(
    endpoint: String,
    session: String,
    block: String,
    tickets: flume::Sender<TicketRequest>,
    sink: EventSink,
    stop: flume::Receiver<()>,
) {
    let emit = |event: StreamEvent| {
        sink(HostEvent::Stream {
            block_id: block.clone(),
            event,
        })
    };
    let mut welcome: Option<wire::Welcome> = None;
    let mut decoder: Option<StreamDecoder> = None;

    loop {
        let result: Result<()> = async {
            let mut socket = None;
            if let (Some(previous), Some(decoder)) = (&welcome, &decoder) {
                let mut next = transport::socket(&endpoint, "/api/data", wire::PROTOCOL).await?;
                if let Ok(resumed) =
                    wire::resume(&mut next, &previous.token, decoder.received).await
                {
                    anyhow::ensure!(
                        resumed.nonce == previous.nonce,
                        "Rex changed the stream nonce during resume"
                    );
                    welcome = Some(resumed);
                    socket = Some(next);
                    emit(StreamEvent::Status {
                        live: true,
                        message: "Live · stream resumed".into(),
                    });
                }
            }
            if socket.is_none() {
                let (next, fresh) = open_data(&endpoint, &session, &block, &tickets).await?;
                decoder = Some(StreamDecoder::new(fresh.nonce));
                welcome = Some(fresh);
                socket = Some(next);
                emit(StreamEvent::Status {
                    live: false,
                    message: "Restoring terminal…".into(),
                });
            }
            let mut socket = socket.context("data socket")?;
            let decoder = decoder.as_mut().context("stream decoder")?;
            loop {
                tokio::select! {
                    bytes = wire::next_binary(&mut socket) => {
                        let bytes = bytes?;
                        let events = decoder.feed(&bytes).map_err(|error| {
                            // Undecodable: the next attempt starts fresh.
                            welcome = None;
                            error
                        })?;
                        for event in events {
                            emit(event);
                        }
                    }
                    _ = stop.recv_async() => return Ok(()),
                }
            }
        }
        .await;

        match result {
            Ok(()) => return,
            Err(error) => {
                if error
                    .downcast_ref::<wire::DataClosed>()
                    .is_some_and(|closed| closed.code == Some(4000))
                {
                    emit(StreamEvent::Closed);
                    return;
                }
                emit(StreamEvent::Status {
                    live: false,
                    message: format!("Reconnecting · {error:#}"),
                });
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                    _ = stop.recv_async() => return,
                }
            }
        }
    }
}

async fn open_data(
    endpoint: &str,
    session: &str,
    block: &str,
    tickets: &flume::Sender<TicketRequest>,
) -> Result<(transport::Socket, wire::Welcome)> {
    loop {
        let (reply, ticket) = tokio::sync::oneshot::channel();
        tickets
            .send(TicketRequest {
                session_id: session.into(),
                block_id: block.into(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("Rex host worker stopped"))?;
        let ticket = ticket.await?.map_err(anyhow::Error::msg)?;
        let mut next = transport::socket(endpoint, "/api/data", wire::PROTOCOL).await?;
        match wire::handshake(&mut next, &ticket.token).await {
            Ok(welcome) => return Ok((next, welcome)),
            // A pooled ticket may have expired on the server; ask for a
            // fresh one through data.connect.
            Err(error) if ticket.bundled && error.is::<wire::HandshakeRejected>() => {}
            Err(error) => return Err(error),
        }
    }
}

/// Turns a stream's bytes into events: the snapshot first, then output and
/// metadata.
pub struct StreamDecoder {
    snapshot: Option<SnapshotBuffer>,
    parser: ApcParser,
    /// Bytes received since the handshake, for resuming.
    pub received: u64,
}

impl StreamDecoder {
    pub fn new(nonce: u64) -> Self {
        Self {
            snapshot: Some(SnapshotBuffer::default()),
            parser: ApcParser::new(nonce),
            received: 0,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<StreamEvent>> {
        self.received += bytes.len() as u64;
        let mut events = vec![];
        let tail;
        let bytes = match self.snapshot.as_mut() {
            Some(buffer) => {
                let Some((snapshot, rest)) = buffer.push(bytes)? else {
                    return Ok(events);
                };
                self.snapshot = None;
                events.push(StreamEvent::Snapshot(snapshot));
                tail = rest;
                &tail[..]
            }
            None => bytes,
        };
        for part in self.parser.feed(bytes)? {
            match part {
                Part::Output(bytes) => events.push(StreamEvent::Output(bytes)),
                Part::Metadata(value) => {
                    if let Some(event) = metadata_event(&value)? {
                        events.push(event);
                    }
                }
            }
        }
        Ok(events)
    }
}

fn metadata_event(value: &Value) -> Result<Option<StreamEvent>> {
    Ok(match value["type"].as_str() {
        Some("resize") => {
            let dim = |key: &str| -> Result<u16> {
                Ok(value[key]
                    .as_u64()
                    .filter(|v| (1..=65535).contains(v))
                    .with_context(|| format!("invalid resize {key}"))? as u16)
            };
            Some(StreamEvent::Resize {
                cols: dim("cols")?,
                rows: dim("rows")?,
                cell_width: value["cell_width_px"].as_u64().unwrap_or(0) as u32,
                cell_height: value["cell_height_px"].as_u64().unwrap_or(0) as u32,
            })
        }
        Some("theme") => Some(StreamEvent::Theme(Theme::from_metadata(value))),
        Some("child_exited") => Some(StreamEvent::Exited(value["exit_code"].as_i64())),
        _ => None,
    })
}

/// Which blocks of a view are terminals this client can stream.
pub fn terminal_blocks(view: &session::ViewResponse) -> HashSet<String> {
    view.windows
        .iter()
        .flat_map(|w| &w.layers)
        .flat_map(|l| &l.blocks)
        .filter(|b| b.creator_name.as_deref() == Some(TERMINAL_CREATOR))
        .filter_map(|b| b.block_id.clone())
        .collect()
}

#[cfg(test)]
mod test {
    use super::*;

    fn sessions() -> Vec<session::Session> {
        serde_json::from_value(json!([
            {"session_id": "session:a", "label": "work"},
            {"session_id": "session:b", "label": "play"},
            {"session_id": "session:c", "label": "play"},
        ]))
        .unwrap()
    }

    #[test]
    fn session_selection_is_exact_and_never_ambiguous() {
        let s = sessions();
        assert_eq!(select_session(&s, None).as_deref(), Some("session:a"));
        assert_eq!(
            select_session(&s, Some("work")).as_deref(),
            Some("session:a")
        );
        assert_eq!(
            select_session(&s, Some("session:b")).as_deref(),
            Some("session:b")
        );
        assert_eq!(select_session(&s, Some("play")), None, "ambiguous");
        assert_eq!(select_session(&s, Some("wor")), None, "no prefixes");
    }

    #[test]
    fn stream_decoder_emits_snapshot_then_output_and_metadata() {
        let mut snap = b"GHOSTSNP\x01\x00".to_vec();
        snap.extend(6u16.to_le_bytes());
        snap.extend(0u32.to_le_bytes());
        snap.extend([0u8; 4]);
        let mut d = StreamDecoder::new(9);
        let mut bytes = snap.clone();
        bytes.extend(b"hi\x1b_rex1;9;{\"type\":\"child_exited\",\"exit_code\":3}\x1b\\");
        let events = d.feed(&bytes).unwrap();
        assert!(matches!(&events[0], StreamEvent::Snapshot(s) if *s == snap));
        assert!(matches!(&events[1], StreamEvent::Output(o) if o == b"hi"));
        assert!(matches!(events[2], StreamEvent::Exited(Some(3))));
        assert_eq!(d.received, bytes.len() as u64);
    }

    #[test]
    fn theme_metadata() {
        let theme = Theme::from_metadata(&json!({
            "type": "theme", "foreground": "#ffffff", "background": "#000000",
            "palette": ["1=#ff0000", "bogus", "300=#00ff00"],
        }));
        assert_eq!(theme.foreground, Some((255, 255, 255)));
        assert_eq!(theme.cursor, None);
        assert_eq!(theme.palette, vec![(1, (255, 0, 0))]);
    }

    #[test]
    fn invalid_resize_is_an_error() {
        assert!(metadata_event(&json!({"type": "resize", "cols": 0, "rows": 5})).is_err());
        assert!(matches!(
            metadata_event(&json!({"type": "resize", "cols": 80, "rows": 24})).unwrap(),
            Some(StreamEvent::Resize {
                cols: 80,
                rows: 24,
                ..
            })
        ));
    }

    #[test]
    fn presses_claim_but_releases_do_not() {
        assert!(Input::Key(json!({"action": "press"})).claims());
        assert!(!Input::Key(json!({"action": "release"})).claims());
        assert!(!Input::Mouse(json!({"action": "motion"})).claims());
        assert!(!Input::Wheel(json!({})).claims());
        assert!(Input::Paste("x".into()).claims());
    }
}
