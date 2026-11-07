#!/bin/sh
# Fetch and build libghostty-vt, the terminal emulation library used by
# this branch of wezterm, into deps/libghostty-vt where the
# ghostty-vt-sys crate finds it automatically.
#
# Environment:
#   ZIG              zig binary to use (default: zig). Must be 0.17.x.
#   GHOSTTY_REV      ghostty commit to build (default: the tested commit)
#   GHOSTTY_CHECKOUT existing ghostty checkout to build instead of cloning
#                    into deps/ghostty (GHOSTTY_REV is then not applied)
set -e

GHOSTTY_REV=${GHOSTTY_REV:-5813c9461d5af78b6307a1254d00b1bc190eaccd}
ROOT=$(cd "$(dirname "$0")" && pwd)
PREFIX="$ROOT/deps/libghostty-vt"
ZIG=${ZIG:-zig}

if ! command -v "$ZIG" >/dev/null 2>&1; then
  echo "zig not found. Install zig 0.17 (https://ziglang.org/download/)" >&2
  echo "or set ZIG=/path/to/zig" >&2
  exit 1
fi
case "$("$ZIG" version)" in
  0.17.*) ;;
  *) echo "warning: libghostty-vt requires zig 0.17.x; found $("$ZIG" version)" >&2 ;;
esac

if [ -n "$GHOSTTY_CHECKOUT" ]; then
  SRC="$GHOSTTY_CHECKOUT"
else
  SRC="$ROOT/deps/ghostty"
  if [ ! -d "$SRC/.git" ]; then
    git clone --filter=blob:none https://github.com/ghostty-org/ghostty.git "$SRC"
  fi
  if ! git -C "$SRC" cat-file -e "$GHOSTTY_REV^{commit}" 2>/dev/null; then
    git -C "$SRC" fetch origin
  fi
  git -C "$SRC" -c advice.detachedHead=false checkout "$GHOSTTY_REV"
fi

cd "$SRC"
"$ZIG" build -Demit-lib-vt -Doptimize=ReleaseFast --prefix "$PREFIX"
echo "libghostty-vt installed to $PREFIX"
