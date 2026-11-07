//! The control protocol's messages, and the state both clients share.
//!
//! Nothing here does I/O. [`Core`] turns requests into text messages and text messages into
//! responses and events, so that the blocking and the async client only move text.

use std::collections::VecDeque;
use std::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::Method;
use crate::error::{ApiError, Error, Result};
use crate::event::{Event, WireEvent};

/// The WebSocket subprotocol of the control connection. Transports must ask for it.
pub const CONTROL_SUBPROTOCOL: &str = "rex.control.v1";

/// The HTTP path of the control connection.
pub const CONTROL_PATH: &str = "/api/control";

/// The largest hello the server accepts, in bytes.
const MAX_HELLO_BYTES: usize = 512;

/// What sort of program a client is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClientKind {
    /// An app that shows sessions, such as Rex.app.
    App,
    /// A command-line tool.
    Cli,
    /// An agent, such as a coding assistant.
    Agent,
    /// Anything else.
    Other,
}

/// What a client tells the server about itself, for showing to people.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClientInfo {
    /// What sort of program the client is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<ClientKind>,
    /// What the client runs on: macos, ios, linux, windows, web, or other.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// The program's name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The program's version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The client's label. Two clients can share a label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Default for ClientInfo {
    fn default() -> Self {
        let platform = if cfg!(target_os = "macos") {
            "macos"
        } else if cfg!(target_os = "ios") {
            "ios"
        } else if cfg!(target_os = "linux") {
            "linux"
        } else if cfg!(target_os = "windows") {
            "windows"
        } else {
            "other"
        };
        ClientInfo {
            kind: None,
            platform: Some(platform.to_owned()),
            name: Some(env!("CARGO_PKG_NAME").to_owned()),
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            label: None,
        }
    }
}

#[derive(Serialize)]
struct Hello<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    api_revision: u32,
    api_min_revision: u32,
    client: &'a ClientInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    reconnect_token: Option<&'a str>,
}

/// The server's first message on a control connection.
#[derive(Clone, Deserialize)]
#[non_exhaustive]
pub struct Welcome {
    /// The protocol, `rex.control.v1`.
    pub protocol: String,
    /// The limits of this connection.
    pub limits: Limits,
    /// The client this connection serves. Left out by servers that can't resume clients.
    #[serde(default)]
    pub client: Option<WelcomeClient>,
    /// Optional behaviors the server supports.
    #[serde(default, deserialize_with = "crate::api::nullable")]
    pub features: Vec<String>,
    /// The API revision every later message on this connection uses.
    #[serde(default)]
    pub api_revision: u32,
    /// The newest API revision the server speaks.
    #[serde(default)]
    pub api_server_revision: u32,
    /// The oldest API revision the server speaks.
    #[serde(default)]
    pub api_min_revision: u32,
}

impl fmt::Debug for Welcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Welcome")
            .field("protocol", &self.protocol)
            .field("limits", &self.limits)
            .field("client", &self.client)
            .field("features", &self.features)
            .field("api_revision", &self.api_revision)
            .field("api_server_revision", &self.api_server_revision)
            .field("api_min_revision", &self.api_min_revision)
            .finish()
    }
}

/// The limits of a control connection.
#[derive(Debug, Clone, Copy, Deserialize)]
#[non_exhaustive]
pub struct Limits {
    /// The largest message the server sends or accepts, in bytes.
    pub max_message_bytes: usize,
}

/// The client that a control connection serves.
#[derive(Clone, Deserialize)]
#[non_exhaustive]
pub struct WelcomeClient {
    /// The client's ID. It stays the same when the client reconnects.
    pub id: String,
    /// Send this in a later hello to continue this client. Never log it.
    pub reconnect_token: String,
    /// True if this connection continued an existing client.
    #[serde(default)]
    pub resumed: bool,
}

impl fmt::Debug for WelcomeClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WelcomeClient")
            .field("id", &self.id)
            .field("reconnect_token", &"<redacted>")
            .field("resumed", &self.resumed)
            .finish()
    }
}

/// A method's result, with the `seq` of the last event the server had queued when it started the
/// request. Events with a higher `seq` happened after the request started.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply<T> {
    /// The `seq` of the last event queued before the request started, or 0.
    pub seq: u64,
    /// The method's result.
    pub value: T,
}

