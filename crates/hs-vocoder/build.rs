//! Compiles the vendored ISC-licensed mbelib C sources (IMBE 7200x4400 path)
//! when the `imbe` feature is enabled.

fn main() {
    #[cfg(feature = "imbe")]
    {
        let dir = "vendor/mbelib";
        println!("cargo:rerun-if-changed={dir}");
        cc::Build::new()
            .file(format!("{dir}/mbelib.c"))
            .file(format!("{dir}/imbe7200x4400.c"))
            .file(format!("{dir}/ecc.c"))
            .include(dir)
            .warnings(false)
            .compile("mbe");
    }
}
