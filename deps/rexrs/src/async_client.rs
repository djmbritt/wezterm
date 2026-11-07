//! The async client.

use serde::Serialize;
use serde_json::Value;

use crate::api::Method;
use crate::client::{Async, ClientBuilder};
use crate::error::{Error, Result};
use crate::event::Event;
use crate::protocol::{Core, Reply, Welcome, WireResponse};
use crate::transport::{AsyncTransport, Frame};

impl ClientBuilder<Async> {
    /// Connects to the server over the built-in [`TokioTransport`](crate::transport::TokioTransport).
    #[cfg(feature = "tokio")]
    pub async fn connect(self) -> Result<AsyncRexClient<crate::transport::TokioTransport>> {
        let transport =
            crate::transport::TokioTransport::connect(&self.resolve_endpoint()?).await?;
        self.connect_with(transport).await
    }

    /// Starts the control protocol over a transport that is already connected to the server's
    /// control endpoint.
    pub async fn connect_with<T: AsyncTransport>(
        self,
        mut transport: T,
    ) -> Result<AsyncRexClient<T>> {
        transport.send(self.hello()?).await?;
        let core = match transport.recv().await? {
            Frame::Text(text) => Core::accept(&text)?,
            Frame::Closed { code, reason } => return Err(Error::Closed { code, reason }),
        };
        Ok(AsyncRexClient { transport, core })
    }
}

/// An async client for a Rex server. It works with any runtime that its transport works with.
///
/// ```no_run
/// # #[cfg(feature = "tokio")]
/// # async fn run() -> rexrs::Result<()> {
/// use rexrs::AsyncRexClient;
/// use rexrs::api::server;
///
/// let mut client = AsyncRexClient::builder().name("my-tool").connect().await?;
/// let version = client.call(server::Version::new()).await?;
/// println!("Rex {}", version.rex_version);
/// client.close().await?;
/// # Ok(())
/// # }
/// ```
///
/// A client sends one request at a time and waits for its response. Events that arrive in the
/// meantime are kept in order for [`next_event`](Self::next_event). If a call's future is dropped
/// before it finishes, the server may still run the request; its late response is skipped.
pub struct AsyncRexClient<T> {
    transport: T,
    core: Core,
}

impl AsyncRexClient<()> {
    /// Starts building an async client.
    pub fn builder() -> ClientBuilder<Async> {
        ClientBuilder::new()
    }
}

impl<T: AsyncTransport> AsyncRexClient<T> {
    /// Calls a method and returns its result.
    pub async fn call<M: Method>(&mut self, request: M) -> Result<M::Response> {
        Ok(self.call_with_seq(request).await?.value)
    }

    /// Calls a method and returns its result with the `seq` that orders it among events.
    pub async fn call_with_seq<M: Method>(&mut self, request: M) -> Result<Reply<M::Response>> {
        let (id, text) = self.core.encode(&request)?;
        self.transport.send(text).await?;
        self.wait(&id).await?.into_reply()
    }

    /// Calls a method by name with untyped arguments, such as one newer than this crate.
    pub async fn call_raw(&mut self, method: &str, payload: impl Serialize) -> Result<Value> {
        let (id, text) = self.core.request(method, &payload)?;
        self.transport.send(text).await?;
        Ok(self.wait(&id).await?.into_reply::<Value>()?.value)
    }

    /// Returns the next event, and waits for one if none has arrived yet.
    pub async fn next_event(&mut self) -> Result<Event> {
        loop {
            if let Some(event) = self.core.pop_event() {
                return Ok(event);
            }
            self.read(None).await?;
        }
    }

    /// Takes the events that have already arrived, without waiting.
    pub fn drain_events(&mut self) -> impl Iterator<Item = Event> + '_ {
        self.core.drain_events()
    }

    /// The server's welcome for this connection.
    pub fn welcome(&self) -> &Welcome {
        self.core.welcome()
    }

    /// The transport.
    pub fn transport(&mut self) -> &mut T {
        &mut self.transport
    }

    /// Closes the connection. The server forgets the client at once, and ends its attachments.
    ///
    /// Dropping the client without closing it leaves the server keeping the client for about a
    /// minute, so that a new connection can continue it with
    /// [`ClientBuilder::reconnect_token`].
    pub async fn close(mut self) -> Result<()> {
        self.transport.close().await
    }

    async fn wait(&mut self, id: &str) -> Result<WireResponse> {
        loop {
            if let Some(response) = self.read(Some(id)).await? {
                return Ok(response);
            }
        }
    }

    async fn read(&mut self, waiting: Option<&str>) -> Result<Option<WireResponse>> {
        match self.transport.recv().await? {
            Frame::Text(text) => self.core.receive(&text, waiting),
            Frame::Closed { code, reason } => Err(Error::Closed { code, reason }),
        }
    }
}
