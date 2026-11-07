use crate::config::validate_domain_name;
use luahelper::impl_lua_conversion_dynamic;
use wezterm_dynamic::{FromDynamic, ToDynamic};

/// A Rex terminal server to attach to. Rex keeps sessions running on the
/// server; wezterm shows the selected session's windows as tabs and its
/// server-side splits as panes.
///
/// ```lua
/// config.rex_domains = {
///   { name = 'rex', session = 'work' },
///   { name = 'laptop', endpoint = 'https://laptop.example.ts.net' },
/// }
/// ```
///
/// Then `wezterm connect rex`.
#[derive(Default, Debug, Clone, FromDynamic, ToDynamic)]
pub struct RexDomain {
    /// The name of this domain. Must be unique amongst all types of domain
    /// in the configuration file.
    #[dynamic(validate = "validate_domain_name")]
    pub name: String,

    /// `unix:///absolute/path.sock`, `https://…`, `http://…`, `wss://…` or
    /// `ws://…`. Defaults to `$REX_SERVER`, then the local per-user socket
    /// (`unix://$XDG_RUNTIME_DIR/rex/server.sock`).
    #[dynamic(default)]
    pub endpoint: Option<String>,

    /// The session to show, by exact label or ID. Defaults to the server's
    /// default session. A name that matches no session, or several, shows
    /// nothing rather than guessing.
    #[dynamic(default)]
    pub session: Option<String>,

    /// Connect when wezterm starts, as with `wezterm connect`.
    #[dynamic(default)]
    pub connect_automatically: bool,
}
impl_lua_conversion_dynamic!(RexDomain);
