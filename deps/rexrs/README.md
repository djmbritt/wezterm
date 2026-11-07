# rexrs

A typed Rust client for the [Rex](https://www.superlogical.com/rex) server's control API.

> This is a vendored copy of [a10y/rexrs](https://github.com/a10y/rexrs) with local changes; see [REX-LINUX.md](REX-LINUX.md). Licensed `MIT OR Apache-2.0` (`LICENSE-MIT`, `LICENSE-APACHE`).

```rust
use rexrs::RexClient;
use rexrs::api::{BlockMethod, Layout, session, terminal};

let mut client = RexClient::builder().name("my-tool").connect()?;

let created = client.call(
    session::Create::new()
        .label("work")
        .initial_windows([session::CreateInitialWindow::new().layout(Layout::shell())]),
)?;
client.call(session::Attach::new(&created.session_id))?;

let block_id = &created.initial_windows[0].block_ids[0];
client.call(terminal::Write::new("ls\n").on(&created.session_id, block_id))?;
```

## Transports

The control API is a WebSocket (`GET /api/control`, subprotocol `rex.control.v1`), not plain
HTTP requests. A transport moves its text messages; the client does everything else.

| Feature | Transport | Client |
| --- | --- | --- |
| `tungstenite` (default) | `TungsteniteTransport` (blocking) | `RexClient::builder().connect()` |
| `tokio` | `TokioTransport` (async) | `AsyncRexClient::builder().connect().await` |

To use another WebSocket library, implement `Transport` or `AsyncTransport` and pass it to
`connect_with`. The built-in transports find the server from `REX_SERVER`, or the local Rex.app
socket on macOS.

## Errors

Server errors become `Error::Api(ApiError::…)`, with one variant for each error code, such as
`ApiError::SessionNotFound` or `ApiError::Conflict`. Codes this crate doesn't know become
`ApiError::Other`, which keeps the code and the status.

## Regenerating the API

The types in `src/api/generated/` and the tests in `tests/api_examples.rs` come from the server's
own description of its API, saved in `codegen/help.json`:

```sh
python3 codegen/generate.py --refresh   # snapshot the local server's API, then regenerate
python3 codegen/generate.py             # regenerate from the saved snapshot
```

## Tests

```sh
cargo test --all-features               # offline: unit tests and every documented example
cargo test --all-features -- --ignored  # live: needs a running Rex server, so not on CI
```
