# Building wezterm with the libghostty-vt backend

This branch replaces wezterm's built-in terminal emulator with
[libghostty-vt](https://lib.ghostty.org/vt/), the terminal emulation
library extracted from [Ghostty](https://ghostty.org). Wezterm keeps its own
GUI, renderer, multiplexer, config and Lua API. libghostty-vt parses
program output and stores the terminal state (screen, scrollback, cursor,
modes) for every local, SSH and tmux-domain pane.

## Requirements

- Everything wezterm normally needs to build from source. See
  <https://wezterm.org/install/source.html>, or run `./get-deps` on Linux.
- A Rust toolchain (`rustup`).
- **Zig 0.17.x**, to build libghostty-vt. Get it from
  <https://ziglang.org/download/>, your package manager, or a version
  manager (for example `mise install zig@0.17.0`).
- `git` and network access the first time. The script clones ghostty, and
  Zig downloads a few small packages that libghostty-vt needs.

## Quick start

```sh
git clone --branch ghostty-backend https://github.com/djmbritt/wezterm.git
cd wezterm
git submodule update --init --recursive

# 1. Build libghostty-vt into deps/libghostty-vt
./build-libghostty-vt.sh

# 2. Build wezterm
cargo build --release -p wezterm-gui -p wezterm -p wezterm-mux-server

# 3. Run it
./target/release/wezterm-gui
```

`ghostty-vt-sys` links libghostty-vt **statically**, so the resulting
binaries don't need libghostty-vt at runtime.

## The build script

`build-libghostty-vt.sh` clones ghostty into `deps/ghostty`, checks out the
commit this branch was tested against, and installs the library to
`deps/libghostty-vt`. Both folders are git-ignored. It accepts these
environment variables:

| Variable           | Purpose                                                         |
|--------------------|-----------------------------------------------------------------|
| `ZIG`              | Zig binary to use (default `zig`)                               |
| `GHOSTTY_REV`      | Ghostty commit to build instead of the tested one               |
| `GHOSTTY_CHECKOUT` | Build an existing ghostty checkout instead of cloning one        |

The script runs the same command that you can run by hand in any ghostty
checkout:

```sh
zig build -Demit-lib-vt -Doptimize=ReleaseFast --prefix /some/prefix
```

## How `ghostty-vt-sys` finds the library

The crate's `build.rs` checks these in order:

1. `LIBGHOSTTY_VT_DIR`: an install prefix that contains
   `lib/libghostty-vt.a` and `include/ghostty/vt.h`.
2. `GHOSTTY_SRC_DIR`: a ghostty checkout. Cargo runs `zig build` itself
   and installs into its `OUT_DIR`. Set `ZIG` if `zig` isn't on `PATH`.
3. `deps/libghostty-vt` in this repository, which is what
   `build-libghostty-vt.sh` produces.
4. pkg-config: `libghostty-vt-static`, then `libghostty-vt`.
   This covers distribution packages of libghostty-vt.

For example, to use a library you built somewhere else:

```sh
LIBGHOSTTY_VT_DIR=$HOME/opt/libghostty-vt cargo build --release -p wezterm-gui
```

## Updating the bindings

`ghostty-vt-sys/src/bindings.rs` is generated with bindgen and committed,
so normal builds don't need libclang. After moving to a libghostty-vt
version whose C API changed, regenerate the file:

```sh
cargo install bindgen-cli
./ghostty-vt-sys/regen-bindings.sh deps/libghostty-vt
```

## Tests

```sh
cargo test -p ghostty-vt-sys -p wezterm-ghostty
```

## Troubleshooting

- **`Unable to locate libghostty-vt`**: run `./build-libghostty-vt.sh`, or
  set `LIBGHOSTTY_VT_DIR`.
- **Zig errors about the version**: libghostty-vt needs Zig 0.17.x
  (`minimum_zig_version` in ghostty's `build.zig.zon`).
- **Zig fails with "reached unreachable code"**: clear the Zig cache
  (`rm -rf ~/.cache/zig`) and retry. This is a known Zig 0.17 problem with
  cached package archives.
- **Cargo can't fetch git dependencies** (for example when a global git
  config rewrites `https://` URLs to `ssh://`): build with
  `CARGO_NET_GIT_FETCH_WITH_CLI=true`.

## Known limitations

- **tmux control mode (`tmux -CC`) doesn't work.** libghostty-vt doesn't
  expose the DCS passthrough that wezterm's tmux integration needs.
- **Inline images aren't shown.** This covers sixel, iTerm2 images and the
  kitty graphics protocol. libghostty-vt supports kitty graphics, but it
  isn't wired into wezterm's renderer yet.
- **Key mapping assumes a US layout** when it picks physical keys for
  characters. This only affects kitty-protocol "alternate key" reporting.
- **A steady block cursor set by an application** shows as your configured
  `default_cursor_style`.
- **Device attribute replies** (`CSI c`) come from libghostty-vt rather
  than wezterm.
- Wezterm's internal panes (launcher, overlays, the SSH connect screen)
  still use wezterm's built-in emulator.
