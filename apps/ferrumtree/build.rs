//! Embed the icon and the version information with the MSVC linker (same resources as the
//! GUI, see `../ferrumtree-gui/res.rs`).

#[path = "../ferrumtree-gui/res.rs"]
mod res;

fn main() {
    println!("cargo:rerun-if-changed=../ferrumtree-gui/res.rs");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("ferrumtree.res");
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    res::write(
        &out,
        &res::Version {
            file_description: "FerrumTree command line",
            internal_name: "ferrumtree",
            original_filename: "ferrumtree.exe",
            product_name: "FerrumTree",
            version: &version,
            copyright: "Copyright (c) 2026 FerrumTree contributors",
        },
    )
    .expect("write resources");
    println!("cargo:rustc-link-arg-bins={}", out.display());
}
