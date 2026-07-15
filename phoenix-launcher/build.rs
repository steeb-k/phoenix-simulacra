// Embed an asInvoker manifest and the app icon. The launcher deliberately does
// NOT request administrator: it runs in the user's context (no shield on its
// icon) and elevates the GUI on demand via ShellExecute "runas", so the lone
// UAC prompt is attributed to the GUI. Unlike the GUI/CLI build scripts this one
// does not emit build-info or a winfsp delay-load arg — the launcher has no such
// dependencies.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let manifest = root.join("windows").join("asinvoker.manifest");
    let icon = root.join("assets").join("phoenix-appicon.ico");

    println!("cargo:rerun-if-changed={}", manifest.display());
    println!("cargo:rerun-if-changed={}", icon.display());

    let manifest_str = manifest
        .to_str()
        .expect("asinvoker.manifest path must be valid UTF-8");
    let icon_str = icon
        .to_str()
        .expect("phoenix-appicon.ico path must be valid UTF-8");

    winres::WindowsResource::new()
        .set_manifest_file(manifest_str)
        .set_icon(icon_str)
        .compile()
        .expect("failed to embed Windows application resources");
}
