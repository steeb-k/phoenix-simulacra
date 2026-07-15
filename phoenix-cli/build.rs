// See `phoenix-gui/build.rs` for the rationale on the `include!` pattern.
include!("../windows/build_info.rs");

fn main() {
    emit_build_info();
    embed_windows_resources();
    // Apply the winfsp-x64.dll delay-load link arg to this binary (it doesn't
    // propagate from the winfsp-sys dependency).
    #[cfg(feature = "winfsp")]
    winfsp::build::winfsp_link_delayload();
}

fn embed_windows_resources() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let manifest = root.join("windows").join("admin.manifest");
    let icon = root.join("assets").join("phoenix-appicon.ico");

    println!("cargo:rerun-if-changed={}", manifest.display());
    println!("cargo:rerun-if-changed={}", icon.display());

    let manifest_str = manifest
        .to_str()
        .expect("admin.manifest path must be valid UTF-8");
    let icon_str = icon
        .to_str()
        .expect("phoenix-appicon.ico path must be valid UTF-8");

    // Present as "Phoenix Simulacra CLI" rather than the crate name
    // "phoenix-cli". See phoenix-gui/build.rs for the full rationale.
    winres::WindowsResource::new()
        .set_manifest_file(manifest_str)
        .set_icon(icon_str)
        .set("FileDescription", "Phoenix Simulacra CLI")
        .set("ProductName", "Phoenix Simulacra")
        .set("InternalName", "simulacra-cli")
        .set("OriginalFilename", "simulacra-cli.exe")
        .set("LegalCopyright", "© 2026 Steve Kzenjak")
        .compile()
        .expect("failed to embed Windows application resources");
}
