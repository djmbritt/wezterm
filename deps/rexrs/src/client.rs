//! The client builder and the blocking client.

use std::marker::PhantomData;

use serde::Serialize;
use serde_json::Value;

use crate::api::Method;
use crate::endpoint::Endpoint;
use crate::error::{Error, Result};
use crate::event::Event;
use crate::protocol::{ClientInfo, ClientKind, Core, Reply, Welcome, WireResponse};
use crate::transport::{Frame, Transport};

/// Marks a [`ClientBuilder`] that builds a blocking [`RexClient`].
#[derive(Debug, Clone, Copy)]
pub struct Blocking;

/// Marks a [`ClientBuilder`] that builds an [`AsyncRexClient`](crate::AsyncRexClient).
#[derive(Debug, Clone, Copy)]
pub struct Async;

/// Settings for a new client. Start one with [`RexClient::builder`] or
/// [`AsyncRexClient::builder`](crate::AsyncRexClient::builder).
#[derive(Debug, Clone)]
pub struct ClientBuilder<M> {
    pub(crate) info: ClientInfo,
    pub(crate) reconnect_token: Option<String>,
    pub(crate) endpoint: Option<Endpoint>,
    api_revisions: std::ops::RangeInclusive<u32>,
    mode: PhantomData<M>,
}

impl<M> ClientBuilder<M> {
    pub(crate) fn new() -> Self {
        ClientBuilder {
            info: ClientInfo::default(),
            reconnect_token: None,
            endpoint: None,
            api_revisions: crate::api::API_REVISION..=crate::api::API_REVISION,
            mode: PhantomData,
        }
    }

    /// Sets the program's name, which the server shows to people. Defaults to `rexrs`.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.info.name = Some(name.into());
        self
    }

    /// Sets the program's version. Defaults to this crate's version.
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.info.version = Some(version.into());
        self
    }

    /// Sets what sort of program the client is.
    pub fn kind(mut self, kind: ClientKind) -> Self {
        self.info.kind = Some(kind);
        self
    }

    /// Sets the platform the client runs on. Defaults to the one this crate was built for.
    pub fn platform(mut self, platform: impl Into<String>) -> Self {
        self.info.platform = Some(platform.into());
        self
    }

    /// Sets the client's label.
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.info.label = Some(label.into());
        self
    }

    /// Replaces everything the client tells the server about itself.
    pub fn info(mut self, info: ClientInfo) -> Self {
        self.info = info;
        self
    }

    /// Continues an earlier client, with the token from its last welcome. If the server no
    /// longer knows the token, it starts a new client; check [`Welcome::client`].
    pub fn reconnect_token(mut self, token: impl Into<String>) -> Self {
        self.reconnect_token = Some(token.into());
        self
    }

    /// Sets the supported control API range. Defaults to the generated revision only.
    /// The caller must gate revision-specific requests on the negotiated welcome.
    pub fn api_revisions(mut self, revisions: std::ops::RangeInclusive<u32>) -> Self {
        self.api_revisions = revisions;
        self
    }

    /// Sets the server to connect to. Defaults to [`Endpoint::discover`]: `REX_SERVER`, or the
    /// local server's socket. Only the built-in transports use it.
    pub fn endpoint(mut self, endpoint: Endpoint) -> Self {
        self.endpoint = Some(endpoint);
        self
    }

    #[cfg_attr(not(any(feature = "tungstenite", feature = "tokio")), allow(dead_code))]
    pub(crate) fn resolve_endpoint(&self) -> Result<Endpoint> {
        match &self.endpoint {
            Some(endpoint) => Ok(endpoint.clone()),
            None => Endpoint::discover(),
        }
    }

    pub(crate) fn hello(&self) -> Result<String> {
        Core::hello(
            &self.info,
            self.reconnect_token.as_deref(),
            &self.api_revisions,
        )
    }
}

impl ClientBuilder<Blocking> {
    /// Connects to the server over the built-in [`TungsteniteTransport`](crate::transport::TungsteniteTransport).
    #[cfg(feature = "tungstenite")]
    pub fn connect(self) -> Result<RexClient<crate::transport::TungsteniteTransport>> {
        let transport = crate::transport::TungsteniteTransport::connect(&self.resolve_endpoint()?)?;
        self.connect_with(transport)
    }

    /// Starts the control protocol over a transport that is already connected to the server's
    /// control endpoint.
    pub fn connect_with<T: Transport>(self, mut transport: T) -> Result<RexClient<T>> {
        transport.send(self.hello()?)?;
        let core = match transport.recv()? {
            Frame::Text(text) => Core::accept(&text)?,
            Frame::Closed { code, reason } => return Err(Error::Closed { code, reason }),
        };
        Ok(RexClient { transport, core })
    }
}

/// A blocking client for a Rex server.
///
/// ```no_run
/// use rexrs::RexClient;
/// use rexrs::api::server;
///
/// let mut client = RexClient::builder().name("my-tool").connect()?;
/// let version = client.call(server::Version::new())?;
/// println!("Rex {}", version.rex_version);
/// client.close()?;
/// # Ok::<(), rexrs::Error>(())
/// ```
///
/// A client sends one request at a time and waits for its response. Events that arrive in the
/// meantime are kept in order for [`next_event`](Self::next_event).
pub struct RexClient<T> {
    transport: T,
    core: Core,
}

impl RexClient<()> {
    /// Starts building a blocking client.
    pub fn builder() -> ClientBuilder<Blocking> {
        ClientBuilder::new()
    }
}

impl<T: Transport> RexClient<T> {
    /// Calls a method and returns its result.
    pub fn call<M: Method>(&mut self, request: M) -> Result<M::Response> {
        Ok(self.call_with_seq(request)?.value)
    }

    /// Calls a method and returns its result with the `seq` that orders it among events.
    pub fn call_with_seq<M: Method>(&mut self, request: M) -> Result<Reply<M::Response>> {
        let (id, text) = self.core.encode(&request)?;
        self.transport.send(text)?;
        self.wait(&id)?.into_reply()
    }

    /// Calls a method by name with untyped arguments, such as one newer than this crate.
    pub fn call_raw(&mut self, method: &str, payload: impl Serialize) -> Result<Value> {
        let (id, text) = self.core.request(method, &payload)?;
        self.transport.send(text)?;
        Ok(self.wait(&id)?.into_reply::<Value>()?.value)
    }

    /// Returns the next event, and waits for one if none has arrived yet.
    pub fn next_event(&mut self) -> Result<Event> {
        loop {
            if let Some(event) = self.core.pop_event() {
                return Ok(event);
            }
            self.read(None)?;
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

    /// The transport, such as to set a read timeout.
    pub fn transport(&mut self) -> &mut T {
        &mut self.transport
    }

    /// Closes the connection. The server forgets the client at once, and ends its attachments.
    ///
    /// Dropping the client without closing it leaves the server keeping the client for about a
    /// minute, so that a new connection can continue it with [`ClientBuilder::reconnect_token`].
    pub fn close(mut self) -> Result<()> {
        self.transport.close()
    }

    fn wait(&mut self, id: &str) -> Result<WireResponse> {
        loop {
            if let Some(response) = self.read(Some(id))? {
                return Ok(response);
            }
        }
    }

    fn read(&mut self, waiting: Option<&str>) -> Result<Option<WireResponse>> {
        match self.transport.recv()? {
            Frame::Text(text) => self.core.receive(&text, waiting),
            Frame::Closed { code, reason } => Err(Error::Closed { code, reason }),
        }
    }
}
