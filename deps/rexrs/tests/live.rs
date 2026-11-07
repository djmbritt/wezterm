//! Tests against a running Rex server, over the blocking client.
//!
//! Rex is in private beta and can't be installed on CI machines, so these tests are ignored by
//! default. Run them on a machine with a Rex server:
//!
//! ```sh
//! cargo test --all-features -- --ignored
//! ```
//!
//! Each test makes its own session and destroys it, even when it fails.

use std::time::{Duration, Instant};

use rexrs::api::{self, BlockMethod, Direction, Layout, Side, session, terminal};
use rexrs::{ApiError, ClientKind, Error, RexClient, Transport};
use serde_json::json;

type Client = RexClient<rexrs::transport::TungsteniteTransport>;

fn connect() -> Client {
    RexClient::builder()
        .name("rexrs-tests")
        .kind(ClientKind::Other)
        .connect()
        .expect("connect to the local Rex server")
}

fn unique(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}-{}-{nanos}", std::process::id())
}

/// A session that is destroyed when the test ends.
struct TestSession {
    session_id: String,
    block_id: String,
}

impl TestSession {
    fn create(client: &mut Client) -> Self {
        let created = client
            .call(
                session::Create::new()
                    .label(unique("rexrs-test"))
                    .initial_windows([session::CreateInitialWindow::new().layout(Layout::shell())]),
            )
            .unwrap();
        let block_id = created.initial_windows[0].block_ids[0].clone();
        TestSession {
            session_id: created.session_id,
            block_id,
        }
    }
}

impl Drop for TestSession {
    fn drop(&mut self) {
        // A new connection must attach to the session before it can destroy it.
        let result = (|| {
            let mut client = RexClient::builder().name("rexrs-tests-cleanup").connect()?;
            client.call(session::Attach::new(&self.session_id))?;
            client.call(session::Destroy::new(&self.session_id))?;
            client.close()
        })();
        // Don't panic while a failed test unwinds, which would abort the test run.
        if !std::thread::panicking() {
            result.expect("destroy the test session");
        }
    }
}

