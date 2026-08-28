//! Battle-state polling: Link Time and enemy mode (Normal / Overdrive /
//! Break) transitions, published as latched change events from the per-frame
//! quest-sequence tick ([`super::quest`] calls [`poll`]). No detours of its
//! own — everything here is guarded data reads over structures the game's own
//! behavior-tree conditions read.
//!
//! Where the reads come from (v2.0.3, `gbfr203fast` decompiles, 2026-08-06):
//!
//! * **Link Time** — `BT::EmLinkTimeCondition`'s evaluate virtual
//!   (`FUN_140d43f10`) is one read: `*(byte*)(DAT_147030fe0 + 0x10) & 1`,
//!   i.e. a singleton pointer at RVA `0x7030fe0` whose byte `+0x10` carries
//!   the link-time-active bit. The enemy AI itself branches on this, so it is
//!   authoritative for "the party is inside Link Time".
//! * **Enemy mode** — `BT::EmBossOverDriveCondition`'s evaluate
//!   (`FUN_140d392b0`) resolves the enemy's entity handle against the entity
//!   table (`DAT_14701e4a8`, the same `ENTITY_TABLE_RVA` the SBA poll
//!   validates against), takes the specified actor at entity`+0x70`, looks a
//!   component up in the `+0xC0` map by a static-init type id
//!   (`DAT_147bb6b4c`, guard dword at `+4` — same protocol as
//!   `SBA_COMPONENT_TYPE_RVA`), and reads `*(u32*)(component + 0x10)` as the
//!   mode, bounds-checked `< 6` against an identity mapping table
//!   (`DAT_145e80260`). The damage hooks feed this module the specified
//!   pointers of recently-hit enemies, so the poll never has to enumerate the
//!   entity table.
//!
//! Mode value SEMANTICS (which of 0..6 is Break) are the protocol's
//! `EnemyModeEvent::MODE_*` constants — the raw value is forwarded and stored,
//! so a live capture correcting the mapping re-interprets old logs instead of
//! invalidating them.

use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use protocol::Message;

use crate::event;
use crate::hooks::diag::{read_ptr_guarded, read_u32_guarded, MODULE_BASE};

/// The battle singleton `EmLinkTimeCondition` reads (v2.0.3). Byte `+0x10`
/// bit 0 = link time active.
const LINK_TIME_SINGLETON_RVA: usize = 0x7031200; // 2.0.4: 0x7030fe0
/// Byte offset of the link-time flag inside the singleton.
const LINK_TIME_FLAG_OFFSET: usize = 0x10;

/// The enemy mode component's static-init type id (v2.0.3); guard dword at
/// `+4`, same `_Init_thread` protocol as `SBA_COMPONENT_TYPE_RVA` (see
/// `poll_context` in sba.rs for the guard semantics).
const EM_MODE_COMPONENT_TYPE_RVA: usize = 0x7bb804c; // 2.0.4: 0x7bb7dcc
/// Mode field inside the resolved component.
const EM_MODE_OFFSET: usize = 0x10;
/// The game's raw mode enum has six values (the BT mapping table
/// `DAT_145e80260` is `[0,1,2,3,4,5]`); anything else is a stale or garbage
/// read and is dropped.
const EM_MODE_LIMIT: u32 = 6;

/// How many recently-hit enemies the mode poll tracks. Fights meter at most a
/// handful of simultaneous mode-bearing enemies; the cap bounds the per-tick
/// work, evicting the least-recently-hit entry.
const TRACKED_ENEMY_CAP: usize = 8;
/// An entry unhit for this long is dropped: its specified pointer may be a
/// freed allocation, and while every read is guarded, a REUSED allocation
/// would produce plausible-looking garbage no guard can catch. Break windows
/// are exactly when everyone is hitting the enemy, so a live target never
/// expires.
const TRACKED_ENEMY_TTL_SECS: u64 = 30;

/// Latched link-time state: 0 unknown (nothing emitted yet), 1 inactive,
/// 2 active.
static LINK_STATE: AtomicU8 = AtomicU8::new(0);

/// Enemy-mode polls run every Nth tick — mode phases last seconds, and the
/// component walk is a dozen guarded reads per enemy.
static POLL_TICK: AtomicU32 = AtomicU32::new(0);
const ENEMY_POLL_DIVIDER: u32 = 6;

struct TrackedEnemy {
    /// The enemy's specified-instance pointer (what the damage hook resolves
    /// components and indexes from).
    specified: usize,
    /// The index its damage events carry (`target.parent_index`), so a mode
    /// event joins the same enemy the parser already tracks.
    actor_index: u32,
    /// Last emitted mode; `u32::MAX` until the first successful read.
    last_mode: u32,
    last_hit: Instant,
}

