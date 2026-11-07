//! The layout tree that methods such as `session.create` and `session.new_split` build blocks from.
//!
//! The server's schema leaves this argument untyped, so it is written by hand.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::Direction;

/// The flavor of a terminal that runs a shell.
pub const SHELL_FLAVOR: &str = "com.superlogical.terminal.shell";

/// A node of a layout: one new block, or a split between two nodes.
///
/// ```
/// use rexrs::api::{Direction, Layout};
///
/// // An editor on the left and two shells stacked on the right.
/// let layout = Layout::split(
///     Direction::Horizontal,
///     Layout::shell().command(["vim"]),
///     Layout::split(Direction::Vertical, Layout::shell(), Layout::shell().label("logs")),
/// );
/// # let _ = layout;
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    /// A new block.
    Block(LayoutBlock),
    /// A split between two nodes.
    Split(LayoutSplit),
}

/// A new block in a [`Layout`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutBlock {
    /// The flavor to make the block with, such as `com.superlogical.terminal.shell`.
    pub flavor: String,
    /// The block's label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The flavor's options. `block_creator.list` gives each flavor's options schema.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub options: Map<String, Value>,
}

/// A split between two nodes of a [`Layout`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutSplit {
    /// How to divide the space.
    pub direction: Direction,
    /// The node on the left or top.
    pub before: Box<Layout>,
    /// The node on the right or bottom.
    pub after: Box<Layout>,
    /// The fraction of the space that `before` gets. The server defaults to 0.5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ratio: Option<f64>,
}

impl Layout {
    /// A block of any flavor.
    pub fn block(flavor: impl Into<String>) -> Self {
        Layout::Block(LayoutBlock {
            flavor: flavor.into(),
            label: None,
            options: Map::new(),
        })
    }

    /// A terminal that runs the user's shell.
    pub fn shell() -> Self {
        Self::block(SHELL_FLAVOR)
    }

    /// A split between two nodes, with an even ratio unless [`ratio`](Self::ratio) sets one.
    pub fn split(direction: Direction, before: Layout, after: Layout) -> Self {
        Layout::Split(LayoutSplit {
            direction,
            before: Box::new(before),
            after: Box::new(after),
            ratio: None,
        })
    }

    /// Sets a block's label. Does nothing to a split.
    pub fn label(mut self, label: impl Into<String>) -> Self {
        if let Layout::Block(block) = &mut self {
            block.label = Some(label.into());
        }
        self
    }

    /// Sets one of a block's flavor options. Does nothing to a split.
    pub fn option(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        if let Layout::Block(block) = &mut self {
            block.options.insert(key.into(), value.into());
        }
        self
    }

    /// Sets the directory a terminal starts in.
    pub fn cwd(self, cwd: impl Into<String>) -> Self {
        self.option("cwd", cwd.into())
    }

    /// Sets the program and arguments a terminal runs instead of an interactive shell.
    pub fn command(self, command: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let command: Vec<Value> = command
            .into_iter()
            .map(|arg| Value::String(arg.into()))
            .collect();
        self.option("command", command)
    }

    /// Sets the fraction of a split's space that its `before` node gets. Does nothing to a block.
    pub fn ratio(mut self, ratio: f64) -> Self {
        if let Layout::Split(split) = &mut self {
            split.ratio = Some(ratio);
        }
        self
    }
}