fn wait_for_screen(client: &mut Client, session: &TestSession, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let screen = client
            .call(
                terminal::Format::new()
                    .trim(true)
                    .on(&session.session_id, &session.block_id),
            )
            .unwrap();
        if screen.content.contains(needle) {
            return screen.content;
        }
        assert!(
            Instant::now() < deadline,
            "{needle:?} never appeared on screen:\n{}",
            screen.content
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
#[ignore = "needs a running Rex server"]
fn handshake_negotiates_revision() {
    let mut client = connect();
    let welcome = client.welcome();
    assert_eq!(welcome.protocol, rexrs::CONTROL_SUBPROTOCOL);
    assert_eq!(welcome.api_revision, api::API_REVISION);
    assert!(welcome.limits.max_message_bytes >= 65536);
    assert!(
        welcome
            .client
            .as_ref()
            .is_some_and(|c| c.id.starts_with("client:"))
    );

    let version = client.call(api::server::Version::new()).unwrap();
    assert_eq!(version.api_revision, Some(u64::from(api::API_REVISION)));
    client.close().unwrap();
}

/// Every read-only method that needs no IDs decodes into its generated response type.
#[test]
#[ignore = "needs a running Rex server"]
fn read_only_methods_decode() {
    let mut client = connect();
    client.call(api::server::Status::new()).unwrap();
    client.call(api::server::Whoami::new()).unwrap();
    client.call(api::server::ApplePrivacyStatus::new()).unwrap();
    client.call(api::session::List::new()).unwrap();
    client.call(api::client::List::new()).unwrap();
    client.call(api::client::Keymap::new()).unwrap();
    client.call(api::hosts::List::new()).unwrap();
    client.call(api::block_creator::List::new()).unwrap();
    client.call(api::help::All::new()).unwrap();
    client
        .call(api::target::Complete::new(["session"], ""))
        .unwrap();
    client.close().unwrap();
}

#[test]
#[ignore = "needs a running Rex server"]
fn terminal_round_trip() {
    let mut client = connect();
    let session = TestSession::create(&mut client);
    client
        .call(session::Attach::new(&session.session_id))
        .unwrap();

    let marker = unique("rexrs-marker");
    let written = client
        .call(
            terminal::Write::new(format!("echo {marker}\n"))
                .on(&session.session_id, &session.block_id),
        )
        .unwrap();
    assert!(written.bytes_written > 0);
    wait_for_screen(&mut client, &session, &marker);

    // Block methods that take no arguments decode too.
    client
        .call(terminal::Size::new().on(&session.session_id, &session.block_id))
        .unwrap();
    client
        .call(terminal::Process::new().on(&session.session_id, &session.block_id))
        .unwrap();
    client
        .call(terminal::Modes::new().on(&session.session_id, &session.block_id))
        .unwrap();
    client.close().unwrap();
}

#[test]
#[ignore = "needs a running Rex server"]
fn layout_changes_show_in_view() {
    let mut client = connect();
    let session = TestSession::create(&mut client);
    client
        .call(session::Attach::new(&session.session_id))
        .unwrap();

    let split = client
        .call(
            session::NewSplit::new(
                &session.session_id,
                Direction::Vertical,
                Side::After,
                Layout::split(
                    Direction::Horizontal,
                    Layout::shell().label("left"),
                    Layout::shell(),
                )
                .ratio(0.25),
            )
            .anchor_id(&session.block_id),
        )
        .unwrap();
    assert_eq!(split.block_ids.len(), 2);

    let view = client
        .call(session::View::new(&session.session_id))
        .unwrap();
    assert_eq!(view.windows.len(), 1);
    let blocks = client
        .call(session::ListBlocks::new(&session.session_id))
        .unwrap()
        .blocks;
    assert_eq!(blocks.len(), 3);
    assert!(blocks.iter().any(|block| block.label == "left"));

    let revision = client
        .call(
            session::SetBlockLabel::new(&session.session_id)
                .block_id(&session.block_id)
                .label("main"),
        )
        .unwrap()
        .revision;
    assert!(revision > view.revision);
    client.close().unwrap();
}

#[test]
#[ignore = "needs a running Rex server"]
fn attached_sessions_send_events() {
    let mut client = connect();
    let session = TestSession::create(&mut client);
    let attached = client
        .call_with_seq(session::Attach::new(&session.session_id))
        .unwrap();

    let label = unique("rexrs-test-renamed");
    client
        .call(session::SetLabel::new(&session.session_id).label(&label))
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    client
        .transport()
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    loop {
        let event = client.next_event().unwrap();
        assert!(event.seq > attached.seq || event.seq == 0);
        if event.name == "session_label_changed" {
            assert_eq!(
                event.session_id.as_deref(),
                Some(session.session_id.as_str())
            );
            assert_eq!(event.payload["label"], label.as_str());
            break;
        }
        assert!(Instant::now() < deadline, "no session_label_changed event");
    }
    client.close().unwrap();
}

#[test]
#[ignore = "needs a running Rex server"]
fn read_timeout_leaves_connection_usable() {
    let mut client = connect();
    client
        .transport()
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    assert!(matches!(client.next_event(), Err(Error::TimedOut)));
    client.call(api::server::Version::new()).unwrap();
    client.close().unwrap();
}

#[test]
#[ignore = "needs a running Rex server"]
fn server_errors_map_to_variants() {
    let mut client = connect();

    let missing = client.call(session::Destroy::new(
        "session:00000000-0000-0000-0000-000000000000",
    ));
    assert!(
        matches!(missing, Err(Error::Api(ApiError::SessionNotFound { .. }))),
        "{missing:?}"
    );

    let unknown = client.call_raw("rexrs.no_such_method", json!({}));
    assert!(
        matches!(unknown, Err(Error::Api(ApiError::MethodNotFound { .. }))),
        "{unknown:?}"
    );

    let bad = client.call_raw("session.create", json!({"label": 5}));
    assert!(
        matches!(bad, Err(Error::Api(ApiError::BadRequest { .. }))),
        "{bad:?}"
    );

    // A connection can't use a session it hasn't attached to.
    let session = TestSession::create(&mut client);
    let unattached = client.call(session::View::new(&session.session_id));
    assert!(
        matches!(unattached, Err(Error::Api(ApiError::Conflict { .. }))),
        "{unattached:?}"
    );

    client
        .call(session::Attach::new(&session.session_id))
        .unwrap();
    // The server reads the nil UUID as a missing ID, so name a block that just doesn't exist.
    let no_block = client.call(terminal::Pwd::new().on(
        &session.session_id,
        "block:01a18300-1111-7222-8333-444455556666",
    ));
    assert!(
        matches!(no_block, Err(Error::Api(ApiError::NotFound { .. }))),
        "{no_block:?}"
    );
    assert_eq!(no_block.unwrap_err().api().map(ApiError::status), Some(404));
    client.close().unwrap();
}

#[test]
#[ignore = "needs a running Rex server"]
fn reconnect_token_resumes_client() {
    let mut client = connect();
    let session = TestSession::create(&mut client);
    // An attached client is kept for a while after its connection drops.
    client
        .call(session::Attach::new(&session.session_id))
        .unwrap();
    let first = client.welcome().client.clone().unwrap();
    // Drop the socket without a close frame, as a lost network would.
    drop(client);

    let mut client = RexClient::builder()
        .reconnect_token(&first.reconnect_token)
        .connect()
        .unwrap();
    let second = client.welcome().client.clone().unwrap();
    assert!(second.resumed);
    assert_eq!(second.id, first.id);
    assert_ne!(second.reconnect_token, first.reconnect_token);
    // The resumed client is still attached.
    client
        .call(session::View::new(&session.session_id))
        .unwrap();
    client.close().unwrap();
}

#[test]
#[ignore = "needs a running Rex server"]
fn custom_transport_wraps_builtin() {
    /// A transport that counts the messages it sends.
    struct Counting<T> {
        inner: T,
        sent: usize,
    }

    impl<T: Transport> Transport for Counting<T> {
        fn send(&mut self, text: String) -> rexrs::Result<()> {
            self.sent += 1;
            self.inner.send(text)
        }
        fn recv(&mut self) -> rexrs::Result<rexrs::Frame> {
            self.inner.recv()
        }
        fn close(&mut self) -> rexrs::Result<()> {
            self.inner.close()
        }
    }

    let endpoint = rexrs::Endpoint::discover().unwrap();
    let inner = rexrs::transport::TungsteniteTransport::connect(&endpoint).unwrap();
    let mut client = RexClient::builder()
        .connect_with(Counting { inner, sent: 0 })
        .unwrap();
    client.call(api::server::Version::new()).unwrap();
    assert_eq!(client.transport().sent, 2, "a hello and one request");
    client.close().unwrap();
}
