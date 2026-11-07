//! `cfg(sew_fixtures)`: the fixture bitstreams of Sew's media test client sit beside this
//! crate (Sew's source tree), so the unit tests that read them build (with `testing`). The
//! published package has no such directory and no dev-dependencies for them: there these tests
//! are left out, even when another crate of the workspace turns `testing` on.
#![allow(clippy::print_stdout)]

fn main() {
    println!("cargo::rustc-check-cfg=cfg(sew_fixtures)");
    println!("cargo::rerun-if-changed=build.rs");
    let Ok(dir) = std::env::var("CARGO_MANIFEST_DIR") else {
        return;
    };
    let fixtures = std::path::Path::new(&dir).join("../sew-media-testclient/fixtures");
    println!("cargo::rerun-if-changed={}", fixtures.display());
    if fixtures.is_dir() {
        println!("cargo::rustc-cfg=sew_fixtures");
    }
}
