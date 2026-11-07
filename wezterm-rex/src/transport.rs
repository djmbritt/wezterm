//! WebSocket connections to a Rex server: the control socket (driven by
//! `rexrs`) and per-terminal data sockets.
//!
//! Ported from rex-linux (`src/transport.rs`,
//! <https://github.com/shuv1337/rex-rune>, MIT).
use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use rexrs::AsyncRexClient;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub type Socket = WebSocketStream<tokio_tungstenite::MaybeTlsStream<Box<dyn Io>>>;

/// The oldest and newest Rex API revisions this client speaks.
pub const MIN_API_REVISION: u32 = 10;
pub const API_REVISION: u32 = 11;

/// The local Rex server: `$REX_SERVER`, else the per-user socket.
pub fn local_endpoint() -> String {
    std::env::var("REX_SERVER").unwrap_or_else(|_| {
        let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".into());
        format!("unix://{runtime}/rex/server.sock")
    })
}

/// Check that an endpoint is one we can connect to.
pub fn validate_endpoint(endpoint: &str) -> Result<()> {
    if let Some(path) = endpoint.strip_prefix("unix://") {
        anyhow::ensure!(
            path.starts_with('/'),
            "unix:// endpoints need an absolute path"
        );
        return Ok(());
    }
    let url = url::Url::parse(endpoint).context("invalid Rex endpoint")?;
    match url.scheme() {
        "https" | "http" | "wss" | "ws" => Ok(()),
        other => bail!("unsupported Rex endpoint scheme {other:?}"),
    }
}

async fn io(endpoint: &str, url: &url::Url) -> Result<Box<dyn Io>> {
    if let Some(path) = endpoint.strip_prefix("unix://") {
        #[cfg(unix)]
        return Ok(Box::new(tokio::net::UnixStream::connect(path).await?));
        #[cfg(not(unix))]
        bail!("unix:// endpoints are not supported on this platform ({path})");
    }
    let host = url.host_str().context("endpoint needs a host")?;
    let port = url
        .port_or_known_default()
        .context("endpoint needs a port")?;
    let tcp = tokio::net::TcpStream::connect((host, port)).await?;
    tcp.set_nodelay(true)?;
    Ok(Box::new(tcp))
}

/// Open a WebSocket at `path` on the server, negotiating `protocol`.
pub async fn socket(endpoint: &str, path: &str, protocol: &str) -> Result<Socket> {
    tokio::time::timeout(
        Duration::from_secs(10),
        open_socket(endpoint, path, protocol),
    )
    .await
    .context("Rex WebSocket connection timed out")?
}

async fn open_socket(endpoint: &str, path: &str, protocol: &str) -> Result<Socket> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut request_url = if endpoint.starts_with("unix://") {
        url::Url::parse("ws://localhost")?
    } else {
        let mut url = url::Url::parse(endpoint)?;
        let scheme = match url.scheme() {
            "https" | "wss" => "wss",
            "http" | "ws" => "ws",
            _ => bail!("unsupported endpoint scheme"),
        };
        url.set_scheme(scheme)
            .map_err(|_| anyhow::anyhow!("invalid scheme"))?;
        url
    };
    request_url.set_path(path);
    request_url.set_query(None);
    let mut request = request_url.as_str().into_client_request()?;
    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", protocol.parse()?);
    let stream = io(endpoint, &request_url).await?;
    let (socket, response) =
        tokio_tungstenite::client_async_tls_with_config(request, stream, None, None).await?;
    anyhow::ensure!(
        response
            .headers()
            .get("Sec-WebSocket-Protocol")
            .and_then(|h| h.to_str().ok())
            == Some(protocol),
        "Rex did not negotiate {protocol}"
    );
    Ok(socket)
}

/// `rexrs` transport over our socket.
pub struct Transport {
    pub socket: Socket,
}

impl rexrs::AsyncTransport for Transport {
    async fn send(&mut self, text: String) -> rexrs::Result<()> {
        self.socket
            .send(Message::Text(text.into()))
            .await
            .map_err(rexrs::Error::transport)
    }

    async fn recv(&mut self) -> rexrs::Result<rexrs::transport::Frame> {
        loop {
            match self.socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    return Ok(rexrs::transport::Frame::Text(text.to_string()));
                }
                Some(Ok(Message::Close(frame))) => {
                    return Ok(rexrs::transport::Frame::Closed {
                        code: frame.as_ref().map(|f| f.code.into()),
                        reason: frame.map(|f| f.reason.to_string()).unwrap_or_default(),
                    });
                }
                Some(Ok(_)) => self.socket.flush().await.map_err(rexrs::Error::transport)?,
                Some(Err(error)) => return Err(rexrs::Error::transport(error)),
                None => {
                    return Ok(rexrs::transport::Frame::Closed {
                        code: None,
                        reason: "connection ended".into(),
                    });
                }
            }
        }
    }

    async fn close(&mut self) -> rexrs::Result<()> {
        self.socket
            .close(None)
            .await
            .map_err(rexrs::Error::transport)
    }
}

pub type Client = AsyncRexClient<Transport>;

/// Connect the control socket and complete HELLO/WELCOME. `reconnect`
/// resumes a previous client identity.
pub async fn control(
    endpoint: &str,
    reconnect: Option<&str>,
    name: &str,
    version: &str,
) -> Result<Client> {
    // Attachment only: the server's lifetime and listeners belong to it.
    let socket = socket(endpoint, "/api/control", "rex.control.v1").await?;
    let mut builder = AsyncRexClient::builder()
        .name(name)
        .version(version)
        .api_revisions(MIN_API_REVISION..=API_REVISION)
        .platform(std::env::consts::OS)
        .kind(rexrs::ClientKind::App);
    if let Some(token) = reconnect {
        builder = builder.reconnect_token(token);
    }
    let client = tokio::time::timeout(
        Duration::from_secs(8),
        builder.connect_with(Transport { socket }),
    )
    .await
    .context("Rex control WELCOME timed out")??;
    anyhow::ensure!(
        (MIN_API_REVISION..=API_REVISION).contains(&client.welcome().api_revision),
        "Rex negotiated API revision {}; this client supports {MIN_API_REVISION}–{API_REVISION}",
        client.welcome().api_revision
    );
    Ok(client)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn endpoints() {
        assert!(validate_endpoint("unix:///run/user/1000/rex/server.sock").is_ok());
        assert!(validate_endpoint("unix://relative.sock").is_err());
        assert!(validate_endpoint("https://host.example.ts.net").is_ok());
        assert!(validate_endpoint("wss://host:8443").is_ok());
        assert!(validate_endpoint("ftp://host").is_err());
    }
}
