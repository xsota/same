use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let brand = manifest_dir.join("../../assets/brand");
    println!("cargo:rerun-if-changed=build.rs");
    println!(
        "cargo:rerun-if-changed={}",
        brand.join("same.ico").display()
    );

    // Windows: embed the app icon as resource 1, which GPUI loads for the
    // window and taskbar (and Explorer shows for same.exe).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let ico = brand
            .join("same.ico")
            .canonicalize()
            .expect("assets/brand/same.ico");
        // Written with an absolute path: the resource compiler resolves
        // relative paths against the preprocessed copy, not this crate.
        let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("same.rc");
        let ico = ico.to_string_lossy().replace('\\', "\\\\");
        std::fs::write(&out, format!("1 ICON \"{ico}\"\n")).unwrap();
        embed_resource::compile(&out, embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}
