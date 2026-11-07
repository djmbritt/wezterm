---
tags:
  - multiplexing
  - rex
---
# `rex_domains`

*Since: nightly builds only (the `rex-client` branch)*

Attaches wezterm to a [Rex](https://www.superlogical.com/rex) terminal server.

Rex keeps sessions, windows, split layouts and the programs in them on the
server. A Rex domain shows one Rex session in one wezterm window:

* each Rex window is a wezterm tab;
* each tab's panes follow the Rex window's split layout;
* each pane is a Rex terminal, restored from the server's snapshot and kept
  current by its output stream.

The server owns the layout and the colours. Splits, new tabs and closed panes
made in wezterm are sent to the server as requests, and changes made by other
Rex clients show up in wezterm.

```lua
config.rex_domains = {
  {
    -- The name used by `wezterm connect` and the launcher.
    name = 'rex',
    -- Optional. Defaults to $REX_SERVER, else
    -- unix://$XDG_RUNTIME_DIR/rex/server.sock
    endpoint = 'unix:///run/user/1000/rex/server.sock',
    -- Optional. The session's label or id. Defaults to the first session.
    session = 'work',
    -- Optional. Attach when wezterm starts.
    connect_automatically = false,
  },
}
```

Then:

```console
$ wezterm connect rex
```

Endpoints can be `unix://` paths or `http(s)://`/`ws(s)://` URLs.

## What happens when you…

| Action | Effect on the server |
| --- | --- |
| attach | Nothing. Attaching never starts a server, creates a session or opens a terminal. If the session has no terminals, the attach fails. If the session ends, the domain detaches. |
| split a pane | `session.new_split` beside it, running the login shell. |
| open a new tab | `session.new_window` in the session, running the login shell. |
| close a pane or tab | The terminals are closed (wezterm asks first). |
| close the wezterm window, quit, or `DetachDomain` | Nothing: the programs keep running. Attach again to pick up where you were. |
| resize the window | Size requests. The server applies them while wezterm owns the terminal (or nobody does). |
| focus the wezterm window | Takes ownership of the terminals in the active tab, as Rex's own clients do. Typing or clicking in a terminal also takes it. |

Typed keys, mouse, wheel and paste go to the server as input events, and the
server's terminal encodes them (including the kitty keyboard protocol). While
the connection or a terminal's stream is down, input is dropped, never queued
or replayed.

## Limitations

* Floating and minimized layers are not shown.
* Moving or resizing splits in wezterm is not sent to the server; the next
  layout change from the server resets it.
* A spawned command is ignored; new tabs and splits run the login shell.
* Kitty graphics from the server can't use file, temp-file or shared-memory
  media.
* The session is shown in one wezterm window; new tabs open there.
* One session per domain. Use several `rex_domains` entries for several
  sessions.
