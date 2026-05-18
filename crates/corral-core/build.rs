//! Compile-time probe that asserts our hand-encoded Darwin struct
//! sizes and field offsets match the actual macOS SDK. A mismatch
//! (e.g. Apple bumps `struct kinfo_proc`) fails the build rather than
//! producing silent garbage reads at runtime.
//!
//! Skipped on non-macOS targets — the structs and the parsers that use
//! them are macOS-specific.

use std::env;

fn main() {
    let target = env::var("TARGET").unwrap_or_default();
    if !target.contains("darwin") {
        return;
    }

    let mut build = cc::Build::new();
    build
        .file("build_probe/layout.c")
        .flag_if_supported("-Wno-unused")
        .warnings_into_errors(true);
    build.compile("watcher_layout_probe");
}
