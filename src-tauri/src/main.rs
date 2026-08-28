// Prevents additional console window on Windows in release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    collections::HashMap,
    fs::File,
    io::Write,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
};

use anyhow::Context;
use gbfr_logs::toolbox_rpc::{self, HookStatus};
use gbfr_logs::{db, legality, overmastery, parser, settings_db, synthesis, transmarvel};

use db::logs::LogEntry;
#[cfg(windows)]
use dll_syringe::{process::OwnedProcess, Syringe};
#[cfg(windows)]
use interprocess::os::windows::named_pipe::tokio::RecvPipeStream;
use log::{error, info, LevelFilter};
use parser::{
    constants::{CharacterType, EnemyType},
    v1::{self, PlayerData},
};
use protocol::Message;
use rusqlite::params_from_iter;
use serde::{Deserialize, Serialize};
#[cfg(windows)]
use std::path::Path;
use tauri::{
    api::dialog::blocking::FileDialogBuilder, AppHandle, CustomMenuItem, LogicalSize, Manager,
    Size, State, SystemTray, SystemTrayEvent, SystemTrayMenu, SystemTrayMenuItem,
};
use tauri_plugin_log::LogTarget;
use tauri_plugin_window_state::{AppHandleExt, StateFlags};
use tokio_stream::StreamExt;
use tokio_util::codec::FramedRead;

struct AlwaysOnTop(AtomicBool);
struct ClickThrough(AtomicBool);
struct DebugMode(AtomicBool);

struct TrayLabels {
    open_meter: std::sync::Mutex<String>,
    open_logs: std::sync::Mutex<String>,
    always_on_top: std::sync::Mutex<String>,
    always_on_top_active: std::sync::Mutex<String>,
    clickthrough: std::sync::Mutex<String>,
    clickthrough_active: std::sync::Mutex<String>,
    reset_windows: std::sync::Mutex<String>,
    quit: std::sync::Mutex<String>,
}

impl Default for TrayLabels {
    fn default() -> Self {
        Self {
            open_meter: std::sync::Mutex::new("Open Meter".into()),
            open_logs: std::sync::Mutex::new("Open Logs".into()),
            always_on_top: std::sync::Mutex::new("Always on top".into()),
            always_on_top_active: std::sync::Mutex::new("Always on top \u{2713}".into()),
            clickthrough: std::sync::Mutex::new("Clickthrough".into()),
            clickthrough_active: std::sync::Mutex::new("Clickthrough \u{2713}".into()),
            reset_windows: std::sync::Mutex::new("Reset Windows".into()),
            quit: std::sync::Mutex::new("Quit".into()),
        }
    }
}

/// Sender half of the live parser's reset channel. `None` until a parser is
/// connected; replaced on every reconnect (the parser is owned by the
/// pipe-reading task, so commands reach it through this channel).
struct ResetChannel(std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<()>>>);

#[tauri::command]
fn reset_encounter(state: State<ResetChannel>) {
    if let Some(tx) = state.0.lock().unwrap().as_ref() {
        let _ = tx.send(());
    }
}

/// Sender half of the live parser's republish channel. Same shape and lifetime
/// as [`ResetChannel`] — the parser is owned by the pipe-reading task, so a
/// command reaches it through a channel rather than a lock.
struct RepublishChannel(std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<()>>>);

/// Asks the live parser to re-send the encounter, the party and its verdicts.
///
/// The party and legality events are only emitted when they actually change, so
/// a meter that mounts mid-fight has already missed them. It calls this once on
/// mount to catch up; a meter that mounts outside a fight simply gets the empty
/// state it would have drawn anyway.
#[tauri::command]
fn request_live_republish(state: State<RepublishChannel>) {
    if let Some(tx) = state.0.lock().unwrap().as_ref() {
        let _ = tx.send(());
    }
}

/// The meter's damage-source filters, shared between the settings UI and the
/// live parser.
///
/// `current` is read when a parser is constructed so it starts correct however
/// the startup order falls out; `tx` tells an already-running parser to adopt a
/// new value, reparse and re-emit, which is what makes toggling apply to the
/// fight in progress rather than only to hits from here on.
#[derive(Default)]
struct MeterFilterState {
    current: std::sync::Mutex<v1::MeterFilters>,
    tx: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<v1::MeterFilters>>>,
}

