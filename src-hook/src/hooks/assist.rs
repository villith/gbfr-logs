//! Infinity Full Assist unlock (DEV BUILDS ONLY — cargo feature `fullassist`).
//!
//! Unlike every other hook in this crate, this one CHANGES GAME BEHAVIOR instead of
//! observing it: it lets the game's built-in Full Assist AI keep running in Infinity
//! quests, which the game normally disables. It is compiled only when the
//! `fullassist` feature is on (`npm run dev`), so a release `hook.dll` does not
//! contain this code at all.
//!
//! * `FullAssistGate` (rva 0x219290) is a 0-arg function returning a bool byte that
//!   the assist-disable path consults. Returning 1 keeps assist enabled. We detour it,
//!   call the original, and only ever flip 0 -> 1 — never 1 -> 0.
//! * `gate+4` is `mov rcx,[rip+disp]` -> the quest-state global; `gate+16` is a
//!   `call` to `GetCurrentQuestId(questState, &out)`.
//! * `AssistDisableTermHandler+23` is `mov rdi,[rip+disp]` -> the assist-selection
//!   global; `*global + 0x10` holds the player's selected mode (2 = Full Assist).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::{anyhow, Result};
use retour::static_detour;

use crate::hooks::diag::readable;
use crate::process::Process;

/// `FullAssistGate` — 0-arg, returns a bool byte. Matches its own prologue (the match
/// start IS the entry, confirmed by the `cc` padding before it), so we detour it directly.
const FULL_ASSIST_GATE_SIG: &str =
    "48 83 EC 28 48 8B 0D ? ? ? ? 48 8D 54 24 24 E8 ? ? ? ? 8B 4C 24 24 89 C8 C1 E8 14";

/// `AssistDisableTermHandler` — not hooked; we only need the RIP-relative operand at +23
/// to find the global holding the player's assist-mode selection.
const ASSIST_DISABLE_HANDLER_SIG: &str = "56 57 48 83 EC 28 48 8B 05 ? ? ? ? 0F B6 49 30 88 88 4E 0E 00 00 48 8B 3D ? ? ? ? 31 F6 E8 ? ? ? ? B9 00 00 00 00 84 C0 74 ? 8B 47 10";

/// `mov rcx,[rip+disp32]` at `gate+4`: the quest-state global.
const GATE_QUEST_STATE_INSN: usize = 4;
/// `call rel32` at `gate+16`: `GetCurrentQuestId(questState, &out)`.
const GATE_QUEST_ID_CALL: usize = 16;
/// `mov rdi,[rip+disp32]` at `handler+23`: the assist-selection global.
const HANDLER_ASSIST_SELECTION_INSN: usize = 23;
/// Selected assist mode lives at `*assist_selection_global + 0x10`.
const ASSIST_MODE_OFFSET: usize = 0x10;

type GetCurrentQuestId = unsafe extern "system" fn(*const usize, *mut u32);

static_detour! {
    static FullAssistGate: unsafe extern "system" fn() -> u8;
}

#[cfg(any(feature = "eject", test))]
pub(super) fn disable() {
    super::disable_quiet("FullAssistGate", &FullAssistGate);
}

static UNLOCK_ENABLED: AtomicBool = AtomicBool::new(false);
static QUEST_STATE_GLOBAL: AtomicUsize = AtomicUsize::new(0);
static ASSIST_SELECTION_GLOBAL: AtomicUsize = AtomicUsize::new(0);
static QUEST_ID_GETTER: AtomicUsize = AtomicUsize::new(0);

/// Quest ids pack the type as `category = (id >> 20) & 0xFF`, `sub = (id >> 12) & 0xFF`.
const INFINITY_CATEGORY: u32 = 4;
const INFINITY_SUB_CATEGORY: u32 = 11;

/// Quest types confirmed NOT to be Infinity. Anything else with a readable type is
/// "unknown" and falls through to the id allowlist rather than being assumed safe.
const KNOWN_NON_INFINITY_TYPES: [(u32, u32); 2] = [(4, 8), (4, 10)];

/// Infinity quests confirmed by id, used when the packed type can't be read.
const FALLBACK_INFINITY_QUEST_IDS: [u32; 5] = [4240129, 4240148, 4240147, 4240137, 4240150];

/// Assist-mode selection byte (`*assist_selection_global + 0x10`).
const FULL_ASSIST_MODE: u8 = 2;

/// How the packed quest id classifies.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum QuestType {
    /// No quest id available (0 / sentinel).
    Unavailable,
    /// Type bits present but not one we recognise.
    Unknown,
    Infinity,
    NonInfinity,
}

