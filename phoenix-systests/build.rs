fn main() {
    // The winfsp-x64.dll delay-load link arg must be applied to the crate that
    // produces the final binary (the test exes here); it doesn't propagate from
    // the winfsp-sys dependency. Without this the test binary treats winfsp as a
    // normal import and fails at startup with STATUS_DLL_NOT_FOUND.
    #[cfg(feature = "winfsp")]
    winfsp::build::winfsp_link_delayload();
}
