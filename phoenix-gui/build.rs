fn main() {
    embed_windows_resources();
}

fn embed_windows_resources() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let manifest = root.join("windows").join("admin.manifest");
    let icon = root.join("carbon-phoenix-icon.ico");

    println!("cargo:rerun-if-changed={}", manifest.display());
    println!("cargo:rerun-if-changed={}", icon.display());

    let manifest_str = manifest
        .to_str()
        .expect("admin.manifest path must be valid UTF-8");
    let icon_str = icon
        .to_str()
        .expect("carbon-phoenix-icon.ico path must be valid UTF-8");

    winres::WindowsResource::new()
        .set_manifest_file(manifest_str)
        .set_icon(icon_str)
        .compile()
        .expect("failed to embed Windows application resources");
}