fn resolve_quest_type(quest_id: u32) -> QuestType {
    if quest_id == 0 || quest_id == u32::MAX {
        return QuestType::Unavailable;
    }

    // A set high nibble means this isn't a plain packed quest id; don't trust the
    // type bits we'd decode out of it.
    if quest_id & 0xF000_0000 != 0 {
        return QuestType::Unknown;
    }

    let category = (quest_id >> 20) & 0xFF;
    let sub_category = (quest_id >> 12) & 0xFF;

    if (category, sub_category) == (INFINITY_CATEGORY, INFINITY_SUB_CATEGORY) {
        QuestType::Infinity
    } else if KNOWN_NON_INFINITY_TYPES.contains(&(category, sub_category)) {
        QuestType::NonInfinity
    } else {
        QuestType::Unknown
    }
}

fn is_infinity_quest(quest_id: u32) -> bool {
    match resolve_quest_type(quest_id) {
        QuestType::Infinity => true,
        QuestType::NonInfinity => false,
        // Type unreadable or unrecognised: only the confirmed ids count. Defaulting to
        // "not Infinity" keeps us from touching quests we can't identify.
        QuestType::Unavailable | QuestType::Unknown => {
            FALLBACK_INFINITY_QUEST_IDS.contains(&quest_id)
        }
    }
}

/// The whole decision, as plain values so it is testable without a game.
///
/// Only reached once [`run`] has established that the game's own gate said "no" and the
/// unlock is armed — passing those two in as parameters made them look variable when both
/// are constant at the only call site, so the terms could never be false.
fn should_unlock(assist_mode: u8, quest_id: u32) -> bool {
    assist_mode == FULL_ASSIST_MODE && is_infinity_quest(quest_id)
}

/// Resolve a `[rip+disp32]` operand into the absolute address it points at.
/// `disp_offset` is where the disp32 sits inside the instruction, `insn_len` its length.
fn resolve_rip_relative(insn: usize, disp_offset: usize, insn_len: usize) -> Option<usize> {
    if !readable(insn, insn_len) {
        return None;
    }

    let disp = unsafe { ((insn + disp_offset) as *const i32).read_unaligned() };
    (insn + insn_len).checked_add_signed(disp as isize)
}

/// Resolve the target of a `call rel32` at `insn`.
fn resolve_call_target(insn: usize) -> Option<usize> {
    if !readable(insn, 5) {
        return None;
    }

    // Refuse anything that isn't actually an E8 call — the sig should guarantee it, but a
    // future patch that shifts the instruction must fail loudly rather than call garbage.
    if unsafe { (insn as *const u8).read() } != 0xE8 {
        return None;
    }

    let rel = unsafe { ((insn + 1) as *const i32).read_unaligned() };
    (insn + 5).checked_add_signed(rel as isize)
}

/// Dereference a global holding a pointer, rejecting anything unreadable.
///
/// Goes through the crate's shared guarded reader rather than a private raw deref, so this
/// inherits whatever `readable`'s SEH probe strategy is — that guard is safety-critical
/// here (it was the fix for both the in-combat slowdown and the identity crash) and a
/// second bypassing copy would silently miss any future change to it.
fn read_global_ptr(global: usize) -> Option<usize> {
    crate::hooks::diag::read_ptr_guarded(global, 0)
        .filter(|&ptr| ptr != 0 && readable(ptr, std::mem::size_of::<usize>()))
}

/// The player's selected assist mode, or `None` if the chain isn't readable.
fn read_assist_mode() -> Option<u8> {
    let selection = read_global_ptr(ASSIST_SELECTION_GLOBAL.load(Ordering::Relaxed))?;
    if !readable(selection + ASSIST_MODE_OFFSET, 1) {
        return None;
    }

    Some(unsafe { ((selection + ASSIST_MODE_OFFSET) as *const u8).read() })
}

/// The current quest id, via the game's own getter — the same call the original gate just
/// made, so it is safe to invoke from inside the detour.
fn read_quest_id() -> Option<u32> {
    let quest_state = read_global_ptr(QUEST_STATE_GLOBAL.load(Ordering::Relaxed))?;
    let getter = QUEST_ID_GETTER.load(Ordering::Relaxed);
    if getter == 0 {
        return None;
    }

    let mut quest_id: u32 = 0;
    unsafe {
        let getter: GetCurrentQuestId = std::mem::transmute(getter);
        getter(quest_state as *const usize, &mut quest_id);
    }

    Some(quest_id)
}

/// Whether the setting file asks for the unlock. Read ONCE at injection: the app rewrites
/// `hook-config.json` before injecting, so the value is current for this game session and
/// toggling it takes effect on the next launch.
fn read_unlock_setting() -> bool {
    let Some(mut path) = crate::get_dll_dir() else {
        return false;
    };
    path.push("portable_data");
    path.push("hook-config.json");

    let Ok(contents) = std::fs::read_to_string(&path) else {
        return false;
    };

    serde_json::from_str::<serde_json::Value>(&contents)
        .ok()
        .and_then(|v| v.get("unlock_full_assist_infinity")?.as_bool())
        .unwrap_or(false)
}

