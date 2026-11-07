//! Errors from the client and from the server.

use std::error::Error as StdError;

/// The result type of this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Anything that can go wrong while talking to a Rex server.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The server answered a request with an error.
    #[error(transparent)]
    Api(#[from] ApiError),

    /// The transport failed, such as when the socket couldn't be opened or a write failed.
    #[error("transport error: {0}")]
    Transport(#[source] Box<dyn StdError + Send + Sync>),

    /// The transport's read timed out, such as after
    /// [`TungsteniteTransport::set_read_timeout`](crate::transport::TungsteniteTransport::set_read_timeout).
    /// The connection is still usable.
    #[error("timed out waiting for the server")]
    TimedOut,

    /// The server closed the connection.
    ///
    /// For example, code 1008 with a reason that starts with `no common API revision` means that
    /// the client and the server share no API revision.
    #[error("connection closed by server (code {code:?}): {reason}")]
    Closed {
        /// The WebSocket close code, if the server sent a close frame.
        code: Option<u16>,
        /// The close reason, if any.
        reason: String,
    },

    /// The server broke the control protocol, such as by sending something other than a welcome
    /// first.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// A request is larger than the server accepts. The server would close the connection, so
    /// the client doesn't send it.
    #[error("request is {size} bytes, larger than the server's limit of {limit}")]
    RequestTooLarge {
        /// The encoded request's size, in bytes.
        size: usize,
        /// The connection's message limit, in bytes.
        limit: usize,
    },

    /// A request couldn't be encoded, or a response couldn't be decoded into its type.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// A server address couldn't be parsed or found.
    #[error("invalid endpoint: {0}")]
    InvalidEndpoint(String),

    /// The client's settings are invalid, such as a hello larger than the server accepts.
    #[error("invalid configuration: {0}")]
    Config(String),
}

impl Error {
    /// Wraps a transport's own error.
    pub fn transport(error: impl Into<Box<dyn StdError + Send + Sync>>) -> Self {
        Error::Transport(error.into())
    }

    /// The server's error, if this is one.
    pub fn api(&self) -> Option<&ApiError> {
        match self {
            Error::Api(error) => Some(error),
            _ => None,
        }
    }
}

/// An error that the server returned for one request.
///
/// Each variant is one of the server's error codes. Branch on the variant, not on the message,
/// whose wording can change between releases. A code that this crate doesn't know becomes
/// [`ApiError::Other`]; use [`status`](ApiError::status) to handle it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ApiError {
    /// `bad_request` (400): the arguments are invalid, such as a missing field or a wrong type.
    #[error("bad request: {message}")]
    BadRequest { message: String },

    /// `not_found` (404): something the arguments name doesn't exist, such as a block or window.
    #[error("not found: {message}")]
    NotFound { message: String },

    /// `method_not_found` (404): the server, or the block the arguments name, has no method by
    /// that name. This happens when the server is older than the client.
    #[error("method not found: {message}")]
    MethodNotFound { message: String },

    /// `session_not_found` (404): no session has the `session_id` the arguments name.
    #[error("session not found: {message}")]
    SessionNotFound { message: String },

    /// `conflict` (409): the request can't run in the current state, such as when the connection
    /// isn't attached to the session. It can succeed once the state changes.
    #[error("conflict: {message}")]
    Conflict { message: String },

    /// `not_supported` (400): the named client has no such action or can't perform it now, or
    /// the server doesn't keep the data the request needs.
    #[error("not supported: {message}")]
    NotSupported { message: String },

    /// `refused` (403): the named client chose not to perform the action.
    #[error("refused: {message}")]
    Refused { message: String },

    /// `client_unavailable` (409): the named client can't take the request, such as when its
    /// connection has dropped.
    #[error("client unavailable: {message}")]
    ClientUnavailable { message: String },

    /// `too_large` (413): the result is larger than the message limit, or the change would make
    /// stored data larger than the server allows.
    #[error("too large: {message}")]
    TooLarge { message: String },

    /// `revision_too_low` (400): the method needs a newer API revision than the connection uses.
    #[error("API revision too low: {message}")]
    RevisionTooLow { message: String },

    /// `timeout` (504): the request didn't finish in the time the server allows. Some of its
    /// work may already have happened.
    #[error("timeout: {message}")]
    Timeout { message: String },

    /// `internal` (500): the request failed for any other reason.
    #[error("internal server error: {message}")]
    Internal { message: String },

    /// A code that this version of the crate doesn't know.
    #[error("{code} (status {status}): {message}")]
    Other {
        code: String,
        status: u16,
        message: String,
    },
}

