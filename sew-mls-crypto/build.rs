//! `cfg(sew_rfc9180_vectors)`: the RFC 9180 vectors (`tests/vectors/rfc9180.json`) are in Sew's
//! source tree, so the unit test in `src/hpke_impl.rs` that reads them builds. The published
//! package has no `tests/` and no dev-dependencies for it: there the test is left out. The script
//! does nothing else.
#![allow(clippy::print_stdout)]

fn main() {
    println!("cargo::rustc-check-cfg=cfg(sew_rfc9180_vectors)");
    println!("cargo::rerun-if-changed=build.rs");
    let Ok(dir) = std::env::var("CARGO_MANIFEST_DIR") else {
        return;
    };
    let vectors = std::path::Path::new(&dir).join("tests/vectors/rfc9180.json");
    println!("cargo::rerun-if-changed={}", vectors.display());
    if vectors.is_file() {
        println!("cargo::rustc-cfg=sew_rfc9180_vectors");
    }
}
