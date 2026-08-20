//! Link `libairspy` when the `airspy` feature is on. No new crate
//! dependencies: ask `pkg-config` where the library lives and fall back to the
//! usual Homebrew / distro paths, so `brew install airspy` (macOS) or
//! `apt install libairspy-dev` (Debian/Ubuntu) is all a build needs.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=AIRSPY_LIB_DIR");
    if std::env::var_os("CARGO_FEATURE_AIRSPY").is_none() {
        return;
    }
    if let Some(dir) = std::env::var_os("AIRSPY_LIB_DIR") {
        println!("cargo:rustc-link-search=native={}", dir.to_string_lossy());
    } else if let Ok(out) = std::process::Command::new("pkg-config")
        .args(["--variable=libdir", "libairspy"])
        .output()
    {
        let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && !dir.is_empty() {
            println!("cargo:rustc-link-search=native={dir}");
        }
    }
    for dir in ["/opt/homebrew/lib", "/usr/local/lib"] {
        if std::path::Path::new(dir).is_dir() {
            println!("cargo:rustc-link-search=native={dir}");
        }
    }
    println!("cargo:rustc-link-lib=airspy");
}
