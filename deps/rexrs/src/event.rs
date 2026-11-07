//! Events that the server sends without being asked.

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::Result;

/// An event from the server, such as `session_created` or a session's `block_closed`.
///
/// An event that happens in a session arrives with that session's ID in
/// [`session_id`](Event::session_id). The event reference in the server's `/llms.txt` lists every
/// event and its fields. Skip events whose name you don't know: newer servers add events.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// The event's place among the events sent on this connection, starting at 1.
    pub seq: u64,
    /// The session the event happened in, or `None` for a server event.
    pub session_id: Option<String>,
    /// The event's name, such as `session_label_changed`.
    pub name: String,
    /// The event's fields.
    pub payload: Value,
}

impl Event {
    /// Decodes the event's fields into a type.
    pub fn parse<T: DeserializeOwned>(&self) -> Result<T> {
        Ok(T::deserialize(&self.payload)?)
    }

    pub(crate) fn from_wire(seq: u64, event: WireEvent) -> Result<Self> {
        if event.kind == "session_event" {
            let inner: SessionEvent = serde_json::from_value(event.payload)?;
            return Ok(Event {
                seq,
                session_id: Some(inner.session_id),
                name: inner.event.kind,
                payload: inner.event.payload,
            });
        }
        Ok(Event {
            seq,
            session_id: None,
            name: event.kind,
            payload: event.payload,
        })
    }
}

#[derive(Deserialize)]
pub(crate) struct WireEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    payload: Value,
}

#[derive(Deserialize)]
struct SessionEvent {
    session_id: String,
    event: WireEvent,
}
