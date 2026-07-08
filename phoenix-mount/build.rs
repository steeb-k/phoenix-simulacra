fn main() {
    // Only the `winfsp` feature needs WinFsp. When it's on, configure
    // delay-loading of winfsp-<arch>.dll so the built binary links against the
    // bundled/redistributable DLL at run time (no static WinFsp dependency).
    #[cfg(feature = "winfsp")]
    winfsp::build::winfsp_link_delayload();
}
