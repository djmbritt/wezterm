#!/bin/sh
# Regenerate src/bindings.rs from an installed libghostty-vt prefix.
# Usage: ./regen-bindings.sh /path/to/libghostty-vt-prefix
set -e
I="$1/include"
cd "$(dirname "$0")"
bindgen "$I/ghostty/vt.h" \
  --allowlist-function 'ghostty_.*' --allowlist-type 'Ghostty.*' --allowlist-var 'GHOSTTY_.*' \
  --no-layout-tests --default-enum-style consts --no-prepend-enum-name --no-doc-comments --use-core \
  --raw-line '#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, clippy::all)]' \
  -o src/bindings.rs -- -I"$I"