static TRACKED: Mutex<Vec<TrackedEnemy>> = Mutex::new(Vec::new());

/// Notes an enemy the damage funnel just saw, so the mode poll knows where to
/// look. Called from `build_damage_event`'s enemy-victim path per hit — cheap
/// (one bounded Vec scan under a mutex nothing holds long).
pub(crate) fn note_enemy_target(specified: usize, actor_index: u32) {
    if specified == 0 {
        return;
    }
    let Ok(mut tracked) = TRACKED.lock() else {
        return;
    };
    if let Some(entry) = tracked
        .iter_mut()
        .find(|entry| entry.specified == specified)
    {
        entry.actor_index = actor_index;
        entry.last_hit = Instant::now();
        return;
    }
    if tracked.len() >= TRACKED_ENEMY_CAP {
        // Evict the least-recently-hit entry rather than refusing the new
        // one — the new target is demonstrably live, the old one may not be.
        if let Some(oldest) = tracked
            .iter()
            .enumerate()
            .min_by_key(|(_, entry)| entry.last_hit)
            .map(|(position, _)| position)
        {
            tracked.remove(oldest);
        }
    }
    tracked.push(TrackedEnemy {
        specified,
        actor_index,
        last_mode: u32::MAX,
        last_hit: Instant::now(),
    });
}

/// Clears the tracked-enemy table (quest boundaries — stale pointers must not
/// survive into the next fight's poll).
pub(crate) fn reset() {
    if let Ok(mut tracked) = TRACKED.lock() {
        tracked.clear();
    }
    LINK_STATE.store(0, Ordering::Relaxed);
}

/// One per-frame poll, called from the quest-sequence tick. Emits only
/// transitions.
pub(crate) fn poll(tx: &event::Tx) {
    let base = MODULE_BASE.load(Ordering::Relaxed);
    if base == 0 {
        return;
    }

    poll_link_time(tx, base);

    if POLL_TICK.fetch_add(1, Ordering::Relaxed) % ENEMY_POLL_DIVIDER == 0 {
        poll_enemy_modes(tx, base);
    }
}

fn poll_link_time(tx: &event::Tx, base: usize) {
    // A missing singleton (town, teardown) reads as inactive: if we said
    // "active" the fight's window must still be closed.
    let active = read_ptr_guarded(base, LINK_TIME_SINGLETON_RVA)
        .filter(|singleton| *singleton != 0)
        .map(|singleton| read_u32_guarded(singleton, LINK_TIME_FLAG_OFFSET) & 1 != 0)
        .unwrap_or(false);

    let next = if active { 2 } else { 1 };
    let last = LINK_STATE.swap(next, Ordering::Relaxed);
    if last == next {
        return;
    }
    // The very first observation is only worth announcing when it is ACTIVE —
    // "link time is not running" is the assumed state.
    if last == 0 && !active {
        return;
    }
    crate::hooks::diag::ev!("link_time", "active={active}");
    let _ = tx.send(Message::LinkTime(protocol::LinkTimeEvent { active }));
}

fn poll_enemy_modes(tx: &event::Tx, base: usize) {
    // Same static-init guard protocol as sba.rs's poll_context: 0 = never
    // initialized, -1 = in progress, 0x8000xxxx = ready.
    let type_guard = read_u32_guarded(base, EM_MODE_COMPONENT_TYPE_RVA + 4);
    if type_guard == 0 || type_guard == 0xFFFF_FFFF {
        return;
    }
    let type_id = read_u32_guarded(base, EM_MODE_COMPONENT_TYPE_RVA);

    let Ok(mut tracked) = TRACKED.lock() else {
        return;
    };
    tracked.retain(|entry| entry.last_hit.elapsed().as_secs() < TRACKED_ENEMY_TTL_SECS);

    for entry in tracked.iter_mut() {
        let Some(component) = super::sba::game_stdmap_find(entry.specified + 0xC0, type_id) else {
            continue;
        };
        let mode = read_u32_guarded(component, EM_MODE_OFFSET);
        if mode >= EM_MODE_LIMIT || mode == entry.last_mode {
            continue;
        }
        // Underscored: the ev! body compiles away without the hookdiag feature.
        let (_idx, _last) = (entry.actor_index, entry.last_mode);
        crate::hooks::diag::ev!("enemy_mode", "idx={_idx:#x} mode {_last}->{mode}");
        entry.last_mode = mode;
        let _ = tx.send(Message::EnemyMode(protocol::EnemyModeEvent {
            actor_index: entry.actor_index,
            mode,
        }));
    }
}
