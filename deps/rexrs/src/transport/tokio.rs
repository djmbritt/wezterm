//! An async transport built on `tokio-tungstenite`.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{self, Message};

use super::{AsyncTransport, Frame};
use crate::endpoint::Endpoint;
use crate::error::{Error, Result};
use crate::protocol::{CONTROL_PATH, CONTROL_SUBPROTOCOL};

/// An async control connection over `tokio-tungstenite`.
pub struct TokioTransport<S = TokioStream> {
    socket: WebSocketStream<S>,
}

impl TokioTransport<TokioStream> {
    /// Opens a control connection to `endpoint`.
    pub async fn connect(endpoint: &Endpoint) -> Result<Self> {
        let stream = match endpoint {
            #[cfg(unix)]
            Endpoint::Unix(path) => {
                TokioStream::Unix(UnixStream::connect(path).await.map_err(|error| {
                    Error::transport(io::Error::new(
                        error.kind(),
                        format!("{}: {error}", path.display()),
                    ))
                })?)
            }
            #[cfg(not(unix))]
            Endpoint::Unix(_) => {
                return Err(Error::InvalidEndpoint(
                    "Unix sockets need a Unix platform".into(),
                ));
            }
            Endpoint::Tcp { host, port } => {
                let stream = TcpStream::connect((host.as_str(), *port))
                    .await
                    .map_err(Error::transport)?;
                stream.set_nodelay(true).map_err(Error::transport)?;
                TokioStream::Tcp(stream)
            }
        };
        Self::handshake(stream, &endpoint.ws_url(CONTROL_PATH)).await
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> TokioTransport<S> {
    /// Opens a control connection over a stream that is already connected, such as a TLS
    /// stream. `url` is the `ws://` or `wss://` URL of the control endpoint.
    pub async fn handshake(stream: S, url: &str) -> Result<Self> {
        let mut request = url.into_client_request().map_err(Error::transport)?;
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_static(CONTROL_SUBPROTOCOL),
        );
        let (socket, _response) = tokio_tungstenite::client_async(request, stream)
            .await
            .map_err(|error| Error::transport(format!("WebSocket handshake failed: {error}")))?;
        Ok(TokioTransport { socket })
    }

    /// The underlying WebSocket.
    pub fn socket(&mut self) -> &mut WebSocketStream<S> {
        &mut self.socket
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> AsyncTransport for TokioTransport<S> {
    async fn send(&mut self, text: String) -> Result<()> {
        self.socket
            .send(Message::text(text))
            .await
            .map_err(Error::transport)
    }

    async fn recv(&mut self) -> Result<Frame> {
        loop {
            match self.socket.next().await {
                Some(Ok(Message::Text(text))) => return Ok(Frame::Text(text.as_str().to_owned())),
                Some(Ok(Message::Close(frame))) => {
                    return Ok(match frame {
                        Some(frame) => Frame::Closed {
                            code: Some(frame.code.into()),
                            reason: frame.reason.as_str().to_owned(),
                        },
                        None => Frame::Closed {
                            code: None,
                            reason: String::new(),
                        },
                    });
                }
                Some(Ok(_)) => continue,
                None
                | Some(Err(
                    tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed,
                )) => {
                    return Ok(Frame::Closed {
                        code: None,
                        reason: String::new(),
                    });
                }
                Some(Err(error)) => return Err(Error::transport(error)),
            }
        }
    }

    async fn close(&mut self) -> Result<()> {
        match self.socket.close(None).await {
            Ok(())
            | Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {}
            Err(error) => return Err(Error::transport(error)),
        }
        // Wait for the server's close frame, so that the close handshake completes.
        while let Some(message) = self.socket.next().await {
            match message {
                Ok(_) => continue,
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    break;
                }
                Err(error) => return Err(Error::transport(error)),
            }
        }
        Ok(())
    }
}

/// A stream that a [`TokioTransport`] can connect over.
#[derive(Debug)]
#[non_exhaustive]
pub enum TokioStream {
    /// A Unix socket.
    #[cfg(unix)]
    Unix(UnixStream),
    /// A TCP connection.
    Tcp(TcpStream),
}

macro_rules! delegate {
    ($self:ident, $method:ident($($arg:expr),*)) => {
        match $self.get_mut() {
            #[cfg(unix)]
            TokioStream::Unix(stream) => Pin::new(stream).$method($($arg),*),
            TokioStream::Tcp(stream) => Pin::new(stream).$method($($arg),*),
        }
    };
}

impl AsyncRead for TokioStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        delegate!(self, poll_read(cx, buf))
    }
}

impl AsyncWrite for TokioStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        delegate!(self, poll_write(cx, buf))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        delegate!(self, poll_flush(cx))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        delegate!(self, poll_shutdown(cx))
    }
}
