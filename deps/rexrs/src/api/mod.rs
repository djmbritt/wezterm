//! Typed requests and responses for the Rex control API.
//!
//! Every server method has a request type that implements [`Method`], in a module named after
//! the method's namespace: `session.new_split` is [`session::NewSplit`], and its result is
//! [`session::NewSplitResponse`]. Required arguments go to `new`, and each optional argument has
//! a setter of the same name:
//!
//! ```
//! use rexrs::api::{Direction, Layout, Side, session};
//!
//! let request = session::NewSplit::new("session:<uuid>", Direction::Horizontal, Side::After,
//!     Layout::shell().label("logs"))
//!     .anchor_id("block:<uuid>")
//!     .focus(true);
//! # let _ = request;
//! ```
//!
//! Block methods, such as `com.superlogical.terminal.write`, implement [`BlockMethod`] instead.
//! Their arguments name no block, so [`BlockMethod::on`] picks the block to call them on.
//!
//! The types in the namespace modules are generated from the server's own description of its
//! API by `codegen/generate.py`.

mod generated;
mod layout;

pub use generated::*;
pub use layout::{Layout, LayoutBlock, LayoutSplit};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};

/// A server method: a request type, its wire name, and the type of its result.
pub trait Method: Serialize {
    /// The method's wire name, such as `session.new_split`.
    const NAME: &'static str;
    /// The method's result.
    type Response: DeserializeOwned;
}

/// A method that a block provides, such as `com.superlogical.terminal.write`.
///
/// The implementing type holds the method's own arguments. Use [`on`](BlockMethod::on) to name
/// the block, which gives a [`BlockRequest`] that the client can send.
pub trait BlockMethod: Serialize + Sized {
    /// The method's full wire name, such as `com.superlogical.terminal.write`.
    const NAME: &'static str;
    /// The method's result.
    type Response: DeserializeOwned;

    /// Calls this method on a block.
    fn on(self, session_id: impl Into<String>, block_id: impl Into<String>) -> BlockRequest<Self> {
        BlockRequest {
            session_id: session_id.into(),
            block_id: block_id.into(),
            args: self,
        }
    }
}

/// A block method's arguments, and the block to call it on.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BlockRequest<A> {
    /// The session that holds the block.
    pub session_id: String,
    /// The block to call the method on.
    pub block_id: String,
    /// The method's own arguments.
    pub args: A,
}

impl<A: BlockMethod> Method for BlockRequest<A> {
    const NAME: &'static str = A::NAME;
    type Response = A::Response;
}

/// An empty result, for methods that return `{}`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Empty {}

/// How a split divides its space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Direction {
    /// Side by side: before on the left, after on the right.
    #[serde(rename = "horizontal")]
    Horizontal,
    /// One above the other: before on top, after below.
    #[serde(rename = "vertical")]
    Vertical,
    /// A value that this version of the crate doesn't know. It can't be sent.
    #[serde(other, skip_serializing)]
    Unknown,
}

/// Which part of a split something goes in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Side {
    /// The left or top part.
    #[serde(rename = "before")]
    Before,
    /// The right or bottom part.
    #[serde(rename = "after")]
    After,
    /// A value that this version of the crate doesn't know. It can't be sent.
    #[serde(other, skip_serializing)]
    Unknown,
}

/// Deserializes `null` as the type's default, for fields the server sends as null when empty.
pub(crate) fn nullable<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}
