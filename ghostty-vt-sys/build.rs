//! Locates (or builds) libghostty-vt and links it statically.
//!
//! Resolution order:
//! 1. `LIBGHOSTTY_VT_DIR`: an install prefix containing `lib/libghostty-vt.a`
//!    (as produced by `zig build -Demit-lib-vt --prefix <dir>`).
//! 2. `GHOSTTY_SRC_DIR`: a ghostty source checkout; we run
//!    `zig build -Demit-lib-vt -Doptimize=ReleaseFast` into OUT_DIR.
//! 3. `deps/libghostty-vt` in the workspace, as produced by
//!    `./build-libghostty-vt.sh`.
//! 4. pkg-config `libghostty-vt-static`, then `libghostty-vt`.
use std::path::{Path, PathBuf};

fn link_prefix(prefix: &Path) {
    let lib = prefix.join("lib");
    println!("cargo:rustc-link-search=native={}", lib.display());
    println!("cargo:rustc-link-lib=static=ghostty-vt");
    println!("cargo:include={}", prefix.join("include").display());
}

fn main() {
    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_DIR");
    println!("cargo:rerun-if-env-changed=GHOSTTY_SRC_DIR");
    println!("cargo:rerun-if-env-changed=ZIG");

    if let Ok(dir) = std::env::var("LIBGHOSTTY_VT_DIR") {
        let prefix = PathBuf::from(dir);
        let archive = prefix.join("lib").join("libghostty-vt.a");
        println!("cargo:rerun-if-changed={}", archive.display());
        if !archive.exists() {
            panic!(
                "LIBGHOSTTY_VT_DIR={} does not contain lib/libghostty-vt.a",
                prefix.display()
            );
        }
        link_prefix(&prefix);
        return;
    }

    if let Ok(src) = std::env::var("GHOSTTY_SRC_DIR") {
        let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("libghostty-vt");
        let zig = std::env::var("ZIG").unwrap_or_else(|_| "zig".to_string());
        let status = std::process::Command::new(zig)
            .current_dir(&src)
            .args(["build", "-Demit-lib-vt", "-Doptimize=ReleaseFast", "--prefix"])
            .arg(&out)
            .status()
            .expect("failed to run zig; set ZIG or put zig 0.17 on PATH");
        if !status.success() {
            panic!("zig build of libghostty-vt failed");
        }
        link_prefix(&out);
        return;
    }

    let default_prefix = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("..")
        .join("deps")
        .join("libghostty-vt");
    let archive = default_prefix.join("lib").join("libghostty-vt.a");
    println!("cargo:rerun-if-changed={}", archive.display());
    if archive.exists() {
        link_prefix(&default_prefix);
        return;
    }

    for name in ["libghostty-vt-static", "libghostty-vt"] {
        if pkg_config::Config::new().statik(true).probe(name).is_ok() {
            return;
        }
    }

    panic!(
        "Unable to locate libghostty-vt. Run ./build-libghostty-vt.sh from \
         the repository root (requires zig 0.17), set LIBGHOSTTY_VT_DIR to an \
         install prefix or GHOSTTY_SRC_DIR to a ghostty checkout, or make it \
         discoverable via pkg-config. See BUILDING-GHOSTTY.md."
    );
}