#[tauri::command]
fn set_meter_filters(state: State<MeterFilterState>, filters: v1::MeterFilters) {
    {
        // A send costs the live parser a full replay of the fight so far, so an
        // unchanged value must not reach the channel. The frontend pushes on
        // mount as well as on change, which makes a redundant call the normal
        // case rather than the exceptional one.
        let mut current = state.current.lock().unwrap();
        if *current == filters {
            return;
        }
        *current = filters;
    }
    if let Some(tx) = state.tx.lock().unwrap().as_ref() {
        let _ = tx.send(filters);
    }
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn update_tray_labels(
    app: AppHandle,
    labels: State<TrayLabels>,
    always_on_top_state: State<AlwaysOnTop>,
    clickthrough_state: State<ClickThrough>,
    open_meter: String,
    open_logs: String,
    always_on_top: String,
    always_on_top_active: String,
    clickthrough: String,
    clickthrough_active: String,
    reset_windows: String,
    reload_hook: String,
    quit: String,
) {
    let tray = app.tray_handle();

    let aot_active = always_on_top_state.0.load(Ordering::Acquire);
    let ct_active = clickthrough_state.0.load(Ordering::Acquire);

    // `get_item` panics when the id isn't in the menu (tauri `app/tray.rs`:
    // `panic!("item id not found")`). This runs on the main thread inside the
    // webview IPC callback, so such a panic would unwind across FFI. The
    // fallible variant makes a missing item a no-op — which is what lets us
    // set `reload_hook` unconditionally even though the item only exists
    // under `cfg(all(windows, debug_assertions))`.
    let set = |id: &str, title: &str| {
        if let Some(item) = tray.try_get_item(id) {
            let _ = item.set_title(title);
        }
    };

    let aot_title = if aot_active {
        &always_on_top_active
    } else {
        &always_on_top
    };
    let ct_title = if ct_active {
        &clickthrough_active
    } else {
        &clickthrough
    };

    set("open_meter", &open_meter);
    set("open_logs", &open_logs);
    set("always_on_top", aot_title);
    set("toggle_clickthrough", ct_title);
    set("reset_windows", &reset_windows);
    set("reload_hook", &reload_hook);
    set("quit", &quit);

    // Cached so the toggles can re-render the ✓/no-✓ variants without a round
    // trip to the frontend. `reload_hook` has no toggle state, so it isn't
    // stored.
    let store = |slot: &std::sync::Mutex<String>, value: String| {
        if let Ok(mut v) = slot.lock() {
            *v = value;
        }
    };

    store(&labels.open_meter, open_meter);
    store(&labels.open_logs, open_logs);
    store(&labels.always_on_top, always_on_top);
    store(&labels.always_on_top_active, always_on_top_active);
    store(&labels.clickthrough, clickthrough);
    store(&labels.clickthrough_active, clickthrough_active);
    store(&labels.reset_windows, reset_windows);
    store(&labels.quit, quit);
}

/// Toolbox / Synthesis Helper: snapshot the game's synthesis state (served
/// by the hook over the toolbox RPC channel) and report whether predictions
/// are currently possible.
#[tauri::command(async)]
async fn fetch_synthesis_status(
    hook: State<'_, HookStatus>,
) -> Result<synthesis::SynthesisStatus, String> {
    match toolbox_rpc::synthesis_snapshot(&hook).await? {
        None => Ok(synthesis::SynthesisStatus {
            game_running: false,
            sigil_count: 0,
            rng_unpredictable: false,
            seed_latched: false,
        }),
        Some(snap) => Ok(synthesis::SynthesisStatus {
            game_running: true,
            sigil_count: snap.sigils.len() as u32,
            rng_unpredictable: snap.rng_state == 0,
            seed_latched: synthesis::seed_latched(&snap),
        }),
    }
}

/// Toolbox / Synthesis Helper: fresh snapshot + exhaustive pair search. The
/// snapshot is an RPC; the search itself is CPU-heavy, so it stays on a
/// blocking thread.
#[tauri::command(async)]
async fn search_synthesis(
    query: synthesis::SynthesisQuery,
    hook: State<'_, HookStatus>,
) -> Result<synthesis::SynthesisSearchResponse, String> {
    if query.trait1 == synthesis::EMPTY_TRAIT || query.trait2 == Some(synthesis::EMPTY_TRAIT) {
        return Err("invalid-trait".to_string());
    }
    let snap = toolbox_rpc::synthesis_snapshot(&hook)
        .await?
        .ok_or_else(|| "game-not-running".to_string())?;
    tokio::task::spawn_blocking(move || {
        let (matches, pairs_tested) = synthesis::search(&snap, &query);
        Ok(synthesis::SynthesisSearchResponse {
            matches,
            pairs_tested,
            sigil_count: snap.sigils.len() as u32,
            rng_unpredictable: snap.rng_state == 0,
            rng_state: snap.rng_state,
            saved_seed: snap.saved_seed,
            synth_count: synthesis::synth_count(&snap),
            seed_latched: synthesis::seed_latched(&snap),
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Toolbox / Synthesis Helper: current seed identity for staleness polling.
/// `None` = game not running (staleness unknowable, not stale).
#[tauri::command(async)]
async fn fetch_synthesis_seed(
    hook: State<'_, HookStatus>,
) -> Result<Option<synthesis::SynthesisSeed>, String> {
    toolbox_rpc::synthesis_seed(&hook).await
}

/// Compute and broadcast the current hook status to both windows. Called on
/// connect, disconnect, and around a refresh.
fn emit_hook_status(app: &AppHandle) {
    let snapshot = app.state::<HookStatus>().snapshot();
    let _ = app.emit_all("hook-status", &snapshot);
}

/// Re-run the handshake (with its retries) and broadcast the result. Shared by
/// the connect loop and the dev re-handshake so those two cannot drift; the
/// heartbeat folds through the same `apply_hello` but skips the retries (see
/// `hook_heartbeat`).
async fn handshake_and_apply(app: &AppHandle) {
    toolbox_rpc::handshake(&app.state::<HookStatus>()).await;
    emit_hook_status(app);
}

/// Periodic re-probe while the event stream is up.
///
/// The connect-time handshake is one shot, so without this ANY undelivered
/// handshake — a toolbox pipe that lost the startup race with the event pipe,
/// a busy pipe instance, a 2s timeout — pinned its verdict until the game
/// closed, because a fresh event-stream connection was the only other trigger.
/// One RPC per interval, and it only wakes the webviews when the derived
/// status actually changed.
///
/// Deliberately `hello` and not `handshake`: this loop IS the retry, so the
/// backoff ladder would only spend four RPCs (and up to ~10s) per tick to
/// reach the same verdict the next tick re-checks anyway.
/// Re-stat the hook DLL and publish the result if the derived status changed.
///
/// Antivirus quarantines this file (see `hook_dll`), and until it had a state
/// of its own the app reported the aftermath as "No game found" — the inject
/// error went to reload-debug.log and the UI claimed success regardless. Three
/// callers share this: startup (before the game is even running, when the user
/// can still act), the injection loop, and the heartbeat (which is what
/// notices a quarantine that happens mid-session). One `is_file` on a local
/// directory, so the heartbeat can afford it every tick.
#[cfg(windows)]
fn refresh_hook_dll_presence(app: &AppHandle) {
    let hook = app.state::<HookStatus>();
    let before = hook.snapshot();
    let found = gbfr_logs::hook_dll::injectable_hook();
    match &found {
        Some(path) => log::debug!("hook DLL present at {path:?}"),
        None => log::warn!(
            "no hook DLL in any of {:?} — almost always antivirus quarantine; \
             nothing can be injected until it is restored",
            gbfr_logs::hook_dll::search_dirs()
        ),
    }
    hook.dll_missing.store(found.is_none(), Ordering::Relaxed);
    if hook.snapshot() != before {
        emit_hook_status(app);
    }
}

async fn hook_heartbeat(app: AppHandle) {
    loop {
        tokio::time::sleep(toolbox_rpc::HEARTBEAT_INTERVAL).await;

        // Before the connected/gated early-out below: a quarantine that lands
        // while no game is running is exactly the case the user needs told,
        // and it is the case the RPC-only heartbeat could never see.
        #[cfg(windows)]
        refresh_hook_dll_presence(&app);

        let hook = app.state::<HookStatus>();
        // Nothing to ask when the stream is down. A teardown in flight is
        // EXPECTED to fail the RPC, so probing there would manufacture an
        // `Unresponsive` for a hook we are ourselves ejecting.
        if !hook.connected.load(Ordering::Relaxed) || hook.injection_gated() {
            continue;
        }

        let before = hook.snapshot();
        hook.apply_hello(toolbox_rpc::hello().await);
        if hook.snapshot() != before {
            emit_hook_status(&app);
        }
    }
}

#[tauri::command]
fn get_hook_status(hook: State<'_, HookStatus>) -> toolbox_rpc::HookStatusSnapshot {
    hook.snapshot()
}

/// Toolbox / Overmastery Predictor: is the game up, and which characters
/// exist in the roster (for the character picker).
#[tauri::command(async)]
async fn fetch_overmastery_status(
    hook: State<'_, HookStatus>,
) -> Result<overmastery::OvermasteryStatus, String> {
    match toolbox_rpc::overmastery_snapshot(&hook).await? {
        None => Ok(overmastery::OvermasteryStatus {
            game_running: false,
            roster: Vec::new(),
        }),
        Some(snap) => Ok(overmastery::OvermasteryStatus {
            game_running: true,
            roster: snap.roster,
        }),
    }
}

/// Most characters one batch prediction will simulate. The roster is far
/// smaller than this; the cap is only so a malformed query can't ask for an
/// unbounded pile of simulation.
const MAX_PREDICTED_CHARACTERS: usize = 64;

/// Toolbox / Overmastery Predictor: fresh RNG snapshot + simulate the next N
/// meditation rolls at one size, for each requested character.
///
/// The whole batch is simulated from a *single* snapshot: the per-character
/// results are shown side by side, so they must agree about the RNG state
/// they were computed from — and the staleness watch must be able to speak
/// for all of them at once. A per-character failure (someone the live roster
/// doesn't hold) is reported in that character's entry rather than failing
/// the batch; only the whole-read failures below are errors.
#[tauri::command(async)]
async fn predict_overmastery(
    query: overmastery::OvermasteryQuery,
    hook: State<'_, HookStatus>,
) -> Result<Vec<overmastery::OvermasteryCharacterPrediction>, String> {
    let tables = overmastery::stock_tables();
    if query.tier >= tables.tiers.len() {
        return Err("invalid-tier".to_string());
    }
    let rolls = query.rolls.min(500);
    let snap = toolbox_rpc::overmastery_snapshot(&hook)
        .await?
        .ok_or_else(|| "game-not-running".to_string())?;
    if snap.slot_override != u32::MAX {
        return Err("rng-override-active".to_string());
    }
    Ok(query
        .char_ids
        .iter()
        .take(MAX_PREDICTED_CHARACTERS)
        .map(|&char_id| {
            let predicted = overmastery::char_slot_index(&snap.roster, char_id)
                .ok_or("character-not-found")
                .and_then(|char_idx| {
                    let slot = overmastery::rng_slot(query.tier as u32, char_idx);
                    let slot_state = *snap.slots.get(slot as usize).ok_or("slot-out-of-range")?;
                    Ok(overmastery::OvermasteryPrediction {
                        rolls: overmastery::simulate(slot_state, query.tier, tables, rolls),
                        slot,
                        slot_state,
                        unpredictable: slot_state == 0,
                        msp_cost: tables.tiers[query.tier].msp_cost,
                    })
                });
            match predicted {
                Ok(prediction) => overmastery::OvermasteryCharacterPrediction {
                    char_id,
                    prediction: Some(prediction),
                    error: None,
                },
                Err(e) => overmastery::OvermasteryCharacterPrediction {
                    char_id,
                    prediction: None,
                    error: Some(e.to_string()),
                },
            }
        })
        .collect())
}

/// Toolbox: current RNG state of one slot, for staleness polling against a
/// prediction's `slot_state`. Shared by the Overmastery Predictor and the
/// Transmarvel Wishlist (both predictions carry their `slot`). `None` = game
/// not running (staleness unknowable, not stale).
#[tauri::command(async)]
async fn fetch_rng_slot(slot: u32, hook: State<'_, HookStatus>) -> Result<Option<u32>, String> {
    Ok(toolbox_rpc::rng_slot(&hook, slot).await?.map(|s| s.state))
}

/// Toolbox / Transmarvel Searcher: is the game up and the RNG predictable.
#[tauri::command(async)]
async fn fetch_transmarvel_status(
    hook: State<'_, HookStatus>,
) -> Result<transmarvel::TransmarvelStatus, String> {
    match toolbox_rpc::rng_slot(&hook, transmarvel::TM_SLOT).await? {
        None => Ok(transmarvel::TransmarvelStatus {
            game_running: false,
            rng_unpredictable: false,
        }),
        Some(snap) => Ok(transmarvel::TransmarvelStatus {
            game_running: true,
            rng_unpredictable: snap.state == 0,
        }),
    }
}

/// Toolbox / Transmarvel Searcher: fresh RNG snapshot + simulate the next N
/// transmarvel rolls.
#[tauri::command(async)]
async fn predict_transmarvel(
    query: transmarvel::TransmarvelQuery,
    hook: State<'_, HookStatus>,
) -> Result<transmarvel::TransmarvelPrediction, String> {
    // Mirrors the frontend's MAX_ROLLS (TransmarvelSearcher.tsx).
    let rolls = query.rolls.min(50_000);
    let snap = toolbox_rpc::rng_slot(&hook, transmarvel::TM_SLOT)
        .await?
        .ok_or_else(|| "game-not-running".to_string())?;
    if snap.slot_override != u32::MAX {
        return Err("rng-override-active".to_string());
    }
    Ok(transmarvel::TransmarvelPrediction {
        rolls: transmarvel::simulate(snap.state, transmarvel::stock_tables(), rolls),
        slot: transmarvel::TM_SLOT,
        slot_state: snap.state,
        unpredictable: snap.state == 0,
    })
}

#[tauri::command]
fn set_debug_mode(app: AppHandle, state: State<DebugMode>, enabled: bool) {
    if let Some(window) = app.get_window("logs") {
        if enabled {
            window.open_devtools()
        } else {
            window.close_devtools()
        }
    }

    state.0.store(enabled, Ordering::Release);
}

/// Config file the injected hook reads ONCE at startup. Portable: lives in
/// `<exe_dir>/portable_data/` next to the hook DLL, which resolves its own
/// directory at runtime via `__ImageBase`. On Linux the hook runs inside the
/// Proton prefix, so the path lands inside `pfx/drive_c/...` and we write it
/// there.
fn hook_config_path() -> Result<std::path::PathBuf, String> {
    #[cfg(not(target_os = "linux"))]
    let mut path = gbfr_logs::portable::portable_root();
    #[cfg(target_os = "linux")]
    let mut path = {
        use gbfr_logs::linux_support::steam;
        let home = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .ok_or("HOME is not set")?;
        steam::discover(&steam::default_steam_roots(&home))
            .ok_or("Could not find the game's Steam install")?
            .hook_data_dir
    };
    std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
    path.push("hook-config.json");

    Ok(path)
}

/// Whether the dev-only Infinity Full Assist unlock is armed for the next game launch.
#[tauri::command]
fn get_full_assist_unlock() -> Result<bool, String> {
    if !cfg!(debug_assertions) {
        return Ok(false);
    }

    let path = hook_config_path()?;
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return Ok(false);
    };

    let value: serde_json::Value = serde_json::from_str(&contents).map_err(|e| e.to_string())?;

    Ok(value
        .get("unlock_full_assist_infinity")
        .and_then(|v| v.as_bool())
        .unwrap_or(false))
}

/// Arm/disarm the unlock. Dev builds only — and even then it does nothing unless the hook
/// was built with the `fullassist` feature, which only `npm run dev` passes.
#[tauri::command]
fn set_full_assist_unlock(enabled: bool) -> Result<(), String> {
    if !cfg!(debug_assertions) {
        return Err("The Full Assist unlock is only available in development builds".into());
    }

    let path = hook_config_path()?;
    let contents = serde_json::json!({ "unlock_full_assist_infinity": enabled });

    std::fs::write(&path, contents.to_string()).map_err(|e| e.to_string())
}

/// Broadcast on every settings write so the other window can catch up.
/// `value` is `None` for a deletion. `origin` is filled in from the calling
/// window, never from the frontend, so a window cannot misreport it and
/// swallow its own real changes.
#[derive(Clone, serde::Serialize)]
struct SettingsChanged {
    key: String,
    value: Option<String>,
    origin: String,
}

const SETTINGS_CHANGED_EVENT: &str = "settings-changed";

/// Tell the *other* windows a setting changed.
///
/// Only ever called for a write that changed something. A window that hears a
/// change rehydrates, and zustand's rehydrate writes back through the same
/// adapter, so announcing a write that changed nothing is what would let two
/// windows echo each other without end. The originating window is filtered out
/// because it already has the value; the frontend drops its own `origin` too,
/// as defence in depth.
fn announce(window: &tauri::Window, key: String, value: Option<String>) {
    let origin = window.label().to_string();
    let payload = SettingsChanged {
        key,
        value,
        origin: origin.clone(),
    };

    let _ = window
        .app_handle()
        .emit_filter(SETTINGS_CHANGED_EVENT, payload, |w| w.label() != origin);
}

#[tauri::command]
fn get_settings() -> Result<std::collections::HashMap<String, String>, String> {
    settings_db::read_all().map_err(|e| e.to_string())
}

#[tauri::command]
fn set_setting(window: tauri::Window, key: String, value: String) -> Result<(), String> {
    if settings_db::write(&key, &value).map_err(|e| e.to_string())? {
        announce(&window, key, Some(value));
    }
    Ok(())
}

#[tauri::command]
fn delete_setting(window: tauri::Window, key: String) -> Result<(), String> {
    if settings_db::remove(&key).map_err(|e| e.to_string())? {
        announce(&window, key, None);
    }
    Ok(())
}

/// The dialog every settings/logs database import and export starts from.
fn db_file_dialog(title: &str) -> tauri::api::dialog::blocking::FileDialogBuilder {
    tauri::api::dialog::blocking::FileDialogBuilder::new()
        .set_title(title)
        .add_filter("SQLite database", &["db"])
}

/// One save dialog for a whole-database export: pick a path, clear any
/// existing file (`VACUUM INTO` refuses to overwrite, and the dialog already
/// asked), then run `export`. `None` when the dialog is cancelled.
fn export_db_via_dialog(
    file_name: &str,
    export: impl FnOnce(&std::path::Path) -> Result<(), String>,
) -> Result<Option<String>, String> {
    let Some(path) = db_file_dialog(&format!("Export {file_name}"))
        .set_file_name(file_name)
        .save_file()
    else {
        return Ok(None);
    };

    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| e.to_string())?;
    }

    export(&path)?;
    Ok(Some(path.to_string_lossy().into_owned()))
}

/// Save a copy of the settings store wherever the user picks. `None` when the
/// save dialog is cancelled.
#[tauri::command]
async fn export_settings_db() -> Result<Option<String>, String> {
    export_db_via_dialog("settings.db", |path| {
        settings_db::export_to(path).map_err(|e| e.to_string())
    })
}

/// Native file picker for a settings database to import. `None` means the
/// user cancelled. Async so the blocking dialog runs off the main thread.
#[tauri::command]
async fn pick_settings_db_file() -> Option<String> {
    db_file_dialog("Select the settings.db to import")
        .pick_file()
        .map(|path| path.to_string_lossy().into_owned())
}

/// Overwrite our settings with the rows of the settings database at `path`
/// (from [`pick_settings_db_file`]). Returns how many settings actually
/// changed.
///
/// Each changed key is announced on the normal settings-changed channel with a
/// non-window `origin`, so EVERY window — including the one that invoked this —
/// applies the imported values live.
#[tauri::command]
async fn import_settings_from_file(app: tauri::AppHandle, path: String) -> Result<u32, String> {
    let changed =
        settings_db::import_from(std::path::Path::new(&path)).map_err(|e| e.to_string())?;
    let count = changed.len() as u32;
    for (key, value) in changed {
        let _ = app.emit_all(
            SETTINGS_CHANGED_EVENT,
            SettingsChanged {
                key,
                value: Some(value),
                origin: "settings-import".into(),
            },
        );
    }
    Ok(count)
}

/// Emitted after every stored setting has been deleted. Windows respond by
/// dropping the listed keys from their caches and reloading — a reload rather
/// than a rehydrate, because rehydrating a store from a missing key leaves its
/// in-memory state as it was; only a fresh boot lands on the defaults.
#[derive(Clone, Serialize)]
struct SettingsReset {
    keys: Vec<String>,
}

const SETTINGS_RESET_EVENT: &str = "settings-reset";

/// Delete every stored setting and tell all windows to reload on defaults.
#[tauri::command]
async fn reset_settings_to_default(app: tauri::AppHandle) -> Result<(), String> {
    let keys = settings_db::reset_all().map_err(|e| e.to_string())?;
    let _ = app.emit_all(SETTINGS_RESET_EVENT, SettingsReset { keys });
    Ok(())
}

#[cfg(target_os = "linux")]
mod linux_setup {
    use serde::Serialize;
    use std::path::PathBuf;
    use tauri::AppHandle;

    use gbfr_logs::linux_support::{deploy, steam};

    #[derive(Debug, Clone, Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct LinuxSetupStatus {
        steam_found: bool,
        game_dir: Option<String>,
        prefix_found: bool,
        /// "missing" | "current" | "outdated" | "foreign"
        proxy_status: String,
        launch_options: String,
    }

    fn game_and_hook(app: &AppHandle) -> Result<(steam::SteamGame, PathBuf), String> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or("HOME is not set")?;
        let game = steam::discover(&steam::default_steam_roots(&home))
            .ok_or("Could not find the game's Steam install")?;
        let bundled = app
            .path_resolver()
            .resolve_resource("hook.dll")
            .ok_or("hook.dll resource missing from this build")?;
        Ok((game, bundled))
    }

    fn status_word(status: deploy::ProxyStatus) -> String {
        match status {
            deploy::ProxyStatus::Missing => "missing",
            deploy::ProxyStatus::Current => "current",
            deploy::ProxyStatus::Outdated => "outdated",
            deploy::ProxyStatus::Foreign => "foreign",
        }
        .into()
    }

    #[tauri::command]
    pub fn fetch_linux_setup_status(app: AppHandle) -> Result<LinuxSetupStatus, String> {
        let (game, bundled) = match game_and_hook(&app) {
            Ok(pair) => pair,
            Err(e) => {
                // "Steam not found" is the panel's normal empty state, but a
                // missing bundled resource is a packaging defect — keep the
                // real reason in the log.
                log::warn!("linux setup status unavailable: {e}");
                return Ok(LinuxSetupStatus {
                    steam_found: false,
                    game_dir: None,
                    prefix_found: false,
                    proxy_status: "missing".into(),
                    launch_options: deploy::LAUNCH_OPTIONS.into(),
                });
            }
        };
        let proxy = deploy::proxy_status(&game.game_dir, &bundled).map_err(|e| format!("{e:#}"))?;
        Ok(LinuxSetupStatus {
            steam_found: true,
            game_dir: Some(game.game_dir.display().to_string()),
            prefix_found: game.prefix_dir.is_dir(),
            proxy_status: status_word(proxy),
            launch_options: deploy::LAUNCH_OPTIONS.into(),
        })
    }

    #[tauri::command]
    pub fn deploy_linux_hook(app: AppHandle) -> Result<(), String> {
        let (game, bundled) = game_and_hook(&app)?;
        deploy::deploy(&game.game_dir, &bundled)
            .map(|_| ())
            .map_err(|e| format!("{e:#}"))
    }

    #[tauri::command]
    pub fn remove_linux_hook(app: AppHandle) -> Result<(), String> {
        let (game, _) = game_and_hook(&app)?;
        deploy::remove(&game.game_dir).map_err(|e| format!("{e:#}"))
    }
}

#[cfg(target_os = "linux")]
use linux_setup::{deploy_linux_hook, fetch_linux_setup_status, remove_linux_hook};

#[cfg(not(target_os = "linux"))]
#[tauri::command]
fn fetch_linux_setup_status() -> Result<(), String> {
    Err("linux-only".into())
}

#[cfg(not(target_os = "linux"))]
#[tauri::command]
fn deploy_linux_hook() -> Result<(), String> {
    Err("linux-only".into())
}

#[cfg(not(target_os = "linux"))]
#[tauri::command]
fn remove_linux_hook() -> Result<(), String> {
    Err("linux-only".into())
}

/// Native file picker for another installation's logs.db. `None` means the
/// user cancelled. Async so the blocking dialog runs off the main thread.
#[tauri::command]
async fn pick_logs_db_file() -> Option<String> {
    let dialog = db_file_dialog("Select the logs.db to import");

    // Start in the install this importer exists for when it's present,
    // otherwise fall back to Program Files, where installs live.
    #[cfg(windows)]
    let dialog = {
        let program_files = std::env::var_os("ProgramFiles").map(std::path::PathBuf::from);
        let start = program_files
            .as_ref()
            .map(|dir| dir.join("GBFR Logs Awa Edition"))
            .filter(|dir| dir.exists())
            .or_else(|| program_files.filter(|dir| dir.exists()));
        match start {
            Some(dir) => dialog.set_directory(dir),
            None => dialog,
        }
    };

    dialog
        .pick_file()
        .map(|path| path.to_string_lossy().into_owned())
}

/// Payload of the `import-progress` event both [`analyze_logs_db`] and
/// [`import_logs_from_file`] emit while working through a source logs.db.
#[derive(Clone, Serialize)]
struct ImportProgressPayload {
    processed: u32,
    total: u32,
}

/// A progress callback that emits `import-progress` to `window`, throttled to
/// 0.1% steps so a large source doesn't flood the webview with events. Both
/// passes report `(rows examined, total rows)`, so one emitter serves both.
fn import_progress_emitter(window: tauri::Window) -> impl FnMut(u32, u32) {
    let mut last_emitted: Option<u32> = None;
    move |processed, total| {
        let step = (total / 1000).max(1);
        let due = match last_emitted {
            None => true,
            Some(last) => processed >= last + step || processed == total,
        };
        if due {
            last_emitted = Some(processed);
            let _ = window.emit(
                "import-progress",
                ImportProgressPayload { processed, total },
            );
        }
    }
}

/// Dry-run a logs.db import: what would come across, and what data those logs
/// actually carry. Decoding every blob is real CPU time, so it runs on a
/// blocking thread rather than tying up an async-runtime worker the other
/// commands and event emission share; progress goes to the invoking window as
/// `import-progress` events.
#[tauri::command]
async fn analyze_logs_db(
    window: tauri::Window,
    path: String,
) -> Result<db::import::ImportAnalysis, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let conn = db::connect_to_db().map_err(|e| e.to_string())?;
        db::import::analyze_logs_db_file(
            &conn,
            std::path::Path::new(&path),
            import_progress_emitter(window),
        )
        .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Merge the logs.db at `path` (from [`pick_logs_db_file`]) into ours, on a
/// blocking thread for the same reason as [`analyze_logs_db`]. Progress goes
/// to the invoking window as `import-progress` events.
#[tauri::command]
async fn import_logs_from_file(
    app: tauri::AppHandle,
    window: tauri::Window,
    path: String,
) -> Result<db::import::ImportSummary, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let conn = db::connect_to_db().map_err(|e| e.to_string())?;
        let summary = db::import::import_logs_from_file(
            &conn,
            std::path::Path::new(&path),
            import_progress_emitter(window),
        )
        .map_err(|e| e.to_string())?;

        // An import writes rows and can reuse rowids freed by an earlier delete
        // — see `forget_decoded_logs`.
        forget_decoded_logs();

        // Imported rows arrive unstamped; judge them now rather than making the
        // cheat audit wait for the next launch. Same background sweep as startup,
        // so the audit page's progress banner covers this pass too.
        if summary.imported > 0 {
            std::thread::spawn(move || sweep_stale_legality(&app));
        }

        Ok(summary)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// How many logs arrived via the importer — what [`delete_imported_logs`]
/// would delete, so the confirmation dialog can say so.
#[tauri::command]
async fn count_imported_logs() -> Result<u32, String> {
    let conn = db::connect_to_db().map_err(|e| e.to_string())?;
    conn.query_row("SELECT COUNT(*) FROM logs WHERE imported = 1", [], |r| {
        r.get(0)
    })
    .map_err(|e| e.to_string())
}

/// Delete every log that arrived via the importer, returning how many went.
/// Logs recorded by this installation are untouched.
#[tauri::command]
async fn delete_imported_logs() -> Result<u32, String> {
    let conn = db::connect_to_db().map_err(|e| e.to_string())?;
    let deleted = conn
        .execute("DELETE FROM logs WHERE imported = 1", [])
        .map_err(|e| e.to_string())?;
    // Verdicts go with their logs — see delete_logs.
    db::legality::sweep_orphaned_findings(&conn).map_err(|e| e.to_string())?;
    // And so do decoded logs — see delete_logs.
    forget_decoded_logs();
    Ok(deleted as u32)
}

/// Save a copy of the whole log store wherever the user picks. `None` when
/// the save dialog is cancelled.
#[tauri::command]
async fn export_logs_db() -> Result<Option<String>, String> {
    export_db_via_dialog("logs.db", |path| {
        let conn = db::connect_to_db().map_err(|e| e.to_string())?;
        db::export_to(&conn, path).map_err(|e| e.to_string())
    })
}

#[tauri::command]
async fn delete_all_logs() -> Result<(), String> {
    let conn = db::connect_to_db().map_err(|e| e.to_string())?;
    conn.execute("DELETE FROM logs", [])
        .map_err(|e| e.to_string())?;
    // Verdicts go with their logs — see delete_logs.
    db::legality::sweep_orphaned_findings(&conn).map_err(|e| e.to_string())?;
    // Deleting every log leaves every Conflux run roomless — drop them too so the
    // Conflux tab doesn't keep showing ghost "×0 rooms" runs.
    conn.execute("DELETE FROM runs", [])
        .map_err(|e| e.to_string())?;
    // And so do decoded logs — see delete_logs.
    forget_decoded_logs();
    Ok(())
}

#[tauri::command]
fn export_damage_log_to_file(id: u32, options: ParseOptions) -> Result<(), String> {
    let file_path = FileDialogBuilder::new()
        .add_filter("csv", &["csv"])
        .set_file_name(&format!("{id}_damage_log.csv"))
        .set_title("Export Damage Log")
        .save_file()
        .ok_or("No file selected!")?;

    let conn = db::connect_to_db().map_err(|e| e.to_string())?;

    let mut stmt = conn
        .prepare("SELECT data, version FROM logs WHERE id = ?")
        .map_err(|e| e.to_string())?;

    let (blob, version): (Vec<u8>, u8) = stmt
        .query_row([id], |row| Ok((row.get(0)?, row.get(1)?)))
        .context("Failed to fetch log from database")
        .map_err(|e| e.to_string())?;

    let parser = parser::deserialize_version(&blob, version).map_err(|e| e.to_string())?;

    let file = File::create(file_path).map_err(|e| e.to_string())?;

    // @TODO(false): Split formatting into a separate function.
    let mut writer = std::io::BufWriter::new(file);

    writeln!(
        writer,
        "timestamp,source_type,child_source_type,source_index,target_type,target_index,action_id,flags,damage"
    )
    .map_err(|e| e.to_string())?;

    for (event_ts, event) in parser.encounter.event_log() {
        if let Message::DamageEvent(damage_event) = event {
            let timestamp = event_ts - parser.start_time();
            let target_type = EnemyType::from_hash(damage_event.target.parent_actor_type);
            let parent_character_type =
                CharacterType::from_hash(damage_event.source.parent_actor_type);
            let child_character_type = CharacterType::from_hash(damage_event.source.actor_type);

            // Honour the scrub window too, not just the target filter: the export is
            // meant to be what the view shows, and exporting from a windowed view used to
            // silently write the whole fight.
            let inside_window = options.from_ms.map_or(true, |from| timestamp >= from)
                && options.up_to_ms.map_or(true, |up_to| timestamp <= up_to);

            // Unlike the meter paths this walks the raw log directly rather than
            // reparsing, so the filter has to be applied here by hand — an
            // export is meant to be what the view shows, and a CSV that still
            // contained rows the table excluded would not add up to it.
            //
            // The damage-TAKEN stream is excluded for the same reason every
            // other dealt-damage path excludes it: this file has one `damage`
            // column and no side, so an enemy→party hit would land beside the
            // party's own with a `source_type` of `Unknown(...)`, and summing
            // the column — which is the whole point of the export — would
            // overstate party damage by the entire incoming stream.
            if inside_window
                && !v1::is_damage_taken_event(damage_event)
                && !v1::is_excluded(damage_event, &options.filters)
                && v1::target_selected(timestamp, damage_event, &options.target_spans)
            {
                writeln!(
                    writer,
                    "{},{},{},{},{},{},{},{},{}",
                    timestamp,
                    parent_character_type,
                    child_character_type,
                    damage_event.source.parent_index,
                    target_type,
                    damage_event.target.parent_index,
                    damage_event.action_id,
                    damage_event.flags,
                    damage_event.damage
                )
                .map_err(|e| e.to_string())?;
            }
        }
    }

    writer.flush().map_err(|e| e.to_string())?;

    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchResult {
    logs: Vec<LogEntry>,
    page: u32,
    page_count: u32,
    log_count: i32,
    /// IDs of the enemies that can be filtered by.
    enemy_ids: Vec<u32>,
    /// IDs of the quests that can be filtered by.
    quest_ids: Vec<u32>,
    /// Names of the Players that can be filtered by.
    player_ids: Vec<String>,
    /// Names of the Characters that can be filtered by.
    player_types: Vec<String>,
    /// Stored legality verdicts for the logs on this page, keyed by log id.
    /// Sent with the page rather than fetched separately so a row and its
    /// verdicts can never be a render apart — and only for the ten rows drawn,
    /// not the whole table the audit page reads. Logs nobody was flagged in are
    /// absent; the quest list reads a miss as clean.
    legality: HashMap<i64, Vec<db::legality::StoredFinding>>,
}

/// The whole log library, unpaginated, for the picker's client-side search.
/// Kept separate from [`fetch_logs`] rather than an "unpaginated" flag on it —
/// the two return different shapes ([`db::logs::LogSummary`] is deliberately
/// narrower than [`db::logs::LogEntry`]) and want different callers.
///
/// Plain `fn`, so Tauri runs this synchronously on the IPC thread — the same
/// as every other DB reader here. If a large library ever makes that a visible
/// hitch, the fix is `#[tauri::command(async)]` (used 11 times in this file
/// already), not a `LIMIT`: a limit would silently truncate the library the
/// picker searches, breaking client-side search in a way the user can't see.
#[tauri::command]
fn fetch_log_summaries() -> Result<Vec<db::logs::LogSummary>, String> {
    let conn = db::connect_to_db().map_err(|e| e.to_string())?;
    db::logs::fetch_log_summaries(&conn).map_err(|e| e.to_string())
}

#[tauri::command]
fn fetch_logs(
    page: Option<u32>,
    filter_by_enemy_id: Option<u32>,
    filter_by_quest_id: Option<u32>,
    sort_direction: Option<String>,
    sort_type: Option<String>,
    quest_completed: Option<bool>,
    filter_by_player_id: Option<String>,
    filter_by_player_character: Option<String>,
    flagged_only: Option<bool>,
) -> Result<SearchResult, String> {
    // Absent means "no legality filter" — the quest list only ever asks for the
    // flagged runs or for all of them.
    let flagged_only = flagged_only.unwrap_or(false);
    let conn = db::connect_to_db().map_err(|e| e.to_string())?;
    let page = page.unwrap_or(1);
    let per_page = 10;
    let offset = page.saturating_sub(1) * per_page;

    let sort_type_param = sort_type
        .map(|s| match s.as_str() {
            "time" => db::logs::SortType::Time,
            "duration" => db::logs::SortType::Duration,
            "quest-elapsed-time" => db::logs::SortType::QuestElapsedTime,
            _ => db::logs::SortType::Time,
        })
        .unwrap_or(db::logs::SortType::Time);

    let sort_direction_param = sort_direction
        .map(|s| match s.as_str() {
            "asc" => db::logs::SortDirection::Ascending,
            _ => db::logs::SortDirection::Descending,
        })
        .unwrap_or(db::logs::SortDirection::Descending);

    let filters = db::logs::LogFilters {
        enemy_id: filter_by_enemy_id,
        quest_id: filter_by_quest_id,
        cleared: quest_completed,
        player_id: &filter_by_player_id,
        player_character: &filter_by_player_character,
        flagged_only,
    };

    let logs = db::logs::get_logs(
        &conn,
        &filters,
        per_page,
        offset,
        &sort_type_param,
        &sort_direction_param,
    )
    .map_err(|e| e.to_string())?;

    let counts = db::logs::get_counts(&conn, &filters).map_err(|e| e.to_string())?;
    let log_count = counts.logs;

    let page_ids: Vec<i64> = logs.iter().map(|log| log.id()).collect();
    let legality = db::legality::findings_for_logs(&conn, &page_ids).map_err(|e| e.to_string())?;

    // Pages hold a fixed number of CHAINS, not of rows, so a Repeat Quest chain
    // is never cut across a page boundary — see `page_chain_keys`. `log_count`
    // stays the run count: it is the "N saved" figure, which is about the logs.
    let page_count = (counts.chains as f64 / per_page as f64).ceil() as u32;

    let mut enemy_ids = Vec::new();
    let mut quest_ids = Vec::new();
    let mut player_ids = Vec::new();
    let mut player_types = Vec::new();

    let mut query = conn
        .prepare("SELECT primary_target, quest_id, p1_name, p1_type, p2_name, p2_type, p3_name, p3_type, p4_name, p4_type from logs WHERE run_id IS NULL")
        .map_err(|e| e.to_string())?;

    let rows = query
        .query_map([], |row| {
            Ok((
                row.get::<usize, Option<u32>>(0)?,    // primary_target
                row.get::<usize, Option<u32>>(1)?,    // quest_id
                row.get::<usize, Option<String>>(2)?, // p1_name
                row.get::<usize, Option<String>>(3)?, // p1_type
                row.get::<usize, Option<String>>(4)?, // p2_name
                row.get::<usize, Option<String>>(5)?, // p2_type
                row.get::<usize, Option<String>>(6)?, // p3_name
                row.get::<usize, Option<String>>(7)?, // p3_type
                row.get::<usize, Option<String>>(8)?, // p4_name
                row.get::<usize, Option<String>>(9)?, // p4_type
            ))
        })
        .map_err(|e| e.to_string())?;

    for row in rows {
        let (
            primary_target,
            quest_id,
            p1_name,
            p1_type,
            p2_name,
            p2_type,
            p3_name,
            p3_type,
            p4_name,
            p4_type,
        ) = row.map_err(|e| e.to_string())?;

        if let Some(primary_target) = primary_target {
            if !enemy_ids.contains(&primary_target) {
                enemy_ids.push(primary_target);
            }
        }

        // Through the SAME display rule `get_logs` files each row under, or the
        // filter offers ids no row is labelled with and withholds the one every
        // dummy row IS labelled with — leaving that branch of
        // `apply_log_filters` unreachable from this dropdown.
        if let Some(quest_id) = db::logs::quest_id_for_display(quest_id, primary_target) {
            if !quest_ids.contains(&quest_id) {
                quest_ids.push(quest_id);
            }
        }

        for p_name in [p1_name, p2_name, p3_name, p4_name] {
            if let Some(p_name) = p_name {
                if !player_ids.contains(&p_name) {
                    player_ids.push(p_name)
                }
            }
        }

        for p_type in [p1_type, p2_type, p3_type, p4_type] {
            if let Some(p_type) = p_type {
                if !player_types.contains(&p_type) {
                    player_types.push(p_type)
                }
            }
        }
    }

    Ok(SearchResult {
        logs,
        page,
        page_count,
        log_count,
        enemy_ids,
        quest_ids,
        player_ids,
        player_types,
        legality,
    })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfluxSearchResult {
    runs: Vec<db::runs::ConfluxRun>,
    page: u32,
    page_count: u32,
    run_count: i32,
}

#[tauri::command]
fn fetch_conflux_runs(page: Option<u32>) -> Result<ConfluxSearchResult, String> {
    let conn = db::connect_to_db().map_err(|e| e.to_string())?;
    let page = page.unwrap_or(1);
    let per_page = 10u32;
    let offset = page.saturating_sub(1) * per_page;

    let runs = db::runs::get_runs(&conn, per_page, offset).map_err(|e| e.to_string())?;
    let run_count = db::runs::get_runs_count(&conn).map_err(|e| e.to_string())?;
    let page_count = (run_count as f64 / per_page as f64).ceil() as u32;

    Ok(ConfluxSearchResult {
        runs,
        page,
        page_count,
        run_count,
    })
}

/// How far the startup legality sweep has got, for the audit page's banner.
/// `done == total` means it has finished.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
struct LegalitySweepProgress {
    done: usize,
    total: usize,
}

/// Re-audits every log the current rules have not judged, reporting progress to
/// the logs window.
///
/// Errors are logged, never surfaced: a sweep that fails leaves the stored
/// findings exactly as they were, and the next launch tries again. It must not
/// be able to take the app down with it.
fn sweep_stale_legality(app: &tauri::AppHandle) {
    let mut conn = match db::connect_to_db() {
        Ok(conn) => conn,
        Err(error) => {
            log::warn!("legality sweep: could not open the database: {error:#}");
            return;
        }
    };

    // Emitted at most once a second: a 692-log sweep would otherwise push
    // hundreds of events at a window that only renders a percentage.
    let mut last_emit = std::time::Instant::now();
    let result = legality::sweep::sweep_stale_logs(&mut conn, |progress| {
        let finished = progress.done == progress.total;
        if finished || last_emit.elapsed() >= std::time::Duration::from_secs(1) {
            last_emit = std::time::Instant::now();
            let _ = app.emit_to(
                "logs",
                "legality-sweep-progress",
                LegalitySweepProgress {
                    done: progress.done,
                    total: progress.total,
                },
            );
        }
    });

    match result {
        Ok(outcome) if outcome.rescanned > 0 || outcome.unreadable > 0 || outcome.skipped > 0 => {
            info!(
                "legality sweep: re-audited {} logs ({} unreadable, {} older than the audit cutoff)",
                outcome.rescanned, outcome.unreadable, outcome.skipped
            );
        }
        Ok(_) => {}
        Err(error) => log::warn!("legality sweep failed: {error:#}"),
    }
}

#[derive(Debug, Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct EncounterStateResponse {
    encounter_state: v1::DerivedEncounterState,
    players: [Option<PlayerData>; 4],
    quest_id: Option<u32>,
    quest_timer: Option<u32>,
    quest_completed: bool,
    /// 0-based room index when this log is a Conflux room, else None. Lets the
    /// detail view suppress quest-status/elapsed-time (meaningless for a room).
    room_index: Option<u32>,
    /// Copied in from another installation's logs.db, so it may lack data the
    /// source app never recorded — the detail view marks it.
    imported: bool,
    /// Stored build-legality findings, one vector per party slot so it lines up
    /// with `players` by index. Read from the table rather than recomputed:
    /// the startup sweep keeps them current, and opening a log must not pay
    /// for an audit.
    legality: [Vec<legality::Finding>; 4],
    /// Per-spawn selectable targets for the filter dropdown, in first-hit
    /// order — 1:1 with the HP chart's series (same instance numbers).
    target_entries: Vec<v1::TargetSegment>,
    /// Every distinct (source, target, ability) combination inside the current
    /// window, with the selector pins NOT applied — the analysis view cascades
    /// its three selectors from this without a round trip per keystroke.
    selection_facts: Vec<v1::SelectionFact>,
    /// Every window an actor held a status effect for, per actor and never
    /// merged, spanning the FULL fight. The Buffs and Debuffs tables compute
    /// their own uptime from these, so a scrub window narrows the view without
    /// another round trip. Empty on logs recorded before status capture.
    status_intervals: Vec<v1::StatusInterval>,
    /// Spans of fight time a battle state held (SBA performance, Link Time, an
    /// enemy's Break), for the analysis chart's shaded windows. Empty on logs
    /// recorded before the transition events existed.
    chart_windows: Vec<v1::ChartWindow>,
    dps_chart: HashMap<u32, Vec<i32>>,
    /// Stun applied per DPS-chart bucket. Separate from `dps_chart` because
    /// stun reconciles two capture paths with max(), not a sum — see
    /// `build_player_stun_chart`.
    stun_chart: HashMap<u32, Vec<f64>>,
    /// Damage TAKEN per DPS-chart bucket, keyed by the victim's slot key. All
    /// zeroes on logs recorded before damage-taken capture (2026-08-04).
    taken_chart: HashMap<u32, Vec<i64>>,
    /// Enemy HP% per DPS-chart bucket, one series per HP pool passing the target
    /// filter (largest pools first, capped). Empty on logs recorded before HP capture.
    hp_chart: Vec<v1::HpChartSeries>,
    /// The (filters × groupBy) aggregates for `group_query` — table rows and
    /// chart bands from ONE grouping. Empty when no query was sent.
    groups: Vec<v1::GroupAggregate>,
    /// The same grouping over the WHOLE fight, for the chart's band colours
    /// (see [`v1::aggregate_group_reference`]). Empty when the query narrows no
    /// time, which the view reads as "rank by the aggregates you were sent".
    group_reference: Vec<v1::GroupReference>,
    sba_chart: HashMap<u32, Vec<f32>>,
    /// Per-ability bands for the DRILLED Stun/SBA charts, keyed by player slot.
    /// Empty unless the view sent an `ability_series` query — an undrilled log
    /// pays nothing for this.
    ability_series: HashMap<u32, Vec<v1::AbilitySeries>>,
    sba_events: Vec<(i64, protocol::Message)>,
    death_events: Vec<(i64, protocol::Message)>,
    chart_len: usize,
    sba_chart_len: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ParseOptions {
    /// Per-spawn segment spans to filter by (empty = all). Spans, not bare
    /// ids: the game reuses freed instance ids across summon waves, so a
    /// selection is "this id DURING this window".
    #[serde(default)]
    target_spans: Vec<v1::TargetSpan>,
    /// Scrubber cutoff relative to the first event (ms): the derived meter state
    /// reflects the fight up to this moment. Absent/None = the full fight.
    #[serde(default)]
    up_to_ms: Option<i64>,
    /// When true, skip chart/segment/event-list building and return those
    /// fields empty — scrub commits only consume `encounter_state` and keep
    /// their charts from the full fetch, so rebuilding the rest per brush
    /// release is wasted work (and its per-player rows would be wrong anyway:
    /// they'd exist only for players active inside the window).
    #[serde(default)]
    state_only: bool,
    /// Scrubber window start relative to the first event (ms); pairs with
    /// `up_to_ms` so the derived meter state covers only `[from_ms, up_to_ms]`.
    /// Absent/None = from the start of the fight.
    #[serde(default)]
    from_ms: Option<i64>,
    /// Which contested damage sources to count (see [`v1::MeterFilters`]).
    /// Absent means the default, which excludes: a caller that has not opted in
    /// must never silently get contested damage folded into its totals.
    #[serde(default)]
    filters: v1::MeterFilters,
    /// Which source actors and abilities the selector bar has pinned. Absent or
    /// empty means "All" on every dimension — the shipped default.
    #[serde(default)]
    selection: v1::SelectionFilter,
    /// The analysis view's generic aggregation request; None = no groups in
    /// the response. Replaces the pin-shaped drill-chart triggers.
    #[serde(default)]
    group_query: Option<v1::GroupQuery>,
    /// The analysis view's window-filter mask for the DERIVED state — the
    /// same shape and semantics as [`v1::GroupQuery::windows`] (which masks
    /// the groups aggregation independently; the frontend sends the same
    /// combined mask to both so a table and its hover cards agree).
    #[serde(default)]
    windows: Option<Vec<v1::TimeWindow>>,
    /// The analysis view's per-ability chart request; None = no bands in the
    /// response.
    ///
    /// Rides this fetch rather than a command of its own: the walk is one extra
    /// linear pass over a `raw_event_log` this call has already decompressed and
    /// reparsed, and a drill changes the pins — which refetches anyway.
    #[serde(default)]
    ability_series: Option<AbilitySeriesQuery>,
}

/// Which derived-path metric the per-ability bands are built for.
///
/// Only the two the group aggregation cannot serve: damage and taken already
/// have bands through `group_query`.
#[derive(Debug, Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "camelCase")]
enum AbilitySeriesMetric {
    Stun,
    Sba,
}

/// The analysis view's per-ability chart request, sent only when a derived-path
/// tab is drilled in (`groupBy === "ability"`).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AbilitySeriesQuery {
    metric: AbilitySeriesMetric,
    /// The pinned player, or None for the whole party — stun's ability level
    /// widens to the party when no source is pinned (see `metrics/stun.ts`).
    #[serde(default)]
    player: Option<u32>,
}

// Per-second buckets: the quest-details charts and the window scrubber both
// work in whole seconds, so a bucket index IS the elapsed second.
const DPS_INTERVAL: i64 = 1_000;
const SBA_INTERVAL: i64 = 1_000;

/// The analysis view's generic (filters × groupBy) aggregation, present only
/// when the view sent a `group_query` — absent, this is a query-free fetch
/// (e.g. a scrub commit) and the response carries no groups at all.
///
/// `segments`/`assignment` are the caller's segmentation of the UNWINDOWED
/// log, passed in rather than recomputed (the caller already needs it for
/// `selection_facts`): sharing it is what makes an aggregate's spawn key and
/// a target-dropdown entry mean the same enemy.
/// The group aggregation for this request, or NO bands.
///
/// A rejected query is never fatal to the fetch. Every other stale-reference
/// case in this path narrows to nothing — an out-of-range spawn segment
/// aggregates to `Ok(vec![])`, an ability pin that expands to no action ids
/// matches nothing — and a `GroupQueryError` is the same kind of thing: a pin
/// left over from a hostility flip or a hand-edited URL. Propagating it would
/// fail the whole command, so the log page would lose its party table, its
/// charts, its status intervals and its legality verdicts over one bad
/// selector. Logged rather than swallowed, because a query the frontend should
/// never have built is still a bug worth seeing.
fn build_groups(
    parser: &v1::Parser,
    options: &ParseOptions,
    segments: &[v1::TargetSegment],
    assignment: &[Option<usize>],
) -> Vec<v1::GroupAggregate> {
    let built = match &options.group_query {
        Some(query) => v1::aggregate_groups(
            &parser.encounter.raw_event_log,
            &parser.encounter.player_data,
            segments,
            assignment,
            query,
            parser.start_time(),
            DPS_INTERVAL,
            (parser.full_log_duration() / DPS_INTERVAL) as usize + 1,
            options.filters,
        )
        .map_err(|e| format!("group query rejected: {e}")),
        None => Ok(Vec::new()),
    };

    built.unwrap_or_else(|error| {
        log::warn!("{error} — the response carries no group aggregates");
        Vec::new()
    })
}

/// The whole-fight ranking behind the chart's band colours, or none.
///
/// Degrades exactly like [`build_groups`], and for a smaller stake: without a
/// reference the chart falls back to ranking by the aggregates it was sent,
/// which is what it drew before this existed. Never worth failing a fetch over.
fn build_group_reference(
    parser: &v1::Parser,
    options: &ParseOptions,
    segments: &[v1::TargetSegment],
    assignment: &[Option<usize>],
) -> Vec<v1::GroupReference> {
    let Some(query) = &options.group_query else {
        return Vec::new();
    };

    v1::aggregate_group_reference(
        &parser.encounter.raw_event_log,
        &parser.encounter.player_data,
        segments,
        assignment,
        query,
        parser.start_time(),
        DPS_INTERVAL,
        (parser.full_log_duration() / DPS_INTERVAL) as usize + 1,
        options.filters,
    )
    .unwrap_or_else(|e| {
        log::warn!("group reference rejected: {e} — the chart ranks its own bands");
        Vec::new()
    })
}

/// The per-ability chart bands for this request, or none.
///
/// Like [`build_groups`], a request that resolves to nothing is never fatal to
/// the fetch: a pin left over from a metric switch narrows to an empty map, and
/// losing the whole log page over one stale selector would be far worse than a
/// chart falling back to its per-player lines.
fn build_ability_series(
    parser: &v1::Parser,
    options: &ParseOptions,
    player_indices: &[u32],
) -> HashMap<u32, Vec<v1::AbilitySeries>> {
    let Some(query) = &options.ability_series else {
        return HashMap::new();
    };

    // Sized from the FULL log like every other chart, not from the scrub
    // window: these walk the whole event log, and the view slices client-side.
    let chart_len = (parser.full_log_duration() / DPS_INTERVAL) as usize + 1;

    let wanted: Vec<u32> = match query.player {
        Some(player) => player_indices
            .iter()
            .copied()
            .filter(|index| *index == player)
            .collect(),
        None => player_indices.to_vec(),
    };
    if wanted.is_empty() {
        return HashMap::new();
    }

    // ONE scope for both, built from the request — see `v1::ChartScope`. A
    // builder cannot quietly honour a different subset of these than its
    // neighbour, which is how the ability charts once ignored the ability pin.
    let scope = v1::ChartScope {
        player_data: &parser.encounter.player_data,
        target_spans: &options.target_spans,
        abilities: &options.selection.abilities,
        filters: options.filters,
    };

    match query.metric {
        AbilitySeriesMetric::Stun => v1::build_ability_stun_chart(
            &parser.encounter.raw_event_log,
            &wanted,
            parser.start_time(),
            DPS_INTERVAL,
            chart_len,
            &scope,
        ),
        AbilitySeriesMetric::Sba => v1::build_ability_sba_chart(
            &parser.encounter.raw_event_log,
            &wanted,
            parser.start_time(),
            DPS_INTERVAL,
            chart_len,
            &scope,
        ),
    }
}

/// `(async)` for the same reason as [`fetch_encounter_state`]: this decompresses
/// and deserialises the stored blob, which must not run on the main thread.
///
/// `deserialize_encounter`, NOT `deserialize_version`: the events table reads the
/// RAW log, so replaying it through the derived-state machinery would be pure
/// waste. It repopulates a legacy log's event vector itself.
#[tauri::command(async)]
fn fetch_encounter_events(id: u64, offset: usize, count: usize) -> Result<v1::EventPage, String> {
    let conn = db::connect_to_db().map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare("SELECT data, version FROM logs WHERE id = ?")
        .map_err(|e| e.to_string())?;
    let (blob, version): (Vec<u8>, u8) = stmt
        .query_row([id], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(|e| e.to_string())?;

    let encounter = parser::deserialize_encounter(&blob, version).map_err(|e| e.to_string())?;
    // The same rebasing origin `Parser::start_time` uses — the first event in the
    // log — so a row's timestamp lines up with the chart's own axis.
    let start_time = encounter
        .raw_event_log
        .first()
        .map(|(ts, _)| *ts)
        .unwrap_or_default();

    Ok(v1::event_page(
        &encounter.raw_event_log,
        &encounter.player_data,
        start_time,
        offset,
        count,
    ))
}

/// One log, decoded once and reused by every request against it.
///
/// Everything in here is a function of the STORED BLOB alone — nothing a pin, a
/// window, a metric or a filter can change — which is exactly why it is worth
/// keeping. The parser is reused IN PLACE rather than cloned: the first thing
/// `reparse_with_options` does is `derived_state = Default::default()`, so a
/// request cannot see what the last one left behind, and `filters`/`selection`
/// are runtime fields both set before every reparse.
struct CachedLog {
    parser: v1::Parser,
    /// The spawn segmentation over the UNWINDOWED raw log — the one thing every
    /// caller here has to share, because a `SelectionFact`'s `target_segment`
    /// and a `GroupKey::EnemySpawn` both index this very vector (see
    /// `aggregate_groups`). Cached because it depends only on the log, and it
    /// was being recomputed twice per request on top of the copy
    /// `reparse_with_options` makes internally.
    segments: Vec<v1::TargetSegment>,
    assignment: Vec<Option<usize>>,
    /// From the log's own row, and immutable per log like the blob beside it.
    room_index: Option<u32>,
    imported: bool,
}

/// How many decoded logs to keep.
///
/// Deliberately small: an entry is a whole decompressed event log, and the view
/// reads at most a couple at a time (a comparison holds two, and flicking
/// between runs wants a little room behind them). The cost of a miss is what it
/// always was, so this errs low.
const DECODED_LOG_CAPACITY: usize = 4;

fn decoded_logs() -> &'static Mutex<gbfr_logs::log_cache::LogCache<CachedLog>> {
    static CACHE: OnceLock<Mutex<gbfr_logs::log_cache::LogCache<CachedLog>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(gbfr_logs::log_cache::LogCache::new(DECODED_LOG_CAPACITY)))
}

/// The cache, recovering from a poisoned lock rather than failing every request
/// after one panic. Nothing in the cache is a partial write worth protecting —
/// it is a map of ids to decoded logs, and the worst a panicking request can
/// leave behind is an entry that will be reparsed from scratch by the next one.
fn lock_decoded_logs() -> std::sync::MutexGuard<'static, gbfr_logs::log_cache::LogCache<CachedLog>>
{
    decoded_logs()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Drop every decoded log.
///
/// Called by anything that WRITES the log store. Deletion is the one that makes
/// it mandatory rather than merely tidy: the `logs` table has no `AUTOINCREMENT`,
/// so SQLite recycles the rowid of a deleted log, and a cached entry under that
/// id would then serve one fight's events under the next one's name — the same
/// hazard `sweep_orphaned_findings` exists for.
fn forget_decoded_logs() {
    lock_decoded_logs().clear();
}

/// Read one log out of the database and decode it. Runs with NO cache lock held
/// — it opens a connection and decompresses megabytes, and holding the lock
/// across that would serialise every pane behind the first miss.
fn decode_log(id: u64) -> Result<CachedLog, String> {
    let conn = db::connect_to_db().map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare("SELECT data, version, room_index, imported FROM logs WHERE id = ?")
        .map_err(|e| e.to_string())?;

    let (blob, version, room_index, imported): (Vec<u8>, u8, Option<u32>, bool) = stmt
        .query_row([id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .map_err(|e| e.to_string())?;

    // @TODO(false): If we deserialize from an older version, we should save it back into the DB as the newer format.
    let parser = parser::deserialize_version(&blob, version).map_err(|e| e.to_string())?;
    let (segments, assignment) =
        v1::segment_targets_indexed(&parser.encounter.raw_event_log, parser.start_time());

    Ok(CachedLog {
        parser,
        segments,
        assignment,
        room_index,
        imported,
    })
}

/// The decoded log for `id`, from the cache or freshly read.
///
/// Two panes can both miss and both decode — the read above deliberately runs
/// unlocked — and `insert` settles that by keeping whichever landed first, so
/// they end up sharing one entry rather than reparsing each other's work away.
/// The generation captured before that unlocked read guards a rarer case: a
/// `forget_decoded_logs` landing mid-decode, which must not let this decode's
/// now-stale result back into the cache once it finishes.
fn decoded_log(id: u64) -> Result<Arc<Mutex<CachedLog>>, String> {
    let mut cache = lock_decoded_logs();
    if let Some(cached) = cache.get(id) {
        return Ok(cached);
    }
    let generation = cache.generation();
    drop(cache);

    let decoded = decode_log(id)?;
    Ok(lock_decoded_logs().insert_if_current(id, decoded, generation))
}

/// One log's stored legality verdicts, regrouped from the flat rows into one
/// vector per party slot so the response lines up with `players` by index.
///
/// Read per request rather than cached beside the parser: a legality sweep can
/// land while a log is open, and this is one indexed lookup against a table of
/// verdicts — nothing like the decompress the cache exists to avoid. A log the
/// sweep has not reached yet simply has no rows and reads as clean, which is the
/// honest answer until it has been judged.
fn stored_findings(id: u64) -> [Vec<legality::Finding>; 4] {
    let mut findings: [Vec<legality::Finding>; 4] = Default::default();
    let conn = match db::connect_to_db() {
        Ok(conn) => conn,
        // Never fatal: a log still opens without its verdicts.
        Err(error) => {
            log::warn!("could not open the db for log {id}'s legality findings: {error:#}");
            return findings;
        }
    };
    match db::legality::findings_for_log(&conn, id as i64) {
        Ok(stored) => {
            for row in stored {
                if let Some(slot) = findings.get_mut(row.player_index) {
                    slot.push(row.finding);
                }
            }
        }
        Err(error) => log::warn!("could not read legality findings for log {id}: {error:#}"),
    }
    findings
}

// `(async)` so the reparse runs off the main thread — this is called on every
// log open, filter change, brush release, pin, regroup and tab.
//
// The decompress it used to carry too is now behind `decoded_logs`: the blob
// read, the bincode deserialise and the spawn segmentation are the same for
// every request against one log, so they happen once (see `CachedLog`).
#[tauri::command(async)]
fn fetch_encounter_state(id: u64, options: ParseOptions) -> Result<EncounterStateResponse, String> {
    let entry = decoded_log(id)?;
    // Recovered rather than propagated, for the reason in `lock_decoded_logs`:
    // the reparse below rebuilds everything a panicking request could have left
    // half-written.
    let mut cached = entry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Split up front so the parser can be reparsed (&mut) while the segmentation
    // beside it is read (&) — they are separate fields of one guard.
    let CachedLog {
        parser,
        segments,
        assignment,
        room_index,
        imported,
    } = &mut *cached;
    let (room_index, imported) = (*room_index, *imported);

    let findings = stored_findings(id);

    parser.filters = options.filters;
    parser.selection = options.selection.clone();
    parser.reparse_with_options(
        &options.target_spans,
        options.from_ms,
        options.up_to_ms,
        options.windows.as_deref(),
    );

    if options.state_only {
        // Included even here, unlike the charts: it is one pass over the events
        // with no buffers to build, and the analysis view's selectors have to
        // re-cascade against the new window on the very fetch that applies it.
        // The segmentation it indexes into is the CACHED one — over the
        // unwindowed log, exactly as the base load's `target_entries` is — so a
        // pin cannot point at a different enemy on a scoped fetch than on the
        // load.
        let selection_facts = v1::selection_facts(
            &parser.encounter.raw_event_log,
            parser.start_time(),
            options.from_ms,
            options.up_to_ms,
            assignment,
        );

        // The group aggregation: the analysis view's table rows and chart
        // bands under the current pins, window and grouping. Its series span
        // the FULL fight (the view slices client-side), while its measures
        // honor the query's own scrub window.
        let groups = build_groups(parser, &options, segments, assignment);
        let group_reference = build_group_reference(parser, &options, segments, assignment);

        // The drilled Stun/SBA bands, built here too: unlike the other charts
        // these are NOT carried over from the base load. A drill is a pin
        // change, and pins are exactly what the scoped fetch answers — and that
        // fetch sets `state_only`, so building them only below would mean the
        // drill never received them. Costs nothing undrilled: the query is
        // absent, and `build_ability_series` returns empty without walking.
        let player_indices: Vec<u32> = parser
            .derived_state
            .party
            .values()
            .map(|player| player.index)
            .collect();
        let ability_series = build_ability_series(parser, &options, &player_indices);

        // Only the fields the scrub commit actually consumes; everything else stays at its
        // Default so a new response field doesn't have to be mirrored as a hand-written
        // zero here (where forgetting it would silently return stale-looking data).
        return Ok(EncounterStateResponse {
            // TAKEN, not cloned: the parser stays in the cache, and the first
            // thing the next request's reparse does is overwrite this field
            // wholesale — so moving it out costs nothing and saves copying every
            // player's whole skill breakdown.
            encounter_state: std::mem::take(&mut parser.derived_state),
            // Cloned, because this one is log DATA rather than derived state:
            // taking it would empty the cached copy for every later request.
            players: parser.encounter.player_data.clone(),
            quest_id: parser.encounter.quest_id,
            quest_timer: parser.encounter.quest_timer,
            quest_completed: parser.encounter.quest_completed,
            room_index,
            imported,
            legality: findings,
            selection_facts,
            groups,
            group_reference,
            ability_series,
            ..Default::default()
        });
    }

    // Chart buffers span the FULL fight even when a scrub cutoff truncates the
    // derived state — the chart-building loops below walk the full event log, so
    // sizing from the truncated duration would index out of bounds.
    let duration = parser.full_log_duration();

    let start_time = parser.start_time();
    // Dropdown entries are ALWAYS the unfiltered segmentation — the user picks
    // from everything the fight contained, whatever is currently selected.
    // Indexed, because a `SelectionFact` names the SEGMENT it hit rather than
    // the game's actor index (which is recycled between bosses) — and those
    // indices are into this very vector, which ships as `target_entries`.
    // Cached beside the parser, since it depends on the log and nothing else.
    let target_entries = segments;
    // Windowed, but never narrowed by the pins themselves: each selector must
    // keep offering everything the OTHER pins still allow.
    let selection_facts = v1::selection_facts(
        &parser.encounter.raw_event_log,
        start_time,
        options.from_ms,
        options.up_to_ms,
        assignment,
    );

    let player_indices: Vec<u32> = parser
        .derived_state
        .party
        .values()
        .map(|player| player.index)
        .collect();
    // ONE scope for every chart this response builds — see `v1::ChartScope`.
    let chart_scope = v1::ChartScope {
        player_data: &parser.encounter.player_data,
        target_spans: &options.target_spans,
        abilities: &options.selection.abilities,
        filters: options.filters,
    };

    let player_dps = v1::build_player_dps_chart(
        &parser.encounter.raw_event_log,
        &player_indices,
        start_time,
        DPS_INTERVAL,
        (duration / DPS_INTERVAL) as usize + 1,
        &chart_scope,
    );
    let player_stun = v1::build_player_stun_chart(
        &parser.encounter.raw_event_log,
        &player_indices,
        start_time,
        DPS_INTERVAL,
        (duration / DPS_INTERVAL) as usize + 1,
        &chart_scope,
    );

    let player_taken = v1::build_player_taken_chart(
        &parser.encounter.raw_event_log,
        &player_indices,
        start_time,
        DPS_INTERVAL,
        (duration / DPS_INTERVAL) as usize + 1,
    );

    let hp_chart = v1::build_target_hp_charts(
        &parser.encounter.raw_event_log,
        &target_entries,
        start_time,
        DPS_INTERVAL,
        (duration / DPS_INTERVAL) as usize + 1,
        &chart_scope,
    );

    // Closed against the end of the full log, not the scrub window: an interval
    // still open at the last event runs to the end of the fight, and closing it
    // at a window edge instead would report a buff as having dropped there.
    // Segmented against the same `target_entries` the response ships, so a
    // debuff row names the SPAWN that held it. Without it a recycled boss id
    // merged two enemies' windows into one row labelled with a bare number.
    let status_intervals = v1::assemble_intervals(
        &parser.encounter.raw_event_log,
        start_time,
        duration,
        target_entries,
    );

    // Same closing rule as the status intervals: a state still active at the
    // last event ran to the end of what was recorded.
    let chart_windows =
        v1::assemble_chart_windows(&parser.encounter.raw_event_log, start_time, duration);

    let groups = build_groups(parser, &options, target_entries, assignment);
    let group_reference = build_group_reference(parser, &options, target_entries, assignment);

    // The drilled Stun/SBA tabs' bands.
    let ability_series = build_ability_series(parser, &options, &player_indices);

    let sba_chart = parser.generate_sba_chart(SBA_INTERVAL);

    // One entry per art the fight actually contains, recovered from the SBA
    // damage: the hook records an activation event for only some arts, and one
    // for no art at all at a Repeat Quest boundary (see `sba_activations`).
    // Attempts are the button press and pass through as recorded.
    let mut sba_events: Vec<(i64, Message)> = parser
        .encounter
        .event_log()
        .filter(|(_, e)| matches!(e, Message::OnAttemptSBA(_)))
        .map(|(ts, e)| (*ts - start_time, e.clone()))
        .collect();
    sba_events.extend(
        v1::sba_activations(&parser.encounter.raw_event_log)
            .into_iter()
            .map(|activation| match activation {
                v1::SbaActivation::Recorded(index) => {
                    let (ts, event) = &parser.encounter.raw_event_log[index];
                    (*ts - start_time, event.clone())
                }
                v1::SbaActivation::Derived { at, actor_index } => (
                    at - start_time,
                    Message::OnPerformSBA(protocol::OnPerformSBAEvent { actor_index }),
                ),
            }),
    );
    sba_events.sort_by_key(|(ts, _)| *ts);

    let death_events = parser
        .encounter
        .event_log()
        .filter(|(_, e)| matches!(e, Message::OnDeathEvent(_)))
        .map(|(ts, e)| (*ts - start_time, e.clone()))
        .collect();

    Ok(EncounterStateResponse {
        // Taken and cloned respectively, for the reasons on the `state_only`
        // response above: the derived state is rebuilt by the next reparse, the
        // player data is not.
        encounter_state: std::mem::take(&mut parser.derived_state),
        players: parser.encounter.player_data.clone(),
        quest_id: parser.encounter.quest_id,
        quest_timer: parser.encounter.quest_timer,
        quest_completed: parser.encounter.quest_completed,
        room_index,
        imported,
        legality: findings,
        status_intervals,
        chart_windows,
        dps_chart: player_dps,
        stun_chart: player_stun,
        taken_chart: player_taken,
        hp_chart,
        groups,
        group_reference,
        chart_len: (duration / DPS_INTERVAL) as usize + 1,
        sba_chart_len: (duration / SBA_INTERVAL) as usize + 1,
        sba_chart,
        ability_series,
        sba_events,
        death_events,
        // Cloned out of the cache: one entry per spawn, so a handful of structs
        // — the vector this indexes into is what had to be shared, not this
        // copy of it.
        target_entries: target_entries.clone(),
        selection_facts,
    })
}

/// Every flagged player across the whole database, for the Toolbox audit page.
///
/// Reads stored verdicts, so this is a join and a group rather than a re-audit
/// of every log. `(async)` regardless: it still walks every finding.
#[tauri::command(async)]
fn fetch_legality_players() -> Result<Vec<db::legality::FlaggedPlayer>, String> {
    let conn = db::connect_to_db().map_err(|e| e.to_string())?;
    db::legality::flagged_players(&conn).map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_logs(ids: Vec<u64>) -> Result<(), String> {
    let conn = db::connect_to_db().map_err(|e| e.to_string())?;

    let id_params: Vec<String> = ids.iter().map(|_| "?".to_string()).collect();
    let param = id_params.join(",");

    let sql = format!("DELETE FROM logs WHERE id IN ({})", param);
    {
        let mut statement = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
        statement
            .execute(params_from_iter(ids))
            .map_err(|e| e.to_string())?;
    }

    // Verdicts go with their logs: an orphaned finding would attach itself to
    // whatever future encounter lands on the recycled rowid.
    db::legality::sweep_orphaned_findings(&conn).map_err(|e| e.to_string())?;

    // A deleted log may have been a Conflux room; reap any run left with no rooms so it
    // doesn't linger as a ghost "×0 rooms" row (the startup sweep only reaps in-progress
    // runs).
    db::runs::delete_runs_without_rooms(&conn).map_err(|e| e.to_string())?;

    // MANDATORY, not tidiness: `logs` has no AUTOINCREMENT, so SQLite hands a
    // deleted log's rowid to the next encounter recorded — and a cached entry
    // under that id would serve one fight's events under the next one's name.
    forget_decoded_logs();

    Ok(())
}

/// Dev reload diagnostics that land in a file we can always read, regardless of
/// the app's log level/target: `<exe_dir>/portable_data/hook-logs/reload-debug.log`,
/// next to the hook's own gbfr-logs.txt. Also mirrors to the normal `info!` sink.
/// The file write happens only in debug builds; release still gets the `info!`.
#[cfg(windows)]
fn reload_dbg(msg: &str) {
    info!("{msg}");
    #[cfg(debug_assertions)]
    {
        use std::io::Write;
        let line = format!(
            "[{}] {msg}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f")
        );
        // Best-effort: a logging hiccup must never disrupt the reload flow.
        let path = gbfr_logs::portable::portable_root()
            .join("hook-logs")
            .join("reload-debug.log");
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// The hook module currently mapped in the target, preferring the dev
/// `hook-dbg.dll` over the release `hook.dll` (only one is ever present).
#[cfg(windows)]
fn find_hook_module(
    syringe: &Syringe,
) -> std::io::Result<
    Option<dll_syringe::process::ProcessModule<dll_syringe::process::BorrowedProcess<'_>>>,
> {
    use dll_syringe::process::Process as _;
    let process = syringe.process();
    // A hook-dbg.dll lookup error falls through to hook.dll rather than
    // propagating: a transient module-enumeration error must not read as "no
    // hook present" (that would drive a redundant re-inject over a live hook —
    // refcount bump, stale mapping reused). hook.dll's own error still
    // propagates so eject_hook_until_gone can surface a real failure.
    if let Ok(Some(m)) = process.find_module_by_name("hook-dbg.dll") {
        return Ok(Some(m));
    }
    process.find_module_by_name("hook.dll")
}

/// FreeLibrary the injected hook module until it is genuinely unmapped — not
/// just once.
///
/// `syringe.inject()` is `LoadLibrary`, and Windows runs a DLL's `#[ctor]`
/// (`DllMain` attach) ONLY on the first load into a process; a later
/// `LoadLibrary` of an already-mapped module just bumps its refcount and
/// returns the existing handle WITHOUT re-running the ctor. So a hook module
/// left mapped (refcount > 1 from reconnect re-injections, or a prior session's
/// module that outlived a single FreeLibrary) gets reused on the next inject as
/// a dead hook: no ctor, no event pipe, no control pipe. Ejecting until
/// `find_module_by_name` reports it gone guarantees the next inject is a true
/// fresh load. Bounded so a module that refuses to unmap surfaces as an error
/// instead of looping forever.
#[cfg(windows)]
fn eject_hook_until_gone(syringe: &Syringe) -> anyhow::Result<u32> {
    // dll_syringe::eject runs FreeLibrary (decrementing the refcount) and
    // returns Ok, but then a `debug_assert!(..., "ejected module survived")`
    // panics in debug builds when the module is STILL mapped afterward (i.e.
    // the refcount was > 1). The FreeLibrary already happened by that point, so
    // we catch that specific panic and loop — each pass drops one refcount
    // until the module is genuinely unmapped, at which point eject returns Ok
    // without panicking. This drains a legacy refcount left by pre-fix reconnect
    // re-injections; steady-state refcount is 1, so no panic occurs then.
    //
    // Silence the intentional panics' console noise for the duration (restored
    // right after). The window is brief and only during (re)injection.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = (|| -> anyhow::Result<u32> {
        let mut ejected = 0u32;
        for _ in 0..64 {
            let Some(module) = find_hook_module(syringe)? else {
                return Ok(ejected);
            };
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| syringe.eject(module))) {
                // Clean unload (module gone → debug_assert passed).
                Ok(Ok(())) => ejected += 1,
                // A real eject error (not the survival assert): give up.
                Ok(Err(e)) => return Err(anyhow::Error::new(e).context("FreeLibrary of the hook")),
                // "ejected module survived": FreeLibrary ran, refcount still > 0.
                Err(_) => ejected += 1,
            }
        }
        anyhow::bail!("hook module still mapped after 64 FreeLibrary attempts")
    })();
    std::panic::set_hook(prev_hook);
    result
}

/// Is a hook module (`hook-dbg.dll` or `hook.dll`) currently mapped in the
/// target process? Used to avoid a second inject (LoadLibrary would just bump
/// the refcount without re-running the ctor) and, more importantly, to avoid
/// FreeLibrary'ing a live hook — see check_and_perform_hook.
#[cfg(windows)]
fn hook_module_present(syringe: &Syringe) -> bool {
    matches!(find_hook_module(syringe), Ok(Some(_)))
}

// Continuously check for the game process and inject the DLL when found.
#[cfg(windows)]
async fn check_and_perform_hook(app: AppHandle) {
    reload_dbg("check_and_perform_hook: (re)entered, polling for game process");
    loop {
        // Hook reload/refresh in flight: the old module must be ejected (and,
        // for a dev reload, hook-dbg.dll refreshed) before we may inject again.
        {
            let hook = app.state::<HookStatus>();
            let mut waits: u32 = 0;
            while hook.injection_gated() {
                // Log the first wait + periodically: dev_hold_out is designed
                // to hold indefinitely (see its doc comment), so a one-shot
                // log here would leave a long hold invisible in
                // reload-debug.log — exactly where a stranded session should
                // leave a breadcrumb.
                if waits == 0 || waits % 50 == 0 {
                    reload_dbg(&format!(
                        "check_and_perform_hook: gate SET, waiting (reloading={}, dev_hold_out={})",
                        hook.reloading.load(Ordering::Relaxed),
                        hook.dev_hold_out.load(Ordering::Relaxed)
                    ));
                }
                waits += 1;
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            if waits > 0 {
                reload_dbg("check_and_perform_hook: gate CLEARED, proceeding");
            }
        }

        match OwnedProcess::find_first_by_name(gbfr_logs::game_mem::GAME_EXE) {
            Some(target) => {
                // `Syringe` is !Send, so nothing may await while it is alive —
                // hence this sync block, which decides and then drops it. The
                // retry sleeps happen outside, after `ready` comes back false.
                let ready = {
                    let syringe = Syringe::for_process(target);

                    // If a hook is ALREADY mapped, do not inject again and NEVER
                    // FreeLibrary it here. A live hook has detours installed and game
                    // threads running through its trampolines; unmapping it out from
                    // under them crashes the game (AccessViolation). A second inject
                    // would only bump the refcount without re-running the ctor
                    // (LoadLibrary runs it once). The only safe teardown of a live
                    // hook is the graceful control-channel path in reload_hook_inner,
                    // which disables the detours and fully unmaps BEFORE we get here —
                    // so a real reload arrives with nothing mapped and injects below.
                    // A hook already present is therefore either that live hook (from
                    // a prior app session) or an abnormally dead one: in both cases we
                    // just (re)connect and let the pipe tell us which.
                    if hook_module_present(&syringe) {
                        reload_dbg(
                            "check_and_perform_hook: a hook is already mapped; connecting without re-inject (never FreeLibrary a live hook)",
                        );
                        true
                    } else {
                        // Stat before injecting: a quarantined DLL is the
                        // common failure here, and it deserves a named state
                        // rather than an inject error nobody reads. Keep
                        // polling — restoring the file then self-heals
                        // without an app restart.
                        refresh_hook_dll_presence(&app);
                        match gbfr_logs::hook_dll::injectable_hook() {
                            None => {
                                reload_dbg(
                                    "check_and_perform_hook: game found but NO hook DLL on disk; \
                                     not injecting (see the hook status badge)",
                                );
                                false
                            }
                            Some(dll_path) => {
                                reload_dbg(&format!(
                                    "check_and_perform_hook: game found, injecting {dll_path:?}"
                                ));
                                match syringe.inject(&dll_path) {
                                    Ok(module) => {
                                        reload_dbg(&format!(
                                            "check_and_perform_hook: inject OK ({dll_path:?} -> {module:?})"
                                        ));
                                        true
                                    }
                                    // Used to fall through to the "Found
                                    // game.." toast below and then wait on a
                                    // pipe nothing would ever serve. Say so
                                    // and retry instead.
                                    Err(e) => {
                                        reload_dbg(&format!(
                                            "check_and_perform_hook: inject FAILED for {dll_path:?}: {e:?}"
                                        ));
                                        log::error!(
                                            "hook injection failed for {dll_path:?}: {e:?}"
                                        );
                                        let _ = app.emit_all(
                                            "error-alert",
                                            format!("Hook injection failed: {e}"),
                                        );
                                        false
                                    }
                                }
                            }
                        }
                    }
                };

                if !ready {
                    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                    continue;
                }
                let _ = app.emit_all("success-alert", "Found game..");

                connect_and_run_parser(app);
                reload_dbg("check_and_perform_hook: connect_and_run_parser spawned; loop exiting");

                break;
            }
            None => {
                tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
            }
        }
    }
}

/// Dev-only hook hot-reload: ask the running hook to tear itself down, eject
/// the dead module, refresh hook-dbg.dll from the cargo artifact, and let
/// the standard reconnect loop re-inject. See
/// docs/superpowers/specs/2026-07-23-dev-hook-hot-reload-design.md.
#[cfg(all(windows, debug_assertions))]
async fn reload_hook(app: AppHandle) {
    if app
        .state::<HookStatus>()
        .reloading
        .swap(true, Ordering::SeqCst)
    {
        return; // a reload is already in flight
    }
    // Same rule as refresh_hook: asking for a reload means asking for a working
    // hook, so a debug hold-out left set must not survive it and block the
    // re-inject that ends this flow.
    app.state::<HookStatus>()
        .dev_hold_out
        .store(false, Ordering::Relaxed);
    reload_dbg("reload_hook: START (reloading gate set)");
    let result = reload_hook_inner(&app, Some(Path::new("../target/release/hook.dll"))).await;
    app.state::<HookStatus>()
        .reloading
        .store(false, Ordering::SeqCst);
    match result {
        Ok(()) => {
            reload_dbg("reload_hook: reload_hook_inner OK; reloading gate cleared");
            let _ = app.emit_all("success-alert", "Hook reloaded");
        }
        Err(e) => {
            reload_dbg(&format!("reload_hook: reload_hook_inner FAILED: {e:?}"));
            log::warn!("hook reload failed: {e:?}");
            let _ = app.emit_all("error-alert", format!("Hook reload failed: {e}"));
        }
    }
}

#[cfg(windows)]
async fn reload_hook_inner(app: &AppHandle, refresh_from: Option<&Path>) -> anyhow::Result<()> {
    use anyhow::{anyhow, bail, Context};

    reload_dbg("reload_hook_inner: sending Eject over control channel");
    gbfr_logs::control_rpc::eject().await?;
    reload_dbg("reload_hook_inner: Eject acknowledged; waiting for connected=false");

    // The event pipe closing (connect loop flips `connected` off) means the
    // hook's runtime exited. Timing out means the hook is half-dead —
    // ejecting would FreeLibrary a module with live threads, so refuse.
    let wait_start = std::time::Instant::now();
    let deadline = wait_start + std::time::Duration::from_secs(5);
    while app.state::<HookStatus>().connected.load(Ordering::Relaxed) {
        if std::time::Instant::now() >= deadline {
            reload_dbg("reload_hook_inner: TIMEOUT — connected never went false within 5s (pipe did not close)");
            bail!("hook did not shut down within 5s; state unknown — restart the game");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    reload_dbg(&format!(
        "reload_hook_inner: connected=false after {}ms; 300ms grace then eject",
        wait_start.elapsed().as_millis()
    ));
    // Grace: the setup thread finishes exiting shortly after its runtime
    // drops (the pipe closes slightly before the thread is gone).
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let target = OwnedProcess::find_first_by_name(gbfr_logs::game_mem::GAME_EXE)
        .ok_or_else(|| anyhow!("game process not found"))?;
    let syringe = Syringe::for_process(target);
    // Unmap the old module fully (refcount may be > 1), else the reconnect
    // re-inject reuses it without re-running the ctor. See eject_hook_until_gone.
    match eject_hook_until_gone(&syringe).context("unloading the old hook")? {
        0 => reload_dbg("reload_hook_inner: no hook module mapped; skipping eject"),
        n => reload_dbg(&format!(
            "reload_hook_inner: old hook fully unmapped after {n} FreeLibrary call(s)"
        )),
    }

    // Refresh the DLL to be re-injected, if a source path was given. Dev
    // reload copies the cargo artifact over hook-dbg.dll; production refresh
    // re-injects the installed hook.dll in place (no copy needed), passing None.
    if let Some(src) = refresh_from {
        if src.exists() {
            std::fs::copy(src, "hook-dbg.dll").context("refreshing hook-dbg.dll")?;
            reload_dbg(&format!(
                "reload_hook_inner: hook-dbg.dll refreshed from {src:?}"
            ));
        } else {
            // Missing dev artifact: re-inject the existing hook-dbg.dll rather
            // than hard-failing (which would leave the game with no hook, since
            // the old one is already ejected).
            reload_dbg(&format!(
                "reload_hook_inner: {src:?} NOT FOUND; re-injecting existing hook-dbg.dll"
            ));
        }
    }
    Ok(())
}

/// Production hook hot-swap: graceful eject + re-inject of the installed
/// hook.dll, so an updated app can refresh the hook without a game restart.
/// Gated on the running hook advertising the `eject` control channel — a
/// pre-rollout hook cannot be safely ejected, so we tell the user to restart
/// the game instead. Windows only (no injector under Proton).
#[cfg(windows)]
#[tauri::command]
async fn refresh_hook(app: AppHandle) -> Result<(), String> {
    let hook = app.state::<HookStatus>();
    // A user reaching for Refresh wants a working hook; a stale debug hold-out
    // must never outrank that. Clear it BEFORE the `connected` check: a held
    // gate blocks injection, so `connected` is false in exactly the stranded
    // case this exists to rescue, and clearing below the early return would
    // never run there.
    hook.dev_hold_out.store(false, Ordering::Relaxed);
    if !hook.connected.load(Ordering::Relaxed) {
        return Err("game-not-running".into());
    }
    if !hook.supports_eject.load(Ordering::Relaxed) {
        return Err("hook-refresh-unsupported".into());
    }
    if hook.reloading.swap(true, Ordering::SeqCst) {
        return Err("hook-refresh-in-progress".into());
    }
    emit_hook_status(&app);
    // None => re-inject the installed hook.dll in place (no artifact copy).
    let result = reload_hook_inner(&app, None).await;
    app.state::<HookStatus>()
        .reloading
        .store(false, Ordering::SeqCst);
    emit_hook_status(&app);
    result.map_err(|e| format!("{e}"))
}

/// Non-Windows: no injector, so refresh is not available — restart the game.
#[cfg(not(windows))]
#[tauri::command]
async fn refresh_hook(_app: AppHandle) -> Result<(), String> {
    Err("hook-refresh-unsupported".into())
}

#[cfg(windows)]
mod dev_debug {
    //! Dev Debug tab: every command in this module drives the REAL hook over
    //! the dev control channel. Nothing here fabricates frontend state — the
    //! app derives hook status from real handshakes and the parser derives
    //! encounter status from real frames. (The `#[cfg(not(windows))]` stubs
    //! outside this module drive nothing: there is no control-channel client
    //! there, so they only report the unavailable slug.)

    use super::*;

    /// Report a control-channel failure verbatim. `control_rpc` produces
    /// distinct, actionable errors — "hook refused eject: …", "refusing to
    /// broadcast an empty batch", an unexpected response, a connect refusal
    /// (the common case: a release hook serves no control listener), a 2s
    /// timeout — and collapsing all of them into one slug would make this
    /// diagnostic tool less diagnostic than the layer beneath it. The raw
    /// detail IS the diagnosis, and `backendErrorMessage` passes unmapped
    /// strings through verbatim, so it reaches the user intact. Same choice
    /// `refresh_hook` already makes for the reload toast.
    fn control_failure(e: anyhow::Error) -> String {
        log::warn!("debug control call failed: {e:?}");
        format!("{e:#}")
    }

    /// Eject the live hook. With `hold`, keep it out of the process so the
    /// resulting `Disconnected` stays put instead of self-healing in ~1s.
    #[tauri::command]
    pub async fn debug_eject_hook(app: AppHandle, hold: bool) -> Result<(), String> {
        let hook = app.state::<HookStatus>();
        hook.debug_precondition()?;
        // A hold set while a refresh is in flight survives that refresh's
        // clear — `refresh_hook`/`reload_hook` clear `dev_hold_out` at the
        // START of the flow, so a hold stored mid-flight is never covered.
        // The refresh then ends, `reloading` drops, and the gate stays shut
        // with nothing left to open it: the injection loop waits forever and
        // the badge reads "No game found". See `HookStatus::injection_gated`.
        if hook.reloading.load(Ordering::Relaxed) {
            return Err("hook-refresh-in-progress".into());
        }
        // Set the gate BEFORE ejecting, and clear it again if the eject fails.
        // Storing it afterwards would leave a window between the hook dying
        // and the flag landing in which the injection loop is free to
        // re-inject, so the hold silently would not hold; clearing on error
        // covers the reason the store was down here in the first place (a
        // stuck gate after a failed eject). Setting it while still connected
        // is harmless: the injection loop breaks out after spawning
        // `connect_and_run_parser` and is only respawned once that task's pipe
        // is gone, so nothing is reading the gate yet.
        if hold {
            hook.dev_hold_out.store(true, Ordering::Relaxed);
        }
        let result = gbfr_logs::control_rpc::eject().await;
        if result.is_err() && hold {
            hook.dev_hold_out.store(false, Ordering::Relaxed);
        }
        result.map_err(control_failure)
    }

    /// Release the hold-out gate; the injection loop reinjects on its own.
    #[tauri::command]
    pub fn debug_allow_reinject(app: AppHandle) -> Result<(), String> {
        if !cfg!(debug_assertions) {
            return Err("debug-only".into());
        }
        app.state::<HookStatus>()
            .dev_hold_out
            .store(false, Ordering::Relaxed);
        Ok(())
    }

    /// Read the dev hold-out flag so the Debug page can show it. A SEPARATE
    /// query, deliberately not a field on `HookStatusSnapshot`: putting it
    /// there would undo the point, which is that the reported state stays
    /// derived from real signals. Pull-only, no event — re-query this on
    /// `hook-status`, because `refresh_hook`/`reload_hook` also clear the flag,
    /// behind the page's back.
    #[tauri::command]
    pub fn debug_hold_out_state(app: AppHandle) -> Result<bool, String> {
        if !cfg!(debug_assertions) {
            return Err("debug-only".into());
        }
        Ok(app
            .state::<HookStatus>()
            .dev_hold_out
            .load(Ordering::Relaxed))
    }

    /// Override the live hook's `Hello`, then re-run the handshake so the app
    /// DERIVES the new state instead of being handed it. Each field is
    /// independent: a `None` field keeps the hook's real value for that field
    /// while the override stays installed. Removing the override entirely is a
    /// different operation — `debug_clear_hello_override`.
    #[tauri::command]
    pub async fn debug_set_hello_override(
        app: AppHandle,
        hook_version: Option<String>,
        protocol_version: Option<u32>,
        supports_eject: Option<bool>,
    ) -> Result<(), String> {
        app.state::<HookStatus>().debug_precondition()?;
        let o = protocol::control::HelloOverride {
            hook_version,
            protocol_version,
            supports_eject,
        };
        gbfr_logs::control_rpc::set_hello_override(Some(o))
            .await
            .map_err(control_failure)?;
        handshake_and_apply(&app).await;
        Ok(())
    }

    /// Clear the override on the hook. `resync` then re-runs the handshake so
    /// the app picks the change up at once.
    ///
    /// Passing `false` is what makes `hook_heartbeat` observable: every other
    /// path refreshes the status itself, which masks whether the periodic
    /// re-probe runs at all. Left un-resynced the app holds a now-stale
    /// verdict, so the badge should sit on its old state and turn green on its
    /// own within `HEARTBEAT_INTERVAL` — and if it does not, the heartbeat is
    /// broken.
    #[tauri::command]
    pub async fn debug_clear_hello_override(app: AppHandle, resync: bool) -> Result<(), String> {
        app.state::<HookStatus>().debug_precondition()?;
        gbfr_logs::control_rpc::set_hello_override(None)
            .await
            .map_err(control_failure)?;
        if resync {
            handshake_and_apply(&app).await;
        }
        Ok(())
    }

    /// Send one scenario batch to the hook, which rebroadcasts it on the real
    /// event stream. Returns how many frames the hook QUEUED onto its
    /// broadcast channel — not how many the app saw. 0 means nothing is
    /// subscribed to the stream; a lagging receiver can still drop queued
    /// frames (the channel holds 1024) and end the app's event-stream loop.
    #[tauri::command]
    pub async fn debug_broadcast_scenario(
        app: AppHandle,
        kind: gbfr_logs::debug_events::Scenario,
    ) -> Result<u32, String> {
        app.state::<HookStatus>().debug_precondition()?;
        gbfr_logs::control_rpc::broadcast_events(gbfr_logs::debug_events::scenario(kind))
            .await
            .map_err(control_failure)
    }
}

#[cfg(windows)]
use dev_debug::{
    debug_allow_reinject, debug_broadcast_scenario, debug_clear_hello_override, debug_eject_hook,
    debug_hold_out_state, debug_set_hello_override,
};

// Non-Windows: no control channel client, so the Debug tab reports the same
// slug the Toolbox uses for an unusable hook — the one string
// `backendErrors.ts` still maps for these commands. The parameters are
// declared and ignored rather than dropped: unused here, but they keep the
// two sides' shapes visibly identical, so a rename or an added argument shows
// up on this side too.
#[cfg(not(windows))]
#[tauri::command]
async fn debug_eject_hook(_app: AppHandle, _hold: bool) -> Result<(), String> {
    Err("hook-control-unavailable".into())
}

#[cfg(not(windows))]
#[tauri::command]
fn debug_allow_reinject(_app: AppHandle) -> Result<(), String> {
    Err("hook-control-unavailable".into())
}

/// Unlike its siblings this answers rather than errors: with no injection loop
/// to hold out of, the flag can never be set here, so `false` is the truth.
#[cfg(not(windows))]
#[tauri::command]
fn debug_hold_out_state(_app: AppHandle) -> Result<bool, String> {
    Ok(false)
}

#[cfg(not(windows))]
#[tauri::command]
async fn debug_set_hello_override(
    _app: AppHandle,
    _hook_version: Option<String>,
    _protocol_version: Option<u32>,
    _supports_eject: Option<bool>,
) -> Result<(), String> {
    Err("hook-control-unavailable".into())
}

#[cfg(not(windows))]
#[tauri::command]
async fn debug_clear_hello_override(_app: AppHandle, _resync: bool) -> Result<(), String> {
    Err("hook-control-unavailable".into())
}

#[cfg(not(windows))]
#[tauri::command]
async fn debug_broadcast_scenario(
    _app: AppHandle,
    _kind: gbfr_logs::debug_events::Scenario,
) -> Result<u32, String> {
    Err("hook-control-unavailable".into())
}

// Linux: no injector. Refresh the dinput8 proxy in the game folder
// (best-effort — the setup panel surfaces failures), then let the TCP
// connect-retry loop double as the "game running?" poll.
#[cfg(not(windows))]
async fn check_and_perform_hook(app: AppHandle) {
    use gbfr_logs::linux_support::{deploy, steam};

    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let bundled = app.path_resolver().resolve_resource("hook.dll");
    match (home, bundled) {
        (Some(home), Some(bundled)) => match steam::discover(&steam::default_steam_roots(&home)) {
            Some(game) => match deploy::deploy(&game.game_dir, &bundled) {
                Ok(_) => info!("proxy dinput8.dll is current in {:?}", game.game_dir),
                Err(e) => log::warn!("could not deploy the proxy DLL: {e:?}"),
            },
            None => log::warn!("Steam install of the game not found; see Settings → Linux setup"),
        },
        _ => log::warn!("no HOME or no bundled hook.dll; cannot deploy the proxy DLL"),
    }

    connect_and_run_parser(app);
}

/// Connect to the hook's event stream: the named pipe on Windows (localhost
/// TCP when GBFR_LOGS_FORCE_TCP=1, for parity-testing the Linux path), and
/// localhost TCP elsewhere — under Proton the hook detects Wine and listens
/// on TCP because a native Linux app cannot open Wine named pipes.
async fn connect_event_stream() -> anyhow::Result<Box<dyn tokio::io::AsyncRead + Unpin + Send>> {
    #[cfg(windows)]
    if std::env::var("GBFR_LOGS_FORCE_TCP").as_deref() != Ok("1") {
        let stream = RecvPipeStream::connect_by_path(protocol::PIPE_NAME).await?;
        return Ok(Box::new(stream));
    }
    let stream = tokio::net::TcpStream::connect(protocol::TCP_ADDR).await?;
    Ok(Box::new(stream))
}

// Connect to the game hook event channel and listen for damage events.
fn connect_and_run_parser(app: AppHandle) {
    let window = app.get_window("main").expect("Window not found");
    let logs_window = app.get_window("logs").expect("Logs window not found");

    let database = db::connect_to_db().expect("Could not connect to database");
    let mut state = v1::Parser::new(app.clone(), window.clone(), database);
    // Seed from whatever the settings window has already pushed: this task can
    // start either side of the frontend's first push, and the parser outlives
    // game reconnects, so it must not depend on being pushed to again.
    state.filters = *app.state::<MeterFilterState>().current.lock().unwrap();
    // A live save can hand out an id SQLite recycled from a deleted log — see
    // forget_decoded_logs.
    state.on_saved = Some(forget_decoded_logs);

    let (reset_tx, mut reset_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    *app.state::<ResetChannel>().0.lock().unwrap() = Some(reset_tx);

    let (filters_tx, mut filters_rx) = tokio::sync::mpsc::unbounded_channel::<v1::MeterFilters>();
    *app.state::<MeterFilterState>().tx.lock().unwrap() = Some(filters_tx);

    let (republish_tx, mut republish_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    *app.state::<RepublishChannel>().0.lock().unwrap() = Some(republish_tx);

    tauri::async_runtime::spawn(async move {
        #[cfg(windows)]
        let mut connect_failures: u32 = 0;
        loop {
            match connect_event_stream().await {
                Ok(stream) => {
                    info!("Connected to game!");
                    #[cfg(windows)]
                    reload_dbg("connect_and_run_parser: event pipe CONNECTED (hook is live)");

                    let _ = app.emit_all("success-alert", "Connnected to game!");

                    // Marks connected AND drops the previous hook's verdict —
                    // a version answer belongs to the module that gave it.
                    app.state::<HookStatus>().on_connect();
                    // Hello up front, once per (re)connect: the reported
                    // version + eject support drive the status badge and
                    // refresh gating. If it cannot be delivered the state says
                    // exactly that ("not answering", not "out of date") and
                    // `hook_heartbeat` keeps asking.
                    //
                    // Spawned, NOT awaited: nothing below needs the result, and
                    // its retry ladder can run ~10s against a wedged toolbox
                    // channel — which would be ~10s of frames sitting unread in
                    // the event pipe purely to settle a status badge. A late
                    // result is harmless: `snapshot()` gates on `connected`, so
                    // one landing after a disconnect cannot resurrect the badge.
                    let handshake_app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        handshake_and_apply(&handshake_app).await;
                    });

                    let decoder = tokio_util::codec::LengthDelimitedCodec::new();
                    let mut reader = FramedRead::new(stream, decoder);

                    // Drives the trailing half of the `encounter-update`
                    // throttle. Without it the update produced by the last hit
                    // of a fight would be suppressed and never replaced, since
                    // nothing else is coming to carry it.
                    let mut flush_tick =
                        tokio::time::interval(std::time::Duration::from_millis(50));
                    flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

                    loop {
                        let msg = tokio::select! {
                            next = reader.next() => match next {
                                Some(Ok(msg)) => msg,
                                // Pipe closed or read error: the game is gone.
                                _ => break,
                            },
                            _ = flush_tick.tick() => {
                                state.flush_encounter_update();
                                continue;
                            }
                            Some(()) = republish_rx.recv() => {
                                state.republish_live_state();
                                continue;
                            }
                            Some(()) = reset_rx.recv() => {
                                state.on_manual_reset();
                                continue;
                            }
                            Some(filters) = filters_rx.recv() => {
                                // Replay the raw log under the new rule so the
                                // toggle applies to the fight in progress, not
                                // just to hits from here on.
                                state.filters = filters;
                                state.reparse();
                                let _ = window.emit("encounter-update", &state.derived_state);
                                continue;
                            }
                        };

                        // Handle EOF when the game closes.
                        if msg.is_empty() {
                            break;
                        }

                        let debug_mode = app.state::<DebugMode>().0.load(Ordering::Relaxed);

                        if let Ok(msg) = protocol::bincode::deserialize::<protocol::Message>(&msg) {
                            if debug_mode {
                                let _ = logs_window.emit("debug-event", &msg);
                            }

                            match msg {
                                protocol::Message::DamageEvent(event) => {
                                    state.on_damage_event(event);
                                }
                                protocol::Message::OnAreaEnter(event) => {
                                    state.on_area_enter_event(event);
                                }
                                protocol::Message::PlayerLoadEvent(event) => {
                                    state.on_player_load_event(event);
                                }
                                protocol::Message::PlayerIdentityEvent(event) => {
                                    state.on_player_identity_event(event);
                                }
                                protocol::Message::OnQuestComplete(event) => {
                                    state.on_quest_complete_event(event);
                                }
                                protocol::Message::OnQuestElapsedTime(event) => {
                                    state.on_quest_elapsed_time(event);
                                }
                                protocol::Message::OnQuestFail(event) => {
                                    info!(
                                        "quest retire/fail boundary: quest_id={:#x}",
                                        event.quest_id
                                    );
                                    state.on_quest_fail_event(event);
                                }
                                protocol::Message::OnTrialStart(_) => {
                                    info!("training-room session start/teardown boundary");
                                    state.on_trial_start_event();
                                }
                                protocol::Message::OnTrialEnd(_) => {
                                    info!("training-room quit boundary");
                                    state.on_trial_end_event();
                                }
                                protocol::Message::OnUpdateSBA(event) => {
                                    state.on_sba_update(event);
                                }
                                protocol::Message::OnAttemptSBA(event) => {
                                    state.on_sba_attempt(event);
                                }
                                protocol::Message::OnPerformSBA(event) => {
                                    state.on_sba_perform(event);
                                }
                                protocol::Message::OnContinueSBAChain(event) => {
                                    state.on_continue_sba_chain(event);
                                }
                                protocol::Message::SbaGain(event) => {
                                    state.on_sba_gain(event);
                                }
                                protocol::Message::OnDeathEvent(event) => {
                                    state.on_death_event(event);
                                }
                                protocol::Message::ConfluxRoomEnter(event) => {
                                    info!(
                                        "CONFLUX ingress: ConfluxRoomEnter quest_id={:#x} manager={:#x}",
                                        event.quest_id, event.manager_ptr
                                    );
                                    state.on_conflux_room_enter(event);
                                }
                                protocol::Message::ConfluxBuffAcquired(event) => {
                                    info!(
                                        "CONFLUX ingress: ConfluxBuffAcquired buff_id={:#x}",
                                        event.buff_id
                                    );
                                    state.on_conflux_buff_acquired(event);
                                }
                                protocol::Message::ConfluxRunEnd(event) => {
                                    info!(
                                        "CONFLUX ingress: ConfluxRunEnd manager={:#x}",
                                        event.manager_ptr
                                    );
                                    state.on_conflux_run_end(event);
                                }
                                protocol::Message::OnPlayerStun(event) => {
                                    state.on_player_stun(event);
                                }
                                protocol::Message::OnPerfectGuardStun(event) => {
                                    state.on_perfect_guard_stun(event);
                                }
                                protocol::Message::OnPerfectGuardQuickening(event) => {
                                    state.on_perfect_guard_quickening(event);
                                }
                                protocol::Message::OnStunEffect(event) => {
                                    state.on_stun_effect(event);
                                }
                                protocol::Message::StatusApply(event) => {
                                    state.on_status_apply(event);
                                }
                                protocol::Message::StatusRemove(event) => {
                                    state.on_status_remove(event);
                                }
                                protocol::Message::LinkTime(event) => {
                                    state.on_link_time(event);
                                }
                                protocol::Message::EnemyMode(event) => {
                                    state.on_enemy_mode(event);
                                }
                            }
                        }
                    }

                    app.state::<HookStatus>()
                        .connected
                        .store(false, Ordering::Relaxed);
                    emit_hook_status(&app);

                    info!("Game has closed.");

                    // Last chance to persist anything still in progress (abandoned quest →
                    // quit emits no result screen; mid-quest/mid-run quit likewise) — this
                    // parser instance is gone once we go back to waiting for the game.
                    state.on_game_disconnect();

                    // The game has closed, so we should go back to waiting for the game to reopen.
                    #[cfg(windows)]
                    reload_dbg("connect_and_run_parser: event pipe closed (connected=false); breaking to respawn check_and_perform_hook");
                    let _ = app.emit_all("error-alert", "Game has closed!");
                    break;
                }
                Err(_) => {
                    // A fresh hook whose ctor never came up leaves us spinning
                    // here forever (no live connection to lose → no respawn).
                    // Log the first failure + periodically so a wedged reload
                    // is visible instead of silent.
                    #[cfg(windows)]
                    {
                        if connect_failures == 0 || connect_failures % 50 == 0 {
                            reload_dbg(&format!(
                                "connect_and_run_parser: event pipe connect failing (attempt {}); no hook pipe to connect to",
                                connect_failures + 1
                            ));
                        }
                        connect_failures += 1;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }

        // Check for the game process again.
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        #[cfg(windows)]
        reload_dbg("connect_and_run_parser: respawning check_and_perform_hook");
        tauri::async_runtime::spawn(check_and_perform_hook(app));
    });
}

fn system_tray_with_menu() -> SystemTray {
    let meter = CustomMenuItem::new("open_meter", "Open Meter");
    let logs = CustomMenuItem::new("open_logs", "Open Logs");
    let always_on_top = CustomMenuItem::new("always_on_top", "Always on top ✓");
    let toggle_clickthrough = CustomMenuItem::new("toggle_clickthrough", "Clickthrough");
    let reset_windows = CustomMenuItem::new("reset_windows", "Reset Windows");
    let quit = CustomMenuItem::new("quit", "Quit");

    let menu = SystemTrayMenu::new()
        .add_item(meter)
        .add_item(logs)
        .add_item(always_on_top)
        .add_item(toggle_clickthrough)
        .add_item(reset_windows);

    // Dev-only hook hot-reload. The English label is the startup default;
    // `update_tray_labels` localizes it once a webview has booted.
    #[cfg(all(windows, debug_assertions))]
    let menu = menu.add_item(CustomMenuItem::new("reload_hook", "Reload hook (dev)"));

    let menu = menu
        .add_native_item(SystemTrayMenuItem::Separator)
        .add_item(quit);

    SystemTray::new().with_menu(menu)
}

fn toggle_window_visibility(handle: &AppHandle, id: &str, focus: Option<bool>) {
    if let Some(window) = handle.get_window(id) {
        if let Some(focus_value) = focus {
            if focus_value {
                window.set_focus().unwrap();
            }
        }

        match window.is_visible().unwrap() {
            true => window.hide().unwrap(),
            false => window.show().unwrap(),
        }
    }
}

#[tauri::command]
fn toggle_always_on_top(
    window: tauri::Window,
    state: State<AlwaysOnTop>,
    labels: State<TrayLabels>,
) {
    let always_on_top = &state.0;
    let new_state = !always_on_top.load(Ordering::Acquire);
    always_on_top.store(new_state, Ordering::Release);
    if let Err(e) = window.set_always_on_top(new_state) {
        log::warn!("set_always_on_top({new_state}) failed: {e:?}");
    }
    let _ = window.emit("on-pinned", new_state);
    let title = if new_state {
        labels
            .always_on_top_active
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| "Always on top \u{2713}".into())
    } else {
        labels
            .always_on_top
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| "Always on top".into())
    };
    if let Some(item) = window
        .app_handle()
        .tray_handle()
        .try_get_item("always_on_top")
    {
        let _ = item.set_title(&title);
    }
}

#[tauri::command]
fn toggle_clickthrough(
    window: tauri::Window,
    state: State<ClickThrough>,
    labels: State<TrayLabels>,
) {
    let click_through = &state.0;
    let new_state = !click_through.load(Ordering::Acquire);
    click_through.store(new_state, Ordering::Release);
    if let Err(e) = window.set_ignore_cursor_events(new_state) {
        log::warn!("set_ignore_cursor_events({new_state}) failed: {e:?}");
    }
    let _ = window.emit("on-clickthrough", new_state);
    let title = if new_state {
        labels
            .clickthrough_active
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| "Clickthrough \u{2713}".into())
    } else {
        labels
            .clickthrough
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| "Clickthrough".into())
    };
    if let Some(item) = window
        .app_handle()
        .tray_handle()
        .try_get_item("toggle_clickthrough")
    {
        let _ = item.set_title(&title);
    }
}

#[tauri::command]
fn reset_meter_window(app_handle: AppHandle) {
    reset_window_to_default(&app_handle, "main", true);
}

/// Show `label` and restore the default geometry declared for it in
/// tauri.conf.json. Shared by the in-app reset command and the tray's
/// "Reset Windows" item so the two can't drift apart.
fn reset_window_to_default(handle: &AppHandle, label: &str, center: bool) {
    let Some(window) = handle.get_window(label) else {
        return;
    };
    let _ = window.show();
    let _ = window.unminimize();
    match handle
        .config()
        .tauri
        .windows
        .iter()
        .find(|w| w.label == label)
    {
        Some(default) => {
            let _ = window.set_size(Size::Logical(LogicalSize {
                width: default.width,
                height: default.height,
            }));
        }
        // Resetting is the escape hatch for a window dragged off-screen or
        // sized to nothing, so a config that no longer declares this label
        // should be loud rather than a silent no-op.
        None => log::warn!("no window config for label {label:?}; size not reset"),
    }
    if center {
        let _ = window.center();
    }
}

fn menu_tray_handler(handle: &AppHandle, event: SystemTrayEvent) {
    let should_focus = true;
    match event {
        SystemTrayEvent::LeftClick { .. } => {
            toggle_window_visibility(handle, "main", Some(should_focus))
        }
        SystemTrayEvent::MenuItemClick { id, .. } => match id.as_str() {
            "open_meter" => toggle_window_visibility(handle, "main", Some(should_focus)),
            "open_logs" => toggle_window_visibility(handle, "logs", Some(should_focus)),
            "toggle_clickthrough" => toggle_clickthrough(
                handle.get_window("main").unwrap(),
                handle.state::<ClickThrough>(),
                handle.state::<TrayLabels>(),
            ),
            "always_on_top" => toggle_always_on_top(
                handle.get_window("main").unwrap(),
                handle.state::<AlwaysOnTop>(),
                handle.state::<TrayLabels>(),
            ),
            "reset_windows" => {
                reset_window_to_default(handle, "main", false);
                reset_window_to_default(handle, "logs", false);
            }
            "quit" => {
                let _ = handle.save_window_state(StateFlags::all());
                handle.exit(0)
            }
            #[cfg(all(windows, debug_assertions))]
            "reload_hook" => {
                tauri::async_runtime::spawn(reload_hook(handle.clone()));
            }
            _ => {}
        },
        _ => {} // Ignore rest of the events.
    }
}

fn show_window(app: &AppHandle) {
    let windows = app.windows();

    for window in windows.values() {
        let _ = window.show();
    }
}

fn main() {
    // The overlay depends on WM hints native Wayland refuses to clients
    // (always-on-top, clickthrough); route through XWayland — the game under
    // Proton is an XWayland window anyway. Respect an explicit user choice.
    #[cfg(target_os = "linux")]
    if std::env::var_os("GDK_BACKEND").is_none() {
        std::env::set_var("GDK_BACKEND", "x11");
    }

    // The transparent overlay only stays transparent on WebKitGTK's software
    // (Cairo) path: when accelerated compositing engages — it's on-demand, a
    // CSS animation can trigger it mid-session — the GL surface loses its
    // alpha channel and the whole webview composites onto an opaque backdrop,
    // so the transparency slider appears dead (field report). Keep compositing
    // off; must be set before any webview exists, and only bites with the
    // webkit 2.36 the AppImage pins (2.44+ dropped these escape hatches).
    // Respect an explicit user choice.
    #[cfg(target_os = "linux")]
    if std::env::var_os("WEBKIT_DISABLE_COMPOSITING_MODE").is_none() {
        std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
    }

    // --- Portable mode: keep WebView2 cache in the EXE's own directory ---
    gbfr_logs::portable::ensure_dirs().expect("Failed to create portable directories");
    std::env::set_var(
        "WEBVIEW2_USER_DATA_FOLDER",
        gbfr_logs::portable::webview_data_dir(),
    );

    info!("Starting application..");

    // Setup the database.
    db::setup_db().expect("Failed to setup database");

    // Deliberately not fatal, unlike logs.db: settings are not load-bearing for
    // parsing damage, so a corrupt or unwritable settings.db degrades to
    // localStorage-only behaviour instead of stopping the app from starting.
    if let Err(e) = settings_db::setup() {
        error!("Failed to set up settings.db, settings will not persist: {e}");
    }

    info!("Database setup complete, launching application..");

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_window(app);
        }))
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .plugin(
            tauri_plugin_log::Builder::default()
                .targets([
                    LogTarget::Folder(gbfr_logs::data_paths::data_dir().join("logs")),
                    LogTarget::Stdout,
                ])
                .level(LevelFilter::Warn)
                .level_for("tao", LevelFilter::Error)
                .build(),
        )
        .manage(AlwaysOnTop(AtomicBool::new(true)))
        .manage(ClickThrough(AtomicBool::new(false)))
        .manage(DebugMode(AtomicBool::new(false)))
        .manage(ResetChannel(std::sync::Mutex::new(None)))
        .manage(RepublishChannel(std::sync::Mutex::new(None)))
        .manage(MeterFilterState::default())
        .manage(HookStatus::default())
        .manage(TrayLabels::default())
        .system_tray(system_tray_with_menu())
        .on_system_tray_event(menu_tray_handler)
        .on_window_event(|event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event.event() {
                event.window().hide().unwrap();
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            fetch_encounter_state,
            fetch_encounter_events,
            fetch_logs,
            fetch_log_summaries,
            fetch_conflux_runs,
            fetch_legality_players,
            delete_logs,
            delete_all_logs,
            pick_logs_db_file,
            analyze_logs_db,
            import_logs_from_file,
            count_imported_logs,
            delete_imported_logs,
            export_logs_db,
            toggle_always_on_top,
            toggle_clickthrough,
            reset_meter_window,
            export_damage_log_to_file,
            set_debug_mode,
            reset_encounter,
            request_live_republish,
            set_meter_filters,
            fetch_synthesis_status,
            fetch_synthesis_seed,
            fetch_overmastery_status,
            predict_overmastery,
            fetch_rng_slot,
            fetch_transmarvel_status,
            predict_transmarvel,
            search_synthesis,
            get_full_assist_unlock,
            set_full_assist_unlock,
            fetch_linux_setup_status,
            deploy_linux_hook,
            remove_linux_hook,
            get_hook_status,
            refresh_hook,
            debug_eject_hook,
            debug_allow_reinject,
            debug_hold_out_state,
            debug_set_hello_override,
            debug_clear_hello_override,
            debug_broadcast_scenario,
            update_tray_labels,
            get_settings,
            set_setting,
            delete_setting,
            export_settings_db,
            pick_settings_db_file,
            import_settings_from_file,
            reset_settings_to_default,
        ])
        .setup(|app| {
            // Before anything waits on a game: if antivirus took the hook DLL,
            // the badge says so from the first frame rather than after the
            // user has launched the game and watched an empty meter.
            #[cfg(windows)]
            refresh_hook_dll_presence(&app.handle());

            // Perform the game hook check in a separate thread.
            tauri::async_runtime::spawn(check_and_perform_hook(app.handle()));
            // Outlives individual connections on purpose: it must also run for
            // a hook that was already mapped when the app started.
            tauri::async_runtime::spawn(hook_heartbeat(app.handle()));

            // Re-audit any log the current rules have not judged — every log
            // recorded before the legality work, and every log at all after a
            // rule changes. On its own thread so a rules bump never delays the
            // window appearing.
            let handle = app.handle();
            std::thread::spawn(move || sweep_stale_legality(&handle));

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod ability_series_query_tests {
    use super::{AbilitySeriesMetric, AbilitySeriesQuery};
    use serde_json::json;

    /// The wire contract with `AnalysisView`'s `abilityQuery` memo. Locked by a
    /// test for the same reason `groups.rs` locks `GroupQuery`'s: the frontend
    /// builds this by hand, and a silent rename would degrade to "no bands"
    /// rather than to an error anyone would notice.
    #[test]
    fn deserializes_a_pinned_request() {
        let query: AbilitySeriesQuery =
            serde_json::from_value(json!({ "metric": "stun", "player": 3 }))
                .expect("valid AbilitySeriesQuery JSON");

        assert_eq!(query.metric, AbilitySeriesMetric::Stun);
        assert_eq!(query.player, Some(3));
    }

    /// Stun's ability level widens to the whole party when no source is pinned
    /// (see `metrics/stun.ts`), so `player` is optional rather than required.
    #[test]
    fn defaults_to_every_player() {
        let query: AbilitySeriesQuery =
            serde_json::from_value(json!({ "metric": "sba" })).expect("valid JSON");

        assert_eq!(query.metric, AbilitySeriesMetric::Sba);
        assert_eq!(query.player, None);
    }
}
