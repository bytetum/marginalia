//! Re-run the build (and so re-embed `web/`) when a file is added to or removed from it.
fn main() {
    println!("cargo:rerun-if-changed=web");
}
