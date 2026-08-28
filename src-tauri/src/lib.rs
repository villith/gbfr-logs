//! Library crate for gbfr-logs. Holds the parser + db modules so both the main
//! Tauri binary (`main.rs`) and auxiliary binaries (e.g. `bin/skill_backfill.rs`)
//! can share them. main.rs is a thin binary that `use`s this crate.
pub mod backfill;
// Client for the hook's control channel: hook hot-reload / refresh (`Eject`)
// plus the dev Debug tab's event broadcast and `Hello` override. Windows only;
// the hook ships the `eject` control channel in release.
#[cfg(windows)]
pub mod control_rpc;
pub mod data_paths;
pub mod db;
// Dev-only Debug tab scripts. Pure and platform-independent, so unlike the hook
// path it also compiles and tests on Linux CI.
pub mod debug_events;
#[cfg(windows)]
pub mod game_mem;
pub mod hook_dll;
pub mod legality;
pub mod linux_support;
// The decoded-log cache behind `fetch_encounter_state`. Pure and
// platform-independent, so it tests on Linux CI too.
pub mod log_cache;
pub mod overmastery;
pub mod parser;
pub mod portable;
pub mod rpc;
pub mod settings_db;
pub mod synthesis;
pub mod toolbox_rpc;
pub mod transmarvel;
