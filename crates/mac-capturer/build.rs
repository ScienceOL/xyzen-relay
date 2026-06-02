//! Compile MacCapturer.swift into a static library that's linked into the
//! crate. Out-of-band Swift integration (vs cc-rs) because cc doesn't speak
//! Swift; we shell out to `swiftc -emit-library -static`.
//!
//! Why static: avoids shipping a separate `.dylib` next to the binary.
//! Cost: caller must pass through the Swift runtime, which means linking
//! the macOS Swift system libraries — done via `cargo:rustc-link-arg`.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let target = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target != "macos" {
        // Stub on non-mac so the crate still type-checks.
        return;
    }

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let swift_src = manifest.join("swift").join("MacCapturer.swift");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let lib_path = out_dir.join("libMacCapturer.a");

    println!("cargo:rerun-if-changed={}", swift_src.display());

    let status = Command::new("swiftc")
        .args([
            "-emit-library",
            "-static",
            "-parse-as-library",
            "-O",
            "-target",
            "arm64-apple-macos12.3",
            "-module-name",
            "MacCapturer",
            // Skip the back-compat shims; we already require macOS 12.3.
            "-runtime-compatibility-version",
            "none",
            "-Xfrontend",
            "-disable-autolink-framework",
            "-Xfrontend",
            "CoreAudioTypes",
            "-emit-module",
            "-emit-module-path",
        ])
        .arg(out_dir.join("MacCapturer.swiftmodule"))
        .args(["-o"])
        .arg(&lib_path)
        .arg(&swift_src)
        .status()
        .expect("run swiftc");
    if !status.success() {
        panic!("swiftc failed");
    }

    // Tell rustc to link our static lib.
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=MacCapturer");

    // System frameworks.
    for fw in ["ScreenCaptureKit", "VideoToolbox", "CoreMedia", "CoreVideo",
               "Foundation", "CoreFoundation", "AVFoundation", "CoreGraphics"] {
        println!("cargo:rustc-link-lib=framework={fw}");
    }

    // Swift runtime.
    println!("cargo:rustc-link-arg=-L/usr/lib/swift");
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    for lib in ["swiftCore", "swiftFoundation", "swiftDispatch"] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
}
