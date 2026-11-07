//! Where a Rex server listens.

use std::fmt;
use std::path::PathBuf;

use percent_encoding::percent_decode_str;

use crate::error::{Error, Result};

/// The environment variable that a Rex server sets in each of its terminals to its own address.
pub const SERVER_ENV: &str = "REX_SERVER";

/// The address of a Rex server.
///
/// A local server listens on a Unix socket. A server with a network listener can also be reached
/// over plain TCP; transports for TLS listeners, such as Tailscale, can be plugged in through
/// [`Transport`](crate::Transport).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Endpoint {
    /// A Unix socket, such as `~/Library/Application Support/rex/server.sock`.
    Unix(PathBuf),
    /// A TCP listener.
    Tcp {
        /// The host name or IP address.
        host: String,
        /// The port.
        port: u16,
    },
}

impl Endpoint {
    /// A Unix socket at `path`.
    pub fn unix(path: impl Into<PathBuf>) -> Self {
        Endpoint::Unix(path.into())
    }

    /// A TCP listener at `host` and `port`.
    pub fn tcp(host: impl Into<String>, port: u16) -> Self {
        Endpoint::Tcp {
            host: host.into(),
            port,
        }
    }

    /// Parses an address in the form that `rex server status` prints and `REX_SERVER` holds.
    ///
    /// Accepts `unix://<percent-encoded path>`, `tcp://host:port`, `ws://host:port`, and a bare
    /// absolute path to a socket.
    ///
    /// ```
    /// use rexrs::Endpoint;
    ///
    /// let endpoint = Endpoint::parse("unix:///Users/me/Library/Application%20Support/rex/server.sock")?;
    /// assert_eq!(endpoint, Endpoint::unix("/Users/me/Library/Application Support/rex/server.sock"));
    /// # Ok::<(), rexrs::Error>(())
    /// ```
    pub fn parse(address: &str) -> Result<Self> {
        let invalid = || Error::InvalidEndpoint(address.to_owned());
        if let Some(path) = address.strip_prefix("unix://") {
            let path = percent_decode_str(path)
                .decode_utf8()
                .map_err(|_| invalid())?;
            if path.is_empty() {
                return Err(invalid());
            }
            return Ok(Endpoint::Unix(PathBuf::from(path.as_ref())));
        }
        if let Some(rest) = address
            .strip_prefix("tcp://")
            .or_else(|| address.strip_prefix("ws://"))
        {
            let rest = rest.trim_end_matches('/');
            let (host, port) = rest.rsplit_once(':').ok_or_else(invalid)?;
            let host = host.trim_start_matches('[').trim_end_matches(']');
            let port = port.parse().map_err(|_| invalid())?;
            if host.is_empty() {
                return Err(invalid());
            }
            return Ok(Endpoint::tcp(host, port));
        }
        if address.starts_with('/') {
            return Ok(Endpoint::unix(address));
        }
        Err(invalid())
    }

    /// The address in `REX_SERVER`, if it is set.
    pub fn from_env() -> Result<Option<Self>> {
        match std::env::var(SERVER_ENV) {
            Ok(address) if !address.is_empty() => Self::parse(&address).map(Some),
            _ => Ok(None),
        }
    }

    /// The socket that the local Rex.app server listens on.
    ///
    /// Only macOS has a known default. On other platforms, set `REX_SERVER` or name the
    /// endpoint.
    pub fn local_default() -> Result<Self> {
        if cfg!(target_os = "macos") {
            let home = std::env::var_os("HOME")
                .ok_or_else(|| Error::InvalidEndpoint("HOME is not set".into()))?;
            let mut path = PathBuf::from(home);
            path.extend(["Library", "Application Support", "rex", "server.sock"]);
            return Ok(Endpoint::Unix(path));
        }
        Err(Error::InvalidEndpoint(format!(
            "no default server address on this platform; set {SERVER_ENV} or name an endpoint"
        )))
    }

    /// The address in `REX_SERVER`, or else [`local_default`](Self::local_default).
    pub fn discover() -> Result<Self> {
        match Self::from_env()? {
            Some(endpoint) => Ok(endpoint),
            None => Self::local_default(),
        }
    }

    /// The `ws://` URL of `path` on this server. Over a Unix socket, any host name works.
    pub fn ws_url(&self, path: &str) -> String {
        match self {
            Endpoint::Unix(_) => format!("ws://localhost{path}"),
            Endpoint::Tcp { host, port } if host.contains(':') => {
                format!("ws://[{host}]:{port}{path}")
            }
            Endpoint::Tcp { host, port } => format!("ws://{host}:{port}{path}"),
        }
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Endpoint::Unix(path) => write!(f, "unix://{}", path.display()),
            Endpoint::Tcp { host, port } if host.contains(':') => {
                write!(f, "tcp://[{host}]:{port}")
            }
            Endpoint::Tcp { host, port } => write!(f, "tcp://{host}:{port}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_percent_encoded_unix_socket() {
        let endpoint = Endpoint::parse("unix:///a%20b/server.sock").unwrap();
        assert_eq!(endpoint, Endpoint::unix("/a b/server.sock"));
        assert_eq!(
            endpoint.ws_url("/api/control"),
            "ws://localhost/api/control"
        );
    }

    #[test]
    fn parses_tcp() {
        assert_eq!(
            Endpoint::parse("tcp://example.com:7000").unwrap(),
            Endpoint::tcp("example.com", 7000)
        );
        assert_eq!(
            Endpoint::parse("ws://[::1]:7000/").unwrap(),
            Endpoint::tcp("::1", 7000)
        );
        assert_eq!(Endpoint::tcp("::1", 7000).ws_url("/x"), "ws://[::1]:7000/x");
    }

    #[test]
    fn parses_bare_path() {
        assert_eq!(
            Endpoint::parse("/tmp/rex.sock").unwrap(),
            Endpoint::unix("/tmp/rex.sock")
        );
    }

    #[test]
    fn rejects_garbage() {
        for address in [
            "",
            "unix://",
            "tcp://host",
            "tcp://:80",
            "tcp://host:port",
            "rex",
        ] {
            assert!(Endpoint::parse(address).is_err(), "{address:?} parsed");
        }
    }
}