#[derive(Serialize)]
struct Request<'a, P> {
    #[serde(rename = "type")]
    kind: &'static str,
    id: &'a str,
    method: &'a str,
    payload: &'a P,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Incoming {
    Welcome(Welcome),
    Response(WireResponse),
    Event {
        seq: u64,
        payload: WireEvent,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
pub(crate) struct WireResponse {
    id: String,
    #[serde(default)]
    seq: u64,
    #[serde(default)]
    payload: Option<Value>,
    #[serde(default)]
    error: Option<WireError>,
}

#[derive(Deserialize)]
struct WireError {
    code: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    status: u16,
}

impl WireResponse {
    pub(crate) fn into_reply<T: DeserializeOwned>(self) -> Result<Reply<T>> {
        if let Some(error) = self.error {
            return Err(ApiError::from_wire(&error.code, error.status, error.message).into());
        }
        // A method that returns nothing still decodes, such as into `Empty` or `Value`.
        let payload = self
            .payload
            .unwrap_or_else(|| Value::Object(Default::default()));
        Ok(Reply {
            seq: self.seq,
            value: T::deserialize(payload)?,
        })
    }
}

/// The per-connection state that both clients share.
pub(crate) struct Core {
    welcome: Welcome,
    next_id: u64,
    events: VecDeque<Event>,
}

impl Core {
    pub(crate) fn hello(
        info: &ClientInfo,
        reconnect_token: Option<&str>,
        revisions: &std::ops::RangeInclusive<u32>,
    ) -> Result<String> {
        if revisions.is_empty() {
            return Err(Error::Config("API revision range is empty".into()));
        }
        let hello = Hello {
            kind: "hello",
            api_revision: *revisions.end(),
            api_min_revision: *revisions.start(),
            client: info,
            reconnect_token,
        };
        let text = serde_json::to_string(&hello)?;
        if text.len() >= MAX_HELLO_BYTES {
            return Err(Error::Config(format!(
                "the hello is {} bytes, and the server accepts less than {MAX_HELLO_BYTES}; \
                 shorten the client's name, version, or label",
                text.len()
            )));
        }
        Ok(text)
    }

    /// Reads the server's first message, which must be a welcome.
    pub(crate) fn accept(text: &str) -> Result<Self> {
        let welcome = match serde_json::from_str::<Incoming>(text) {
            Ok(Incoming::Welcome(welcome)) => welcome,
            Ok(_) => return Err(Error::Protocol("the first message wasn't a welcome".into())),
            Err(error) => return Err(Error::Protocol(format!("invalid welcome: {error}"))),
        };
        if welcome.protocol != CONTROL_SUBPROTOCOL {
            return Err(Error::Protocol(format!(
                "unexpected protocol {:?}",
                welcome.protocol
            )));
        }
        Ok(Core {
            welcome,
            next_id: 0,
            events: VecDeque::new(),
        })
    }

    pub(crate) fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    /// Encodes a request, returning its ID and its text.
    pub(crate) fn request<P: Serialize>(
        &mut self,
        method: &str,
        payload: &P,
    ) -> Result<(String, String)> {
        self.next_id += 1;
        let id = self.next_id.to_string();
        let text = serde_json::to_string(&Request {
            kind: "request",
            id: &id,
            method,
            payload,
        })?;
        let limit = self.welcome.limits.max_message_bytes;
        if text.len() > limit {
            return Err(Error::RequestTooLarge {
                size: text.len(),
                limit,
            });
        }
        Ok((id, text))
    }

    pub(crate) fn encode<M: Method>(&mut self, request: &M) -> Result<(String, String)> {
        self.request(M::NAME, request)
    }

    /// Handles one message from the server. Queues events, and returns the response to the
    /// request with ID `waiting`, if this is it. Responses to other IDs belong to requests that
    /// were abandoned, so they are dropped.
    pub(crate) fn receive(
        &mut self,
        text: &str,
        waiting: Option<&str>,
    ) -> Result<Option<WireResponse>> {
        match serde_json::from_str::<Incoming>(text)? {
            Incoming::Response(response) if Some(response.id.as_str()) == waiting => {
                Ok(Some(response))
            }
            Incoming::Response(_) | Incoming::Unknown => Ok(None),
            Incoming::Event { seq, payload } => {
                self.events.push_back(Event::from_wire(seq, payload)?);
                Ok(None)
            }
            Incoming::Welcome(_) => Err(Error::Protocol("the server sent a second welcome".into())),
        }
    }

    pub(crate) fn pop_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    pub(crate) fn drain_events(&mut self) -> std::collections::vec_deque::Drain<'_, Event> {
        self.events.drain(..)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::API_REVISION;

    const WELCOME: &str = r#"{"type":"welcome","protocol":"rex.control.v1","limits":{"max_message_bytes":65536},
        "client":{"id":"client:1","reconnect_token":"secret"},"api_revision":7}"#;

    #[test]
    fn hello_names_the_revision() {
        let text =
            Core::hello(&ClientInfo::default(), None, &(API_REVISION..=API_REVISION)).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["type"], "hello");
        assert_eq!(value["api_revision"], API_REVISION);
        assert_eq!(value["client"]["name"], "rexrs");
        assert!(value.get("reconnect_token").is_none());
    }

    #[test]
    fn hello_rejects_oversized_client_info() {
        let info = ClientInfo {
            label: Some("x".repeat(600)),
            ..ClientInfo::default()
        };
        assert!(matches!(
            Core::hello(&info, None, &(7..=8)),
            Err(Error::Config(_))
        ));
    }

    #[test]
    fn builder_advertises_supported_range() {
        let builder = crate::RexClient::builder().api_revisions(7..=8);
        let hello: Value = serde_json::from_str(&builder.hello().unwrap()).unwrap();
        assert_eq!(hello["api_revision"], 8);
        assert_eq!(hello["api_min_revision"], 7);
        let invalid = std::ops::RangeInclusive::new(8, 7);
        assert!(
            crate::RexClient::builder()
                .api_revisions(invalid)
                .hello()
                .is_err()
        );
    }

    #[test]
    fn welcome_debug_redacts_token() {
        let core = Core::accept(WELCOME).unwrap();
        assert!(!format!("{:?}", core.welcome()).contains("secret"));
    }

    #[test]
    fn rejects_other_first_message() {
        let text = r#"{"type":"event","seq":1,"payload":{"type":"x","payload":{}}}"#;
        assert!(matches!(Core::accept(text), Err(Error::Protocol(_))));
    }

    #[test]
    fn queues_events_and_matches_responses() {
        let mut core = Core::accept(WELCOME).unwrap();
        let (id, text) = core
            .request("session.view", &serde_json::json!({"session_id": "s"}))
            .unwrap();
        let request: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(request["method"], "session.view");
        assert_eq!(request["id"], id.as_str());

        let event = r#"{"type":"event","seq":3,"payload":{"type":"session_event","payload":
            {"session_id":"session:1","event":{"type":"block_closed","payload":{"block_id":"b"}}}}}"#;
        assert!(core.receive(event, Some(&id)).unwrap().is_none());
        let stale = r#"{"type":"response","id":"stale","seq":0,"payload":{}}"#;
        assert!(core.receive(stale, Some(&id)).unwrap().is_none());
        assert!(
            core.receive(r#"{"type":"from_the_future"}"#, Some(&id))
                .unwrap()
                .is_none()
        );

        let response =
            format!(r#"{{"type":"response","id":"{id}","seq":3,"payload":{{"revision":9}}}}"#);
        let reply: Reply<Value> = core
            .receive(&response, Some(&id))
            .unwrap()
            .unwrap()
            .into_reply()
            .unwrap();
        assert_eq!(reply.seq, 3);
        assert_eq!(reply.value["revision"], 9);

        let event = core.pop_event().unwrap();
        assert_eq!(event.session_id.as_deref(), Some("session:1"));
        assert_eq!(event.name, "block_closed");
        assert_eq!(event.payload["block_id"], "b");
        assert!(core.pop_event().is_none());
    }

    #[test]
    fn maps_errors() {
        let mut core = Core::accept(WELCOME).unwrap();
        let text = r#"{"type":"response","id":"1","seq":0,
            "error":{"code":"session_not_found","message":"no such session","status":404}}"#;
        let response = core.receive(text, Some("1")).unwrap().unwrap();
        let error = response.into_reply::<Value>().unwrap_err();
        assert!(matches!(
            error,
            Error::Api(ApiError::SessionNotFound { ref message }) if message == "no such session"
        ));
    }

    #[test]
    fn rejects_oversized_requests() {
        let mut core = Core::accept(WELCOME).unwrap();
        let payload = serde_json::json!({"data": "x".repeat(70_000)});
        assert!(matches!(
            core.request("m", &payload),
            Err(Error::RequestTooLarge { .. })
        ));
    }
}
