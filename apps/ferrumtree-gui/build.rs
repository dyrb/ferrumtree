//! Embed the manifest, the icon and the version information with the MSVC linker itself
//! (no resource compiler, no extra crates).

#[path = "res.rs"]
mod res;

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("ferrumtree-gui.res");
    let manifest = std::path::Path::new(&dir).join("app.manifest");
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=res.rs");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    res::write(
        &out,
        &res::Version {
            file_description: "FerrumTree",
            internal_name: "ferrumtree-gui",
            original_filename: "ferrumtree-gui.exe",
            product_name: "FerrumTree",
            version: &version,
            copyright: "Copyright (c) 2026 FerrumTree contributors",
        },
    )
    .expect("write resources");
    println!("cargo:rustc-link-arg-bins={}", out.display());
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", manifest.display());
    // The manifest carries its own trustInfo; do not let the linker add another.
    println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:NO");
}