pub struct OnFullAssistGateHook;

impl OnFullAssistGateHook {
    pub fn new() -> Self {
        Self
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        if !read_unlock_setting() {
            log::info!("Infinity Full Assist unlock is off; gate hook not installed");
            #[cfg(feature = "console")]
            println!("full assist unlock: off (gate hook not installed)");
            return Ok(());
        }

        let gate = process.search_match_address(FULL_ASSIST_GATE_SIG)?;
        let handler = process.search_match_address(ASSIST_DISABLE_HANDLER_SIG)?;

        let quest_state_global = resolve_rip_relative(gate + GATE_QUEST_STATE_INSN, 3, 7)
            .ok_or_else(|| anyhow!("Could not resolve the quest-state global"))?;
        let assist_selection_global =
            resolve_rip_relative(handler + HANDLER_ASSIST_SELECTION_INSN, 3, 7)
                .ok_or_else(|| anyhow!("Could not resolve the assist-selection global"))?;
        let quest_id_getter = resolve_call_target(gate + GATE_QUEST_ID_CALL)
            .ok_or_else(|| anyhow!("Could not resolve the quest-id getter"))?;

        QUEST_STATE_GLOBAL.store(quest_state_global, Ordering::Relaxed);
        ASSIST_SELECTION_GLOBAL.store(assist_selection_global, Ordering::Relaxed);
        QUEST_ID_GETTER.store(quest_id_getter, Ordering::Relaxed);
        UNLOCK_ENABLED.store(true, Ordering::Relaxed);

        #[cfg(feature = "console")]
        println!(
            "full assist unlock: gate={gate:#x} quest_state={quest_state_global:#x} \
             assist_selection={assist_selection_global:#x} getter={quest_id_getter:#x}"
        );

        unsafe {
            let func: unsafe extern "system" fn() -> u8 = std::mem::transmute(gate);
            FullAssistGate.initialize(func, move || run())?;
            FullAssistGate.enable()?;
        }

        Ok(())
    }
}

impl Default for OnFullAssistGateHook {
    fn default() -> Self {
        Self::new()
    }
}

/// The detour. Only ever turns the game's `false` into `true`; on any unreadable pointer,
/// failed read, or panic it returns the game's own answer untouched.
fn run() -> u8 {
    let original = unsafe { FullAssistGate.call() };

    if original != 0 || !UNLOCK_ENABLED.load(Ordering::Relaxed) {
        return original;
    }

    // A panic here would unwind across the FFI boundary into game code (UB, and in practice
    // a silent freeze), so it is contained and treated as "leave the original result".
    let unlock = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let assist_mode = read_assist_mode()?;
        let quest_id = read_quest_id()?;
        Some(should_unlock(assist_mode, quest_id))
    }))
    .unwrap_or(None);

    if unlock == Some(true) {
        1
    } else {
        original
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 0x40B001 -> category 4, sub 11.
    const INFINITY_QUEST: u32 = 4240129;
    /// 0x40800A -> category 4, sub 8 (a known non-Infinity multiplayer quest).
    const NON_INFINITY_QUEST: u32 = 0x40800A;

    #[test]
    fn classifies_infinity_quest_by_packed_type() {
        assert_eq!(resolve_quest_type(INFINITY_QUEST), QuestType::Infinity);
    }

    #[test]
    fn classifies_known_non_infinity_quest_by_packed_type() {
        assert_eq!(
            resolve_quest_type(NON_INFINITY_QUEST),
            QuestType::NonInfinity
        );
    }

    #[test]
    fn treats_absent_quest_id_as_unavailable() {
        assert_eq!(resolve_quest_type(0), QuestType::Unavailable);
        assert_eq!(resolve_quest_type(u32::MAX), QuestType::Unavailable);
    }

    #[test]
    fn treats_high_nibble_quest_id_as_unknown() {
        assert_eq!(resolve_quest_type(0x1040_B001), QuestType::Unknown);
    }

    #[test]
    fn falls_back_to_the_id_allowlist_when_the_type_is_unknown() {
        // Same id, type bits obscured by a high nibble: the allowlist still recognises it.
        assert!(!is_infinity_quest(0x1000_0001));
        assert!(is_infinity_quest(4240137));
    }

    #[test]
    fn unlocks_full_assist_in_an_infinity_quest() {
        assert!(should_unlock(FULL_ASSIST_MODE, INFINITY_QUEST));
    }

    #[test]
    fn does_not_unlock_plain_assist_mode() {
        assert!(!should_unlock(1, INFINITY_QUEST));
    }

    #[test]
    fn does_not_unlock_outside_infinity_quests() {
        assert!(!should_unlock(FULL_ASSIST_MODE, NON_INFINITY_QUEST));
    }
}
