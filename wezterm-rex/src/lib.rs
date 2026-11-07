//! A client for [Rex](https://www.superlogical.com/rex) terminal servers,
//! for wezterm's Rex domain.
//!
//! This crate is protocol only; it knows nothing about wezterm's mux. A
//! [`Host`] connects to one Rex server and reports sessions, views (windows,
//! split layouts, focus) and terminal streams as [`HostEvent`]s. Terminal
//! streams carry a libghostty snapshot followed by live VT output, which the
//! caller feeds to a libghostty-backed terminal.
//!
//! The protocol handling is ported from rex-linux
//! (<https://github.com/shuv1337/rex-rune>, MIT), an unofficial Linux client.
//! Neither project is affiliated with Superlogical.
pub mod host;
pub mod input;
pub mod transport;
pub mod wire;

pub use host::{
    terminal_blocks, Host, HostCommand, HostConfig, HostEvent, Input, StreamEvent, Theme,
    TERMINAL_CREATOR,
};
pub use rexrs::api::session::{
    Layer, LayerBlock, Session, Split, ViewLayoutNode, ViewResponse, Window,
};
pub use rexrs::api::Direction;
pub use transport::local_endpoint;
