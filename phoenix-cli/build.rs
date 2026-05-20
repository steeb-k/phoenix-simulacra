fn main() {
    embed_admin_manifest();
}

fn embed_admin_manifest() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("windows")
        .join("admin.manifest");

    println!("cargo:rerun-if-changed={}", manifest.display());

    let manifest_str = manifest
        .to_str()
        .expect("admin.manifest path must be valid UTF-8");

    winres::WindowsResource::new()
        .set_manifest_file(manifest_str)
        .compile()
        .expect("failed to embed Windows application manifest");
}