impl ApiError {
    /// Builds the error for a code, status, and message from the server.
    pub fn from_wire(code: &str, status: u16, message: String) -> Self {
        match code {
            "bad_request" => ApiError::BadRequest { message },
            "not_found" => ApiError::NotFound { message },
            "method_not_found" => ApiError::MethodNotFound { message },
            "session_not_found" => ApiError::SessionNotFound { message },
            "conflict" => ApiError::Conflict { message },
            "not_supported" => ApiError::NotSupported { message },
            "refused" => ApiError::Refused { message },
            "client_unavailable" => ApiError::ClientUnavailable { message },
            "too_large" => ApiError::TooLarge { message },
            "revision_too_low" => ApiError::RevisionTooLow { message },
            "timeout" => ApiError::Timeout { message },
            "internal" => ApiError::Internal { message },
            _ => ApiError::Other {
                code: code.to_owned(),
                status,
                message,
            },
        }
    }

    /// The server's error code, such as `not_found`.
    pub fn code(&self) -> &str {
        match self {
            ApiError::BadRequest { .. } => "bad_request",
            ApiError::NotFound { .. } => "not_found",
            ApiError::MethodNotFound { .. } => "method_not_found",
            ApiError::SessionNotFound { .. } => "session_not_found",
            ApiError::Conflict { .. } => "conflict",
            ApiError::NotSupported { .. } => "not_supported",
            ApiError::Refused { .. } => "refused",
            ApiError::ClientUnavailable { .. } => "client_unavailable",
            ApiError::TooLarge { .. } => "too_large",
            ApiError::RevisionTooLow { .. } => "revision_too_low",
            ApiError::Timeout { .. } => "timeout",
            ApiError::Internal { .. } => "internal",
            ApiError::Other { code, .. } => code,
        }
    }

    /// A number with the same meaning as an HTTP status code, such as 404.
    pub fn status(&self) -> u16 {
        match self {
            ApiError::BadRequest { .. }
            | ApiError::NotSupported { .. }
            | ApiError::RevisionTooLow { .. } => 400,
            ApiError::Refused { .. } => 403,
            ApiError::NotFound { .. }
            | ApiError::MethodNotFound { .. }
            | ApiError::SessionNotFound { .. } => 404,
            ApiError::Conflict { .. } | ApiError::ClientUnavailable { .. } => 409,
            ApiError::TooLarge { .. } => 413,
            ApiError::Internal { .. } => 500,
            ApiError::Timeout { .. } => 504,
            ApiError::Other { status, .. } => *status,
        }
    }

    /// The server's message, written for people.
    pub fn message(&self) -> &str {
        match self {
            ApiError::BadRequest { message }
            | ApiError::NotFound { message }
            | ApiError::MethodNotFound { message }
            | ApiError::SessionNotFound { message }
            | ApiError::Conflict { message }
            | ApiError::NotSupported { message }
            | ApiError::Refused { message }
            | ApiError::ClientUnavailable { message }
            | ApiError::TooLarge { message }
            | ApiError::RevisionTooLow { message }
            | ApiError::Timeout { message }
            | ApiError::Internal { message }
            | ApiError::Other { message, .. } => message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_codes_round_trip() {
        for code in [
            "bad_request",
            "not_found",
            "method_not_found",
            "session_not_found",
            "conflict",
            "not_supported",
            "refused",
            "client_unavailable",
            "too_large",
            "revision_too_low",
            "timeout",
            "internal",
        ] {
            let error = ApiError::from_wire(code, 0, "m".into());
            assert_eq!(error.code(), code);
            assert!(
                !matches!(error, ApiError::Other { .. }),
                "{code} fell through"
            );
        }
    }

    #[test]
    fn unknown_code_keeps_status() {
        let error = ApiError::from_wire("teapot", 418, "short and stout".into());
        assert_eq!(
            error,
            ApiError::Other {
                code: "teapot".into(),
                status: 418,
                message: "short and stout".into()
            }
        );
        assert_eq!(error.status(), 418);
    }
}
