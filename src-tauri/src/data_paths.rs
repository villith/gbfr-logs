//! Where the app keeps its writable files (logs.db and the logs/ folder).
//!
//! Windows keeps the historical CWD-relative layout: the installer's shortcut
//! anchors CWD to the install dir and existing databases live there.
//!
//! Linux resolves absolute XDG paths instead, because CWD is useless there in
//! both directions: a desktop entry or AppImage launches with CWD at `/` (or a
//! read-only mount), and chdir'ing away is fatal — linuxdeploy-plugin-gtk
//! patches the bundled libwebkit2gtk's hardcoded `/usr` prefix to the
//! same-length `././`, so WebKit spawns its helper processes (e.g.
//! WebKitNetworkProcess) via paths relative to the CWD that AppRun set. The
//! process must never call `set_current_dir`.

use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

/// Directory holding the app's writable data. `.` on Windows (CWD-relative,
/// see module docs); `$XDG_DATA_HOME/gbfr-logs` on Linux.
///
/// Resolved (and created, on Linux) once, on first call — that call is
/// `setup_db()` in `main()`, so a failure panics before the UI exists rather
/// than unwinding inside a command handler.
pub fn data_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        #[cfg(windows)]
        {
            crate::portable::exe_dir()
        }
        #[cfg(not(windows))]
        {
            let mut dir = tauri::api::path::data_dir()
                .expect("Could not resolve the user data directory ($XDG_DATA_HOME)");
            dir.push("gbfr-logs");
            std::fs::create_dir_all(&dir)
                .unwrap_or_else(|e| panic!("Failed to create {}: {e}", dir.display()));
            dir
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn windows_is_exe_relative() {
        assert!(data_dir().is_absolute(), "data_dir must be absolute (EXE-relative for portability)");
        assert!(data_dir().join("logs.db").is_absolute());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_is_absolute_under_xdg_data() {
        let dir = data_dir();
        assert!(dir.is_absolute());
        assert!(dir.ends_with("gbfr-logs"));
        assert!(dir.exists());
    }
}
