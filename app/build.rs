//! Tauri build step.
//!
//! The RadioReference app key is committed (XOR-masked) in `src/rr.rs` — see
//! the note there. This used to read it from `HS_RR_APP_KEY` or a git-ignored
//! `.rr_app_key` file, but that broke fresh clones (a build had no key), so
//! the obfuscated key now ships with the source.
fn main() {
    tauri_build::build()
}
