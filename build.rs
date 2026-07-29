fn main() {
    // See Readme at: https://github.com/SnowflakePowered/winfsp-rs
    #[cfg(all(windows, feature = "winfsp"))]
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "windows" {
        // Note: can't cross compile from linux to Windows since winfsp can't build on linux
        winfsp::build::winfsp_link_delayload();
    }
}
