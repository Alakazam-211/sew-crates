//! `cfg(sew_fixtures)`: the fixture bitstreams of Sew's media test client sit beside this
//! crate (Sew's source tree), so the unit tests that read them build (with `testing`). The
//! published package has no such directory and no dev-dependencies for them: there these tests
//! are left out, even when another crate of the workspace turns `testing` on.
//!
//! `cfg(sew_apple_preview)`: macOS or iOS with feature `video` (so kessel-video's `apple`
//! feature is on), where kessel-video has `ApplePreview` and `FrameSource::apple_preview`.
//! `cfg(sew_preview_events)`: the camera taps' preview items exist, there or with `testing`
//! (a stand-in handle off Apple, so their order is tested anywhere).
#![allow(clippy::print_stdout)]

fn main() {
    println!("cargo::rustc-check-cfg=cfg(sew_fixtures)");
    println!("cargo::rustc-check-cfg=cfg(sew_apple_preview)");
    println!("cargo::rustc-check-cfg=cfg(sew_preview_events)");
    println!("cargo::rerun-if-changed=build.rs");
    let apple = matches!(
        std::env::var("CARGO_CFG_TARGET_OS").as_deref(),
        Ok("macos" | "ios")
    );
    let video = std::env::var_os("CARGO_FEATURE_VIDEO").is_some();
    let testing = std::env::var_os("CARGO_FEATURE_TESTING").is_some();
    if apple && video {
        println!("cargo::rustc-cfg=sew_apple_preview");
    }
    if (apple && video) || testing {
        println!("cargo::rustc-cfg=sew_preview_events");
    }
    let Ok(dir) = std::env::var("CARGO_MANIFEST_DIR") else {
        return;
    };
    let fixtures = std::path::Path::new(&dir).join("../sew-media-testclient/fixtures");
    println!("cargo::rerun-if-changed={}", fixtures.display());
    if fixtures.is_dir() {
        println!("cargo::rustc-cfg=sew_fixtures");
    }
}
