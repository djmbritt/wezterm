//! A blocking transport built on `tungstenite`.

use std::io::{self, Read, Write};
use std::net::TcpStream;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::time::Duration;

use tungstenite::client::IntoClientRequest;
use tungstenite::http::HeaderValue;
use tungstenite::{Message, WebSocket};

use super::{Frame, Transport};
use crate::endpoint::Endpoint;
use crate::error::{Error, Result};
use crate::protocol::{CONTROL_PATH, CONTROL_SUBPROTOCOL};

/// A blocking control connection over `tungstenite`.
pub struct TungsteniteTransport<S = Stream> {
    socket: WebSocket<S>,
}

impl TungsteniteTransport<Stream> {
    /// Opens a control connection to `endpoint`.
    pub fn connect(endpoint: &Endpoint) -> Result<Self> {
        let stream = match endpoint {
            #[cfg(unix)]
            Endpoint::Unix(path) => Stream::Unix(UnixStream::connect(path).map_err(|error| {
                Error::transport(io::Error::new(
                    error.kind(),
                    format!("{}: {error}", path.display()),
                ))
            })?),
            #[cfg(not(unix))]
            Endpoint::Unix(_) => {
                return Err(Error::InvalidEndpoint(
                    "Unix sockets need a Unix platform".into(),
                ));
            }
            Endpoint::Tcp { host, port } => {
                let stream =
                    TcpStream::connect((host.as_str(), *port)).map_err(Error::transport)?;
                stream.set_nodelay(true).map_err(Error::transport)?;
                Stream::Tcp(stream)
            }
        };
        Self::handshake(stream, &endpoint.ws_url(CONTROL_PATH))
    }

    /// Sets how long [`recv`](Transport::recv) waits before it fails with
    /// [`Error::TimedOut`]. `None` waits forever.
    ///
    /// A timeout leaves the connection usable, so it suits a loop that waits for events but
    /// also has other work to do.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<()> {
        match self.socket.get_ref() {
            #[cfg(unix)]
            Stream::Unix(stream) => stream.set_read_timeout(timeout),
            Stream::Tcp(stream) => stream.set_read_timeout(timeout),
        }
        .map_err(Error::transport)
    }
}

impl<S: Read + Write> TungsteniteTransport<S> {
    /// Opens a control connection over a stream that is already connected, such as a TLS
    /// stream. `url` is the `ws://` or `wss://` URL of the control endpoint.
    pub fn handshake(stream: S, url: &str) -> Result<Self> {
        let mut request = url.into_client_request().map_err(Error::transport)?;
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_static(CONTROL_SUBPROTOCOL),
        );
        let (socket, _response) = tungstenite::client::client(request, stream)
            .map_err(|error| Error::transport(format!("WebSocket handshake failed: {error}")))?;
        Ok(TungsteniteTransport { socket })
    }

    /// The underlying WebSocket.
    pub fn socket(&mut self) -> &mut WebSocket<S> {
        &mut self.socket
    }
}

impl<S: Read + Write> Transport for TungsteniteTransport<S> {
    fn send(&mut self, text: String) -> Result<()> {
        self.socket.send(Message::text(text)).map_err(map_error)
    }

    fn recv(&mut self) -> Result<Frame> {
        loop {
            match self.socket.read() {
                Ok(Message::Text(text)) => return Ok(Frame::Text(text.as_str().to_owned())),
                Ok(Message::Close(frame)) => {
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
                Ok(_) => continue,
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    return Ok(Frame::Closed {
                        code: None,
                        reason: String::new(),
                    });
                }
                Err(error) => return Err(map_error(error)),
            }
        }
    }

    fn close(&mut self) -> Result<()> {
        match self.socket.close(None) {
            Ok(()) => {}
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return Ok(());
            }
            Err(error) => return Err(map_error(error)),
        }
        // Wait for the server's close frame, so that the close handshake completes.
        loop {
            match self.socket.read() {
                Ok(_) => continue,
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    return Ok(());
                }
                Err(error) => return Err(map_error(error)),
            }
        }
    }
}

fn map_error(error: tungstenite::Error) -> Error {
    match error {
        tungstenite::Error::Io(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            Error::TimedOut
        }
        error => Error::transport(error),
    }
}

/// A stream that a [`TungsteniteTransport`] can connect over.
#[derive(Debug)]
#[non_exhaustive]
pub enum Stream {
    /// A Unix socket.
    #[cfg(unix)]
    Unix(UnixStream),
    /// A TCP connection.
    Tcp(TcpStream),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Stream::Unix(stream) => stream.read(buf),
            Stream::Tcp(stream) => stream.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Stream::Unix(stream) => stream.write(buf),
            Stream::Tcp(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Stream::Unix(stream) => stream.flush(),
            Stream::Tcp(stream) => stream.flush(),
        }
    }
}
