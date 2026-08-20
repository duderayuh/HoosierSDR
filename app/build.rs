//! Tauri build step, plus the RadioReference app key embed.
//!
//! The key is read from `HS_RR_APP_KEY` at build time and written into the
//! binary XOR-masked. That keeps it out of the repo and out of a casual
//! `strings` dump — nothing more. A key in a desktop binary cannot be kept
//! secret from someone who wants it; the real controls are RadioReference's
//! per-application key (revocable) and each user's own premium login. A
//! build without the variable embeds nothing and the app asks for a key.
fn main() {
    println!("cargo:rerun-if-env-changed=HS_RR_APP_KEY");
    let key = std::env::var("HS_RR_APP_KEY").unwrap_or_default();
    const MASK: [u8; 16] = [
        0x5a, 0xc3, 0x91, 0x2e, 0x77, 0xb8, 0x04, 0xe5, 0x3c, 0x6f, 0xd2, 0x19, 0x8b, 0x40, 0xa7, 0xf1,
    ];
    let masked: Vec<String> = key
        .bytes()
        .enumerate()
        .map(|(i, b)| (b ^ MASK[i % MASK.len()]).to_string())
        .collect();
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("rr_key.rs");
    std::fs::write(
        out,
        format!(
            "pub(crate) const RR_KEY_MASKED: &[u8] = &[{}];\n",
            masked.join(",")
        ),
    )
    .unwrap();
    tauri_build::build()
}
