//! A typed client for the [Rex](https://www.superlogical.com/rex) server's control API.
//!
//! A Rex server runs sessions of terminals. Clients connect to its control endpoint, a
//! WebSocket, to call methods such as `session.create` and to receive events. This crate gives
//! every method a typed request and response in [`api`], and two clients that send them:
//!
//! - [`RexClient`], a blocking client, over any [`Transport`].
//! - [`AsyncRexClient`], an async client, over any [`AsyncTransport`].
//!
//! The `tungstenite` feature (on by default) adds a blocking transport, and the `tokio` feature
//! adds an async one. To use another WebSocket library, implement the transport traits for it.
//!
//! ```no_run
//! use rexrs::RexClient;
//! use rexrs::api::{Layout, session, terminal::Write, BlockMethod};
//!
//! let mut client = RexClient::builder().name("my-tool").connect()?;
//!
//! let created = client.call(
//!     session::Create::new()
//!         .label("work")
//!         .initial_windows([session::CreateInitialWindow::new().layout(Layout::shell())]),
//! )?;
//! let session_id = created.session_id;
//! let block_id = created.initial_windows[0].block_ids[0].clone();
//!
//! // A connection must attach to a session before it can use it.
//! client.call(session::Attach::new(&session_id))?;
//! client.call(Write::new("ls\n").on(&session_id, &block_id))?;
//!
//! // Errors from the server map to `ApiError` variants.
//! match client.call(session::Destroy::new("session:missing")) {
//!     Err(rexrs::Error::Api(rexrs::ApiError::SessionNotFound { .. })) => {}
//!     other => panic!("unexpected {other:?}"),
//! }
//! # Ok::<(), rexrs::Error>(())
//! ```

pub mod api;
mod async_client;
mod client;
pub mod endpoint;
mod error;
mod event;
mod protocol;
pub mod transport;

pub use async_client::AsyncRexClient;
pub use client::{Async, Blocking, ClientBuilder, RexClient};
pub use endpoint::Endpoint;
pub use error::{ApiError, Error, Result};
pub use event::Event;
pub use protocol::{
    CONTROL_PATH, CONTROL_SUBPROTOCOL, ClientInfo, ClientKind, Limits, Reply, Welcome,
    WelcomeClient,
};
pub use transport::{AsyncTransport, Frame, Transport};
