//! Transports: the WebSocket connections that carry the control protocol.
//!
//! The control connection is a WebSocket at [`CONTROL_PATH`](crate::CONTROL_PATH) that asks for
//! the [`CONTROL_SUBPROTOCOL`](crate::CONTROL_SUBPROTOCOL) subprotocol. Every message, both
//! ways, is one JSON object in one text message. A transport moves those messages and nothing
//! else: the client does the hello, the requests, and the events.
//!
//! Implement [`Transport`] for a blocking WebSocket client, or [`AsyncTransport`] for an async
//! one. This crate has one of each, behind features:
//!
//! | Feature | Transport | Client |
//! | --- | --- | --- |
//! | `tungstenite` (default) | [`TungsteniteTransport`] | [`RexClient`](crate::RexClient) |
//! | `tokio` | [`TokioTransport`] | [`AsyncRexClient`](crate::AsyncRexClient) |
//!
//! A transport must answer the server's pings, or the server closes the connection after about
//! a minute without answers. Most WebSocket libraries answer pings while they read, so a client
//! that sits idle for long periods should keep reading, such as with
//! [`RexClient::next_event`](crate::RexClient::next_event).

use std::future::Future;

use crate::error::Result;

#[cfg(feature = "tokio")]
mod tokio;
#[cfg(feature = "tungstenite")]
mod tungstenite;

#[cfg(feature = "tokio")]
pub use self::tokio::{TokioStream, TokioTransport};
#[cfg(feature = "tungstenite")]
pub use self::tungstenite::{Stream, TungsteniteTransport};

/// A message that a transport received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// A text message.
    Text(String),
    /// The connection closed.
    Closed {
        /// The WebSocket close code, if the peer sent a close frame.
        code: Option<u16>,
        /// The close reason, if any.
        reason: String,
    },
}

/// A blocking WebSocket connection to the server's control endpoint.
pub trait Transport {
    /// Sends one text message.
    fn send(&mut self, text: String) -> Result<()>;

    /// Waits for the next text message, or for the connection to close.
    ///
    /// Skip pings, pongs, and other non-text frames; the server sends no binary messages on the
    /// control connection.
    fn recv(&mut self) -> Result<Frame>;

    /// Closes the connection with a close frame. The server then forgets the client at once.
    fn close(&mut self) -> Result<()>;
}

/// An async WebSocket connection to the server's control endpoint.
///
/// The futures must be `Send`, so that clients can be used from multi-threaded runtimes.
pub trait AsyncTransport: Send {
    /// Sends one text message.
    fn send(&mut self, text: String) -> impl Future<Output = Result<()>> + Send;

    /// Waits for the next text message, or for the connection to close.
    ///
    /// Skip pings, pongs, and other non-text frames; the server sends no binary messages on the
    /// control connection.
    fn recv(&mut self) -> impl Future<Output = Result<Frame>> + Send;

    /// Closes the connection with a close frame. The server then forgets the client at once.
    fn close(&mut self) -> impl Future<Output = Result<()>> + Send;
}
