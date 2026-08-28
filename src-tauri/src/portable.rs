//! Portable-mode helpers: keep ALL writable data inside the EXE's own
//! directory instead of scattering it across `%APPDATA%` / `%LOCALAPPDATA%`.
//!
//! Call `ensure_dirs()` early in `main()`, before Tauri builds — that is the
//! only window where `WEBVIEW2_USER_DATA_FOLDER` can be set, because WRY reads
//! it once when constructing the WebView2 environment.

use std::path::PathBuf;

/// Returns the directory containing the currently running executable.
pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Returns the portable data root: `<exe_dir>/portable_data`
pub fn portable_root() -> PathBuf {
    exe_dir().join("portable_data")
}

/// Returns the WebView2 user data directory: `<portable_root>/WebView2`
pub fn webview_data_dir() -> PathBuf {
    portable_root().join("WebView2")
}

/// Ensures all portable directories exist.
pub fn ensure_dirs() -> std::io::Result<()> {
    std::fs::create_dir_all(webview_data_dir())?;
    Ok(())
}
