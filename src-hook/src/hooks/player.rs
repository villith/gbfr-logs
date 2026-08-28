use std::{
    ffi::{c_void, CStr, CString},
    sync::{Mutex, OnceLock},
};

use anyhow::{anyhow, Result};
use protocol::{Message, PlayerIdentityEvent};
use retour::static_detour;
use windows::Win32::{Foundation::HANDLE, System::Diagnostics::Debug::ReadProcessMemory};

use crate::{
    event,
    hooks::{
        actor_type_id,
        ffi::{Overmasteries, PlayerStats, SigilEntry, SigilList, VBuffer, WeaponInfo},
        globals::{OVERMASTERY_OFFSET, PLAYER_DATA_OFFSET, SIGIL_OFFSET, WEAPON_OFFSET},
    },
    process::Process,
};

type OnLoadPlayerFunc = unsafe extern "system" fn(*const usize) -> usize;
type RefreshPlayerIdentityFunc = unsafe extern "system" fn(*const usize);

static_detour! {
    static OnLoadPlayer: unsafe extern "system" fn(*const usize) -> usize;
    static RefreshPlayerIdentity: unsafe extern "system" fn(*const usize);
}

#[cfg(feature = "hookdiag")]
static_detour! {
    /// FUN_1407a1dc0 — serializes a loadout struct into the compact player
    /// profile blob (the thing network/remote records carry). Hooked in hookdiag
    /// builds only, to observe WHEN profiles are built and whether the source
    /// loadout carries equipped summons at that moment (the town roster records
    /// dump with all four summon slots empty).
    static BuildPlayerProfile: unsafe extern "system" fn(*mut u8, *mut u8);
}

#[cfg(any(feature = "eject", test))]
pub(super) fn disable() {
    super::disable_quiet("OnLoadPlayer", &OnLoadPlayer);
    super::disable_quiet("RefreshPlayerIdentity", &RefreshPlayerIdentity);
    // BuildPlayerProfile only exists in hookdiag builds (see its static_detour
    // block above), so its teardown must be gated the same way.
    #[cfg(feature = "hookdiag")]
    super::disable_quiet("BuildPlayerProfile", &BuildPlayerProfile);
}

/// Entry prologue of FUN_1407a1dc0 (profile <- loadout serializer). VERIFIED
/// unique (1 match, clean entry, 2 args rcx=profile dest / rdx=loadout src) on
/// v2.0.2 via sigscan; the embedded `add rdx, 0x34E8` displacement anchors it.
#[cfg(feature = "hookdiag")]
const BUILD_PLAYER_PROFILE_SIG: &str =
    "41 56 56 57 53 48 83 ec 28 48 89 d7 48 89 ce 48 81 c2 e8 34 00 00";

#[derive(Clone)]
pub struct OnLoadPlayerHook {
    tx: event::Tx,
}

impl OnLoadPlayerHook {
    pub fn new(tx: event::Tx) -> Self {
        OnLoadPlayerHook { tx }
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        let cloned_self = self.clone();

        if let Ok(on_load_player_original) =
            process.search_address("49 89 ce e8 $ { ' } 31 ff 85 c0 ? ? ? ? ? ? 49 8b 46 28")
        {
            #[cfg(feature = "console")]
            println!("Found on load player");

            unsafe {
                let func: OnLoadPlayerFunc = std::mem::transmute(on_load_player_original);
                OnLoadPlayer.initialize(func, move |a1| cloned_self.run(a1))?;
                OnLoadPlayer.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find on_load_player"));
        }

        Ok(())
    }

    fn run(&self, a1: *const usize) -> usize {
        #[cfg(feature = "console")]
        println!("on load player: {:p}", a1);

        let ret = unsafe { OnLoadPlayer.call(a1) };

        let player_idx = unsafe { a1.byte_add(0x170).read() } as u32;

        let player_offset = PLAYER_DATA_OFFSET.load(std::sync::atomic::Ordering::Relaxed);
        let weapon_offset = WEAPON_OFFSET.load(std::sync::atomic::Ordering::Relaxed);
        let overmastery_offset = OVERMASTERY_OFFSET.load(std::sync::atomic::Ordering::Relaxed);
        let sigil_offset = SIGIL_OFFSET.load(std::sync::atomic::Ordering::Relaxed);

        // If any offset failed to resolve (a game patch broke its signature; setup_globals
        // now logs-and-continues instead of aborting, leaving the offset at its default 0),
        // the struct pointers below would be computed at a1+0 and, worse, the sigil pointer
        // would be read from *(a1+0) — the object's vtable pointer reinterpreted as a
        // SigilList*, which is non-null and so passes the NonNull guard before being
        // dereferenced. Bail rather than read/deref garbage on the game thread.
        if player_offset == 0 || weapon_offset == 0 || overmastery_offset == 0 || sigil_offset == 0
        {
            log::warn!(
                "player_load: skipping, unresolved offset(s) player_data={player_offset:#x} \
                 weapon={weapon_offset:#x} overmastery={overmastery_offset:#x} sigil={sigil_offset:#x}"
            );
            return ret;
        }

        let raw_player_stats = std::ptr::NonNull::new(
            unsafe { a1.byte_add(player_offset as usize) } as *mut PlayerStats,
        );

        let raw_weapon_info = std::ptr::NonNull::new(
            unsafe { a1.byte_add(weapon_offset as usize) } as *mut WeaponInfo,
        );

        let raw_overmastery_info =
            std::ptr::NonNull::new(
                unsafe { a1.byte_add(overmastery_offset as usize) } as *mut Overmasteries
            );

        let sigil_list = std::ptr::NonNull::new(
            unsafe { a1.byte_add(sigil_offset as usize).read() } as *mut SigilList,
        );

        if let (
            Some(raw_player_stats),
            Some(weapon_info),
            Some(overmastery_info),
            Some(sigil_list),
        ) = (
            raw_player_stats,
            raw_weapon_info,
            raw_overmastery_info,
            sigil_list,
        ) {
            let character_type = actor_type_id(a1);
            let player_stats = unsafe { raw_player_stats.as_ref() };
            let weapon_info = unsafe { weapon_info.as_ref() };
            let overmastery_info = unsafe { overmastery_info.as_ref() };
            let sigil_list = unsafe { sigil_list.as_ref() };

            if (sigil_list.party_index as u8) == 0xFF && sigil_list.is_online == 0 {
                return ret;
            }

            let sigils = sigil_list
                .sigils
                .iter()
                .map(|sigil| protocol::Sigil {
                    first_trait_id: sigil.first_trait_id,
                    first_trait_level: sigil.first_trait_level,
                    second_trait_id: sigil.second_trait_id,
                    second_trait_level: sigil.second_trait_level,
                    sigil_id: sigil.sigil_id,
                    equipped_character: sigil.equipped_character,
                    sigil_level: sigil.sigil_level,
                    acquisition_count: sigil.acquisition_count,
                    notification_enum: sigil.notification_enum,
                })
                .collect();

            let character_name = CStr::from_bytes_until_nul(&sigil_list.character_name)
                .ok()
                .map(|cstr| cstr.to_owned())
                .unwrap_or(CString::new("").unwrap());

            let display_name =
                VBuffer(std::ptr::addr_of!(sigil_list.display_name) as *const usize).raw();

            let weapon_info = protocol::WeaponInfo {
                weapon_id: weapon_info.weapon_id,
                star_level: weapon_info.star_level,
                plus_marks: weapon_info.plus_marks,
                awakening_level: weapon_info.awakening_level,
                trait_1_id: weapon_info.trait_1_id,
                trait_1_level: weapon_info.trait_1_level,
                trait_2_id: weapon_info.trait_2_id,
                trait_2_level: weapon_info.trait_2_level,
                trait_3_id: weapon_info.trait_3_id,
                trait_3_level: weapon_info.trait_3_level,
                wrightstone_id: weapon_info.wrightstone_id,
                weapon_level: weapon_info.weapon_level,
                weapon_hp: weapon_info.weapon_hp,
                weapon_attack: weapon_info.weapon_attack,
            };

            let overmastery_info = protocol::OvermasteryInfo {
                overmasteries: overmastery_info
                    .stats
                    .iter()
                    .map(|overmastery| protocol::Overmastery {
                        id: overmastery.id,
                        flags: overmastery.flags,
                        value: overmastery.value,
                    })
                    .collect(),
            };

            let payload = Message::PlayerLoadEvent(protocol::PlayerLoadEvent {
                sigils,
                character_name,
                display_name,
                actor_index: player_idx,
                is_online: sigil_list.is_online != 0,
                party_index: sigil_list.party_index as u8,
                player_stats: protocol::PlayerStats {
                    level: player_stats.level,
                    total_hp: player_stats.total_health,
                    total_attack: player_stats.total_attack,
                    stun_power: player_stats.stun_power,
                    critical_rate: player_stats.critical_rate,
                    total_power: player_stats.total_power,
                },
                character_type,
                weapon_info,
                overmastery_info,
            });

            #[cfg(feature = "console")]
            println!("sending player load event: {:?}", payload);

            let _ = self.tx.send(payload);
        }

        ret
    }
}

// ---------------------------------------------------------------------------
// Game 2.0.2 identity path
//
// The full OnLoadPlayer hook above depends on equipment offsets (sigil/weapon/
// overmastery) that shifted in the 2.0 update and are not yet re-derived, so it
// no longer fires. The identity path below recovers the piece the meter actually
// needs to tell players apart — display name + party slot — from the identity
// snapshot alone, which DID survive the patch.
//
// Two moving parts:
//   1. RefreshPlayerIdentity hook — fires when the game rebuilds a player's
//      identity snapshot. We read the stable name/party fields and cache them
//      keyed by the record's player-key.
//   2. identity_event_for_actor — called from the damage hook with the concrete
//      combat actor. It resolves that actor to a cached identity via the actor's
//      own player-key, and emits a PlayerIdentityEvent.
// ---------------------------------------------------------------------------

/// Offset of the identity snapshot pointer inside the player *record* passed to
/// RefreshPlayerIdentity. VERIFIED on v2.0.2: the hooked function reads
/// `[record + 0x5E60]` as the snapshot base (sigscan + Ghidra decompile of
/// FUN_140a2b600). The snapshot's inner field layout matches [`SigilList`]
/// (is_online/character_name/display_name/party_index at 0x1C8/0x1E8/0x208/0x22C).
const PLAYER_IDENTITY_OFFSET: usize = 0x5E60;

/// Offset of the owning player's key inside the player *record*.
const PLAYER_KEY_OFFSET: usize = 0x5EA8;

/// Offset of the owning player's key inside a concrete combat *actor* (the source
/// instance the damage hook sees). UNVERIFIED on this exe — carried over from the
/// same fork. Read via ReadProcessMemory so a bad range fails the read instead of
/// faulting the game thread.
const ACTOR_PLAYER_KEY_OFFSET: usize = 0x1AB40;

/// Sentinel that the game uses for an unset player key. This is the same
/// `0x887AE0B0` player-data type-hash that anchors the player_data offset scan;
/// it appears where a real key has not been assigned, so treat it as "no key".
const INVALID_PLAYER_KEY: u32 = 0x887A_E0B0;

/// Offset of the player record EMBEDDED IN THE ACTOR instance (v2.0.2).
///
/// Derived 2026-07-17 (online-collapse investigation): the only code in the exe
/// that reads `[actor+0x1AB40]` (FUN_1409d1770 @0x9d59ba) also calls
/// `FUN_140a2eae0(actor + 0x2a06*8)` = `actor+0x15030` — a record-shaped struct
/// inline in the actor. Live-confirmed from the 2026-07-17 online-lobby logs:
/// every mode-4 record address the identity hook saw for lobby players is
/// exactly `actor_base + 0x15030` for a damage-actor instance seen in the same
/// session (4/4 actors, two sessions). This is the per-player identity source
/// that the id at +0x1AB40 (character-scoped, shared by same-character players)
/// can never provide.
const ACTOR_RECORD_OFFSET: usize = 0x15030;

/// Record validity flag: the game's own id accessor (FUN_140378f10) treats the
/// record id at +0x5EA8 as unset when the byte at +0x5EBC is 0.
#[allow(dead_code)] // hookdiag ARDIAG probe only
const RECORD_VALID_OFFSET: usize = 0x5EBC;

/// The actor flags word beside the player key. Bit 0x40 gates the game's own
/// record-id comparison (FUN_1409d1770: skips the id path when set).
#[allow(dead_code)] // hookdiag ARDIAG probe only
const ACTOR_FLAGS_OFFSET: usize = 0x1AB64;

/// Prologue of the function that rebuilds the player identity snapshot
/// (FUN_140a2b600). VERIFIED unique (1 match) on v2.0.2; clean entry, 1-arg
/// `fn(rcx = player record)`. Hooking the refresh gives us names as soon as a
/// player's identity is (re)built, before the first damage event.
const REFRESH_PLAYER_IDENTITY_SIG: &str =
    "55 41 57 41 56 41 54 56 57 53 48 83 ec 70 48 8d 6c 24 70 48 c7 45 f8 fe ff ff ff 80 b9 bc 5e 00 00 00";

/// Number of sigil entries at the head of the v2.0.2 identity snapshot. The
/// snapshot IS a sigil list: `FUN_140a2b600` fills 13 entries (slots 0..=0xC,
/// 0x24 bytes each, dest 0x000..0x1D4) via the per-slot resolver `FUN_140a2c610`
/// before writing the name/party fields the identity path already reads.
const SNAPSHOT_SIGIL_COUNT: usize = 13;

/// The game's "empty" sentinel hash, used for unequipped sigil slots (same
/// constant as [`INVALID_PLAYER_KEY`], it's the generic invalid-hash marker).
const EMPTY_SIGIL_HASH: u32 = 0x887A_E0B0;

/// Offset of the 4 equipped-summon entries inline in the player record.
/// LIVE-VERIFIED on v2.0.2 (2026-07-17): 4 entries × 0x1C bytes, filled by the
/// record-update dispatcher (FUN_140a23e70) from the account summon store
/// `DAT_147c23f48` (equipped instance ids @ +0x280..0x28C, owned 1000×0x1C
/// SummonStoneData array @ +0xC40). Entry layout: {u32 summon_kind (summon.tbl
/// key), u32 instance_idx, u32 main_trait_id, u32 equip_bonus_id
/// (summon_base_param key), u32 trait_level (max 15), u32 bonus_level
/// (0-indexed, max 9), u32 unk}. Identical across all local-party records
/// (account-level; the bonuses apply party-wide).
const RECORD_SUMMON_OFFSET: usize = 0x5DD8;
const RECORD_SUMMON_COUNT: usize = 4;
const RECORD_SUMMON_STRIDE: usize = 0x1C;

/// Offset of the per-character loadout struct pointer inside the player record.
/// `*(record+0x5DC8)` is `sys::data::CharaData`; it is populated only on town
/// roster records (mode 4/5) and NULL in-quest (mode 0). Overmasteries and level
/// live here (see below); they are recovered opportunistically whenever the
/// identity hook sees a town record and persisted parser-side.
const RECORD_LOADOUT_OFFSET: usize = 0x5DC8;

/// 4 equipped overmasteries in the loadout: `{u32 id, u32 level_bits}` × 4 at
/// +0x3208. `level_bits` is a single-bit flag (bit N → level N+1, max bit 9 =
/// level 10). LODIAG-verified populated and distinct per character (2026-07-17).
const LOADOUT_OVERMASTERY_OFFSET: usize = 0x3208;
const LOADOUT_OVERMASTERY_COUNT: usize = 4;
const LOADOUT_OVERMASTERY_STRIDE: usize = 0x8;

/// Character level in the loadout. LODIAG-verified = 100 at +0x3530 (2026-07-17).
const LOADOUT_LEVEL_OFFSET: usize = 0x3530;

/// v2.0.2 record-inline equipment fields (live-verified 2026-07-17, see
/// docs/handoff-2026-07-17b-expansion-equipment.md):
/// - `+0x58B8`: 4 × 0x10 overmastery entries `{u32 id, u32 level_bits,
///   u32 effect_idx, f32 value}` — populate IN-QUEST for all party slots
///   (user-verified against the equip screen: +12%/+12%/+10%/+20%, and an
///   HP+800 build reading 800.0/16.0/16.0/16.0 at the +0xC floats). Unlike the
///   town loadout pairs these carry the computed magnitude, so they are the
///   primary overmastery source.
/// - `+0x5AF4..0x5B04`: the 4 equipped ability ids (`AB_PL####_##` hashes).
/// - `+0x5B10`: charid (`PL####` hash) — the key for every save-side map walk.
/// - `+0x5B44`: character level input (100 live) — the in-quest level source
///   (the loadout is town-only).
/// - `+0x5B60`: master level as the game stores it, level+stars combined
///   (55 = 50 + 5 stars local; AI-companion records read 0).
const RECORD_OVERMASTERY_OFFSET: usize = 0x58B8;
const RECORD_OVERMASTERY_COUNT: usize = 4;
const RECORD_OVERMASTERY_STRIDE: usize = 0x10;
const RECORD_ABILITY_OFFSET: usize = 0x5AF4;
const RECORD_ABILITY_COUNT: usize = 4;
const RECORD_CHARID_OFFSET: usize = 0x5B10;
/// Record offset (u32): wire-replicated level. Level-only fallback for when the
/// head-block level reads unpopulated.
const RECORD_LEVEL_WIRE_OFFSET: usize = 0x5B44;
const RECORD_MASTER_LEVEL_OFFSET: usize = 0x5B60;

/// RVA of the save-root singleton `DAT_147c24980` (global holds a pointer).
/// Its equipped-state map (std::unordered_map layout: end node @ +0x40,
/// bucket array @ +0x50, u32 mask @ +0x68; node key @ +0x10, value @ +0x18)
/// is keyed by charid; the value entry leads with the equipped weapon id as
/// 0x10 bytes of inline ASCII "PPPP_WW_UU" (→ weapon.tbl key
/// `WEP_PLPPPP_WW_UU`). Live-verified 2026-07-17 (9 roster characters).
/// v2.0.3: re-derived 2026-07-31 from the `mov r14,[rip+..]` in the weapon-state
/// fill `FUN_140a2d8d0` (now at rva 0xa27000-ish), which loads the save root and
/// the weapon table back to back.
const SAVE_ROOT_RVA: usize = 0x7c22e40; // 2.0.4: 0x7c22bc0
const EQ_MAP_END: usize = 0x40;
const EQ_MAP_BUCKETS: usize = 0x50;
const EQ_MAP_MASK: usize = 0x68;
const EQ_WEAPON_ASCII_LEN: usize = 0x10;

/// Record offset (u32): character level, head of the relocated pre-2.0
/// `PlayerStats` blob at rec+0x00..0x18. The game's stat-compute pass writes
/// this block just before identity-refresh, so it holds computed totals — the
/// mirror at +0x5B44 is wire-replicated and town-filled, and reading it was why
/// the UI showed base character stats.
///
/// The mirror was decompiled 2026-07-17 from the dispatcher `FUN_140a23e70`
/// case 3 (fills `+0x5B48/+0x5B4C/+0x5B54(float)/+0x5B58` from the same source
/// struct as the ability ids) and the town loadout-apply `FUN_1407a1080`
/// (maps loadout+0x3530..0x3550 onto `+0x5B44..0x5B5C`); those loadout-side
/// values are level-interpolated stat-curve columns (`FUN_143c4f260`), i.e.
/// base stats, which is exactly what made the UI wrong.
const RECORD_STATS_OFFSET: usize = 0x00;

/// The equipped weapon's state, live-labeled 2026-07-17 (user's Hraesvelgr:
/// weapon id ded16fcf = WEP_PL2700_06_03, wrightstone Stun Power 20 / ATK 15 /
/// Provoke 10, innate Catastrophe Nova / Glass Cannon / DMG Cap / Sigil
/// Booster — all matched in the WPDIAG blob dump). One 18-u32 struct layout
/// (see `protocol::WeaponState`), whose PRIMARY home is INLINE IN THE RECORD
/// at +0x50 with the active innate skill ids at +0x94 (5 sentinel-terminated
/// slots) — established by the `FUN_140a2d8d0` decompile (2026-07-17 #2):
/// modes 0-2 (`*(record+0x5EAC)`, in-quest) copy the weapon-instance table
/// column keyed by `*(*(record+0x5DD0)+0x44)` via `FUN_140321e30`; town modes
/// 4/5 copy from loadout+0x3228. `*(record+0x5E80)+0x50` (blob) is only the
/// NETWORK mirror of this block (NULL in solo), same struct at +0x50 /
/// innates +0x94.
///
/// NOT a weapon source (live-disproven by the 2026-07-17 WSDIAG2 run): the
/// charid map @ save-root+0x129B08 — its 15 × 0x190 rows are the LOADOUT
/// PRESET store {charid @ +0x60, 13 sigil instance ids @ +0x70, 4 ability
/// ids @ +0xA4, weapon-instance table key @ +0xB4, 5-slot innate override
/// @ +0xC0, 50 skillboard node keys @ +0xD4}.
const RECORD_WEAPON_STATE_OFFSET: usize = 0x50;
const RECORD_INNATE_OFFSET: usize = 0x94;
const RECORD_WEAPON_BLOB_OFFSET: usize = 0x5E80;
const BLOB_WEAPON_STATE_OFFSET: usize = 0x50;
const BLOB_INNATE_OFFSET: usize = 0x94;
const WEAPON_INNATE_SLOTS: usize = 5;
/// The innate-LEVEL pair array: 5 {skill id, level} pairs 0x60 past the id
/// slots (record +0xF4 — one word AFTER the zero word at +0xF0; the first
/// 07-18 read miscounted and shipped 0x5C, which put every read one word
/// early and level-0'd everything). Live-confirmed 2026-07-18 via the
/// in-quest WSDIAG3 dumps — the +0x94 ids repeat there with the user's known
/// levels 32/22/12/1. Levels are only trusted for a pair whose id matches the
/// slot id, so layout drift degrades to the old level-0 behavior instead of
/// attaching garbage.
const INNATE_LEVEL_PAIRS_REL: usize = 0x60;

/// Weapon-state fill mode (`FUN_140a2d8d0` switch) and the mode-0..2 source:
/// a pointer at record+0x5DD0 whose +0x44 is the weapon-instance table key.
#[allow(dead_code)] // hookdiag probes only
const RECORD_MODE_OFFSET: usize = 0x5EAC;
#[allow(dead_code)] // hookdiag probes only
const RECORD_WEAPON_SOURCE_OFFSET: usize = 0x5DD0;
#[allow(dead_code)] // hookdiag probes only
const WEAPON_SOURCE_KEY_OFFSET: usize = 0x44;

/// RVA of the live weapon-instance table singleton `DAT_147c24af8` (the
/// owned-weapon store; earlier misread first as a wrightstone table, then as
/// a "weapon-skill" table). 32 rows × 0x680 from +0x370; each row is
/// 8 columns × 0xD0. A COLUMN IS one weapon instance: its small instance key
/// at +0x00, then the 18-u32 weapon struct (so +0x04 = weapon.tbl Key hash),
/// validity count at +0x40, the 5 active innate skill-id slots at +0x44.
/// `FUN_140321e30` copies a column verbatim into record+0x50 / blob+0x50.
const WEAPON_TABLE_RVA: usize = 0x7c22fb8; // 2.0.4: 0x7c22d38
const WS_TABLE_BASE: usize = 0x370;
const WS_TABLE_ROWS: usize = 32;
const WS_TABLE_ROW_STRIDE: usize = 0x680;
const WS_TABLE_COLS: usize = 8;
const WS_TABLE_COL_STRIDE: usize = 0xD0;
const WS_TABLE_VALID_OFFSET: usize = 0x40;
const WS_TABLE_DATA_OFFSET: usize = 0x44;
const WS_TABLE_SKILL_SLOTS: usize = 5;

/// RVA of the CharaPower progression singleton `DAT_147c24a78` and the two
/// maps the game's own skillboard query (`FUN_140297bc0`, decompiled
/// 2026-07-17) walks:
/// - charid → vector of node instance keys: end @ +0x728, buckets @ +0x738,
///   mask @ +0x750; the value node holds the vector begin/end pointers at
///   +0x18/+0x20 (entries are 8 bytes, key in the low u32).
/// - node instance key → row: end @ +0x320, buckets @ +0x330, mask @ +0x348;
///   row fields: node id @ +0x48, unlock bit index @ +0x5C, effect/ui id
///   @ +0x74 (the id the network profile blob carries).
/// A node is unlocked when the record's inline 400 × 0x38 `{u32 id, u32 bits}`
/// array at `record+0x138` has an entry with that node id and bit
/// `row+0x5C` set — that array is the SOLO skillboard state.
/// v2.0.3: re-derived 2026-07-31 from two independent sites that agree — the
/// skillboard query's own `mov rbx,[rip+..]` prologue load and the record
/// dispatcher's `mov rax,[rip+..]`. This is the global whose staleness silently
/// emptied master traits after the patch.
const CHARA_POWER_RVA: usize = 0x7c22f38; // 2.0.4: 0x7c22cb8
const SB_CHAR_MAP_END: usize = 0x728;
const SB_CHAR_MAP_BUCKETS: usize = 0x738;
const SB_CHAR_MAP_MASK: usize = 0x750;
const SB_NODE_MAP_END: usize = 0x320;
const SB_NODE_MAP_BUCKETS: usize = 0x330;
const SB_NODE_MAP_MASK: usize = 0x348;
const RECORD_NODE_ARRAY_OFFSET: usize = 0x138;
const RECORD_NODE_ARRAY_COUNT: usize = 400;
const RECORD_NODE_ARRAY_STRIDE: usize = 0x38;

/// Cached identity fields for one player, resolved from a snapshot.
#[derive(Clone, Debug)]
struct StoredPlayerIdentity {
    character_name: CString,
    display_name: CString,
    party_index: u8,
    is_online: bool,
    sigils: Vec<protocol::Sigil>,
    summons: Vec<protocol::EquippedSummon>,
    overmasteries: Vec<protocol::Overmastery>,
    player_level: u32,
    abilities: Vec<u32>,
    weapon_key: String,
    master_level: u32,
    skillboard: Vec<u32>,
    stats: Option<protocol::RecordStats>,
    weapon_state: Option<protocol::WeaponState>,
    /// The record's three damage-cap-up fields, already in builder units. See
    /// [`read_record_cap_ups`].
    cap_ups: (Option<f32>, Option<f32>, Option<f32>),
    /// The Mastery (AP-tree) slice of those totals, in table units. See
    /// [`read_record_limit_bonus_caps`].
    limit_bonus_caps: (Option<f32>, Option<f32>, Option<f32>),
}

/// One player's cached identity, paired with the id its record most recently
/// carried at [`PLAYER_KEY_OFFSET`] (kept only for the legacy [`find_by_id`]
/// fallback — the equipment join no longer uses it).
///
/// [`find_by_id`]: IdentityStore::find_by_id
struct SlotIdentity {
    id: u32,
    identity: StoredPlayerIdentity,
}

/// Player identities keyed by `(party slot, online)`.
///
/// The equipment join must satisfy two constraints the record id at +0x5EA8
/// cannot meet on its own:
///
/// - **Two players on the SAME character share that id** — it is character-scoped
///   (2026-07-18 live: both Maglielles carried id `0x25d46f4b`). Keying equipment
///   by id therefore returned one player's whole build for both (duplicated
///   weapon/summons/overmasteries/master-traits; only sigils, read per-actor from
///   the embedded snapshot, stayed distinct). Two players on one character always
///   occupy DISTINCT party slots, so the slot separates them.
/// - **A local AI town record and a remote player can share a slot** — the local
///   town-roster records (AI companions) keep refreshing slots 1..=3 during online
///   play, so a slot-ONLY join dressed remote players in a local companion's build
///   (2026-07-18: a remote Tweyen shipped with the local AI Maglielle's build).
///   The AI record is offline and the remote player online, so the online flag
///   separates them.
///
/// `(party slot, online)` uniquely identifies a logical player at any instant (a
/// slot holds one player; the online flag splits the AI/remote overlap), so each
/// claim owns its own bucket and never evicts a sibling.
#[derive(Default)]
struct IdentityStore {
    entries: Vec<SlotIdentity>,
}

impl IdentityStore {
    /// Records the identity for its `(party slot, online)` bucket, replacing any
    /// previous claim for that same bucket (latest wins). Unlike the old id-keyed
    /// store this never evicts another bucket — two same-character players hold
    /// the same `id` in different slots and must coexist.
    fn claim(&mut self, id: u32, identity: StoredPlayerIdentity) {
        let slot = identity.party_index.min(3);
        let online = identity.is_online;

        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|e| e.identity.party_index.min(3) == slot && e.identity.is_online == online)
        {
            #[cfg(feature = "hookdiag")]
            if existing.identity.display_name != identity.display_name || existing.id != id {
                log::info!(
                    "IDDIAG slot {slot} online={online} update {:?}/{:#010x} -> {:?}/{id:#010x}",
                    existing.identity.display_name,
                    existing.id,
                    identity.display_name,
                );
            }
            existing.id = id;
            existing.identity = identity;
        } else {
            self.entries.push(SlotIdentity { id, identity });
        }
    }

    /// Finds the entry whose record most recently carried `id`. Used only by the
    /// legacy fallback for an actor with no embedded record (the primary path
    /// joins on `(party slot, online)` via [`equipment_donor`]); it inherits the
    /// same character-scoped-id limitation, but that path can supply no slot.
    ///
    /// [`equipment_donor`]: IdentityStore::equipment_donor
    fn find_by_id(&self, id: u32) -> Option<&StoredPlayerIdentity> {
        self.entries
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| &entry.identity)
    }

    /// The equipment-donor claim for a damage actor, joined on its own embedded
    /// record's `(party slot, online)` — the key that distinguishes both two
    /// same-character players and a remote player from a local AI on one slot (see
    /// [`IdentityStore`]). `None` when no claim matches; the parser then keeps any
    /// equipment a matching refresh taught it earlier.
    fn equipment_donor(&self, party_index: u8, is_online: bool) -> Option<&StoredPlayerIdentity> {
        let slot = party_index.min(3);
        self.entries
            .iter()
            .find(|e| e.identity.party_index.min(3) == slot && e.identity.is_online == is_online)
            .map(|e| &e.identity)
    }
}

static IDENTITIES: OnceLock<Mutex<IdentityStore>> = OnceLock::new();

fn identities() -> &'static Mutex<IdentityStore> {
    IDENTITIES.get_or_init(|| Mutex::new(IdentityStore::default()))
}

#[derive(Clone)]
pub struct OnLoadPlayerIdentityHook {
    // Retained for symmetry with the other hooks and future use; the identity
    // path publishes through identity_event_for_actor at damage time, not here.
    #[allow(dead_code)]
    tx: event::Tx,
}

impl OnLoadPlayerIdentityHook {
    pub fn new(tx: event::Tx) -> Self {
        Self { tx }
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        let refresh_player_identity = process
            .search_match_address(REFRESH_PLAYER_IDENTITY_SIG)
            .map_err(|_| anyhow!("Could not find refresh_player_identity"))?;

        #[cfg(feature = "console")]
        println!("Found refresh player identity");

        let cloned_self = self.clone();

        unsafe {
            let func: RefreshPlayerIdentityFunc = std::mem::transmute(refresh_player_identity);
            RefreshPlayerIdentity.initialize(func, move |record| cloned_self.run(record))?;
            RefreshPlayerIdentity.enable()?;
        }

        // Diagnostic-only detour on the loadout->profile serializer; failure to
        // find it must not break the identity hook.
        #[cfg(feature = "hookdiag")]
        match process.search_match_address(BUILD_PLAYER_PROFILE_SIG) {
            Ok(build_profile) => {
                type BuildPlayerProfileFunc = unsafe extern "system" fn(*mut u8, *mut u8);
                let detour = |profile: *mut u8, loadout: *mut u8| {
                    log_profile_build(profile, loadout);
                    unsafe { BuildPlayerProfile.call(profile, loadout) }
                };
                unsafe {
                    let func: BuildPlayerProfileFunc = std::mem::transmute(build_profile);
                    BuildPlayerProfile.initialize(func, detour)?;
                    BuildPlayerProfile.enable()?;
                }
                log::info!("PRDIAG serializer hook installed at {build_profile:#x}");
            }
            Err(_) => log::warn!("PRDIAG serializer sig not found — profile-build diag disabled"),
        }

        Ok(())
    }

    fn run(&self, record: *const usize) {
        unsafe { RefreshPlayerIdentity.call(record) };

        if record.is_null() {
            return;
        }

        let snapshot = unsafe {
            (record.byte_add(PLAYER_IDENTITY_OFFSET) as *const *const u8).read_unaligned()
        };
        let player_key = unsafe {
            record
                .byte_add(PLAYER_KEY_OFFSET)
                .cast::<u32>()
                .read_unaligned()
        };

        // v2.0.2 identity diagnosis: log every record refresh with its mode enum and id
        // neighborhood. Ghidra shows record+0x5EAC is a small mode enum (0..=5) and
        // +0x5EA8 an id the game itself re-resolves live — the mode is the missing piece
        // to know WHICH records are authoritative. Guarded reads, hookdiag builds only.
        #[cfg(feature = "hookdiag")]
        log_identity_record(record, snapshot, player_key);

        #[cfg(feature = "hookdiag")]
        log_loadout_probe(record);

        #[cfg(feature = "hookdiag")]
        log_progress_probe(record);

        #[cfg(feature = "hookdiag")]
        log_skillboard_probe(record);

        #[cfg(feature = "hookdiag")]
        log_weapon_probe(record);

        if player_key == 0 || player_key == INVALID_PLAYER_KEY {
            return;
        }

        let Some(mut identity) = (unsafe { read_player_identity(snapshot) }) else {
            return;
        };

        // The equipped summons live inline in the record (not the snapshot).
        // Account-level and party-wide, so they are NOT blanked for AI
        // placeholder records below — the same set genuinely applies to every
        // local party member, and a remote player claiming the slot refreshes
        // the identity with their own record's set anyway.
        identity.summons = read_record_summons(record);

        // Overmasteries: primary source is the record's inline block (+0x58B8),
        // which populates in-quest for every party slot and carries the computed
        // f32 magnitude; the town loadout pairs (id+level only) stand in when the
        // block is still sentinel-empty. Level: town loadout first (it's the
        // authoritative character sheet), record level input (+0x5B44) in-quest.
        // Per-character in both paths, so like sigils they stand for AI
        // companions too; the parser keeps the last non-empty set.
        let record_overmasteries = read_record_overmasteries(record);
        let (loadout_overmasteries, loadout_level) = read_loadout_overmasteries_and_level(record);
        identity.overmasteries = if record_overmasteries.is_empty() {
            loadout_overmasteries
        } else {
            record_overmasteries
        };
        identity.player_level = if loadout_level != 0 {
            loadout_level
        } else {
            sanity_u32(
                crate::hooks::diag::read_u32_guarded(record as usize, RECORD_LEVEL_WIRE_OFFSET),
                150,
            )
        };
        identity.abilities = read_record_abilities(record);
        identity.master_level = sanity_u32(
            crate::hooks::diag::read_u32_guarded(record as usize, RECORD_MASTER_LEVEL_OFFSET),
            200,
        );
        identity.weapon_key = read_equipped_weapon_key(record);
        identity.skillboard = read_record_skillboard(record);
        identity.stats = read_record_stats(record);
        identity.weapon_state = read_weapon_state(record);
        identity.cap_ups = read_record_cap_ups(record as usize);
        identity.limit_bonus_caps = read_record_limit_bonus_caps(record as usize);

        // One line per claim summarizing what the NEW production readers
        // resolved, so a single live run verifies overmasteries/abilities/
        // weapon/master-level/skillboard against the in-game equip screens.
        #[cfg(feature = "hookdiag")]
        log::info!(
            // capup is printed in the SAME units and order as the oracle's
            // `om=[...,n:,s:,b:]`, so the live gate is one diff of two log lines
            // rather than a walk through a stored log.
            "EQDIAG key={player_key:#010x} party={} om={} ab={:x?} weapon={:?} mlvl={} sb={} lvl={} capup=[n:{:?},s:{:?},b:{:?}] lbcap=[n:{:?},s:{:?},b:{:?}]",
            identity.party_index,
            identity
                .overmasteries
                .iter()
                .map(|o| format!("{:08x}/{:x}/{}", o.id, o.flags, o.value))
                .collect::<Vec<_>>()
                .join(","),
            identity.abilities,
            identity.weapon_key,
            identity.master_level,
            identity.skillboard.len(),
            identity.player_level,
            identity.cap_ups.0,
            identity.cap_ups.1,
            identity.cap_ups.2,
            identity.limit_bonus_caps.0,
            identity.limit_bonus_caps.1,
            identity.limit_bonus_caps.2,
        );

        #[cfg(feature = "hookdiag")]
        log_record_node_array(record, &identity.skillboard);

        // Companion lines for the stat/weapon-state labeling run: STDIAG dumps
        // the whole candidate window around the record stat block (0x5B38..0x5B88)
        // so each field can be matched against the in-game status screen, and
        // WSDIAG dumps the matched weapon save row's raw blocks.
        #[cfg(feature = "hookdiag")]
        {
            let base = record as usize;
            let window: Vec<String> = (0..20)
                .map(|i| {
                    let off = 0x5B38 + i * 4;
                    format!(
                        "{off:#06x}={:#010x}",
                        crate::hooks::diag::read_u32_guarded(base, off)
                    )
                })
                .collect();
            log::info!(
                "STDIAG key={player_key:#010x} party={} {}",
                identity.party_index,
                window.join(" ")
            );
            if let Some(ws) = &identity.weapon_state {
                log::info!(
                    "WSDIAG key={player_key:#010x} party={} wid={:#010x} exp={} star={} plus={} awk={} wstone={:#010x} wtraits={:?} innate={:?}",
                    identity.party_index,
                    ws.weapon_id,
                    ws.exp,
                    ws.star_level,
                    ws.plus_marks,
                    ws.awakening_level,
                    ws.wrightstone_id,
                    ws.wrightstone_traits
                        .iter()
                        .map(|t| format!("{:08x}/{}", t.id, t.level))
                        .collect::<Vec<_>>(),
                    ws.innate_traits
                        .iter()
                        .map(|t| format!("{:08x}/{}", t.id, t.level))
                        .collect::<Vec<_>>(),
                );
            } else {
                log::info!(
                    "WSDIAG key={player_key:#010x} party={} weapon state UNRESOLVED (blob null / rows ambiguous or absent)",
                    identity.party_index,
                );
            }
            log_weapon_state_probe(record, player_key);
        }

        // Cache AI companions with their names blanked (see
        // [`StoredPlayerIdentity::blank_names_if_ai`]). A real remote player joining
        // the slot re-claims it with their real name on the next identity refresh
        // (latest claim wins).
        identity.blank_names_if_ai();

        #[cfg(feature = "console")]
        println!(
            "player identity cached: key={player_key:#010x} party={} online={} sigils={} name={}",
            identity.party_index,
            identity.is_online,
            identity.sigils.len(),
            identity.display_name.to_string_lossy()
        );

        // hookdiag: dump the resolved sigil entries once per claim so the snapshot
        // sigil layout can be live-verified against the in-game loadout screen.
        #[cfg(feature = "hookdiag")]
        for (i, sigil) in identity.sigils.iter().enumerate() {
            log::info!(
                "IDDIAG sigil[{i}] key={player_key:#010x} id={:#010x} lvl={} t1={:#010x}/{} t2={:#010x}/{}",
                sigil.sigil_id,
                sigil.sigil_level,
                sigil.first_trait_id,
                sigil.first_trait_level,
                sigil.second_trait_id,
                sigil.second_trait_level
            );
        }

        identities()
            .lock()
            .expect("identity map lock poisoned")
            .claim(player_key, identity);
    }
}

impl StoredPlayerIdentity {
    /// Slot 0 is always the local player. Any other slot that is not flagged
    /// online is an AI companion (or a not-yet-populated lobby placeholder)
    /// whose snapshot name is the local profile's, not its own.
    fn is_ai_placeholder(&self) -> bool {
        self.party_index != 0 && !self.is_online
    }

    /// Blanks the names an AI companion's snapshot cannot own.
    ///
    /// Their snapshot carries the LOCAL profile's name (confirmed live: all four
    /// records announce the local name with distinct keys), so leaving it in
    /// place labels every companion with the local player's name. Blanking lets
    /// AI damage still resolve to its party slot (the meter shows
    /// "[N] <character>", not "[Guest] <character>") while the empty name is what
    /// the frontend keys its "(AI)" marker off.
    ///
    /// **Every path that emits an identity must apply this** — both the
    /// `RefreshPlayerIdentity` claim and the embedded-record path in
    /// [`embedded_identity_event`] read the name from a snapshot that carries
    /// the local profile's name. Missing it on the second is what made the meter
    /// label all three companions with the local player's name.
    ///
    /// Sigils are deliberately NOT blanked: unlike the name, each party slot's
    /// record resolves its sigils from its own per-character loadout, so an AI
    /// companion's snapshot sigils are genuinely that companion's build.
    /// Confirmed from the town roster: the four party records carry four DISTINCT
    /// loadout charids with distinct sigil/overmastery/skillboard data (LODIAG).
    fn blank_names_if_ai(&mut self) {
        if self.is_ai_placeholder() {
            self.display_name = CString::new("").expect("empty CString is valid");
            self.character_name = CString::new("").expect("empty CString is valid");
        }
    }
}

/// Resolves the concrete combat actor (as seen by the damage hook) to a cached
/// identity, emitting a [`PlayerIdentityEvent`] if one is known.
///
/// The actor's id is read FRESH on every call and matched against the latest id
/// each party slot's record announced — deliberately no per-actor caching, because
/// v2.0.2 ids are recycled between players/contexts and a cached pairing goes
/// stale (that was the wrong/flip-flopping-names bug). One guarded 4-byte read per
/// damage event is negligible.
///
/// Returns `None` when the actor has no resolvable player-key or no slot currently
/// claims it (e.g. an NPC/enemy, or a player whose snapshot has not refreshed).
/// Safe to call for every damage source.
pub fn identity_event_for_actor(
    actor: *const usize,
    character_type: u32,
    actor_index: u32,
) -> Option<PlayerIdentityEvent> {
    if actor.is_null() {
        return None;
    }

    #[cfg(feature = "hookdiag")]
    let actor_address = actor as usize;

    // Online-collapse probe: for each distinct damage actor, log its
    // character-scoped ids next to its own embedded record's identity.
    #[cfg(feature = "hookdiag")]
    log_actor_record_probe(actor_address, character_type, actor_index);

    // v2.0.2 collapse fix — PRIMARY PATH: the actor's own embedded record
    // (`actor+0x15030`) carries per-PLAYER identity (party slot, online flag,
    // display name) that the character-scoped id at +0x1AB40 cannot provide:
    // two players on the same character share that id, which merged their
    // meter rows. Live-proven online + solo (2026-07-18). The event is keyed
    // by the synthetic slot key so damage/SBA/stun attribution joins on a
    // player-unique index.
    if let Some(identity) = embedded_identity_struct(actor as usize) {
        let party_index = identity.party_index.min(3);
        // The embedded snapshot carries identity + sigils; the RefreshPlayerIdentity
        // hook reads the fuller equipment payload (weapon, overmasteries, stats...)
        // from the same records — enrich from its cache, joined on the actor's own
        // (party slot, online). The record id at +0x5EA8 can't be the join key: it
        // is character-scoped, so two players on one character share it and both
        // resolved to a single donor (duplicated builds). Slot separates
        // same-character players; the online flag keeps a local AI town record on
        // the same slot from donating its build to a remote player.
        let cached = identities().lock().ok().and_then(|store| {
            store
                .equipment_donor(party_index, identity.is_online)
                .cloned()
        });
        let equip = cached.unwrap_or_else(|| identity.clone());
        return Some(embedded_identity_event(identity, equip, character_type));
    }

    let Some(player_key) = read_actor_player_key(actor) else {
        // Bounded: log the first N actors whose key read comes back
        // empty/sentinel so "player shows as [Guest]" cases are visible.
        #[cfg(feature = "hookdiag")]
        {
            use std::sync::atomic::AtomicU32;
            static N: AtomicU32 = AtomicU32::new(0);
            if crate::hooks::diag::first_n(&N, 60) {
                let raw =
                    crate::hooks::diag::read_u32_guarded(actor_address, ACTOR_PLAYER_KEY_OFFSET);
                log::info!(
                    "IDDIAG actor={actor_address:#x} type={character_type:#010x} idx={actor_index} key read FAILED raw@1AB40={raw:#010x}"
                );
            }
        }
        return None;
    };

    // First sight of this actor: log the id pair around 0x1AB40 — Ghidra shows
    // [+0x1AB40]=id passed to the record manager and [+0x1AB44] compared against
    // the 0x887AE0B0 sentinel. Logging only; resolution never caches.
    #[cfg(feature = "hookdiag")]
    {
        use std::collections::HashSet;
        use std::sync::{Mutex as DiagMutex, OnceLock as DiagOnceLock};
        static SEEN: DiagOnceLock<DiagMutex<HashSet<usize>>> = DiagOnceLock::new();
        let seen = SEEN.get_or_init(|| DiagMutex::new(HashSet::new()));
        let mut seen = seen.lock().expect("iddiag seen lock poisoned");
        if seen.len() < 256 && seen.insert(actor_address) {
            let k44 =
                crate::hooks::diag::read_u32_guarded(actor_address, ACTOR_PLAYER_KEY_OFFSET + 4);
            let k64 =
                crate::hooks::diag::read_u32_guarded(actor_address, ACTOR_PLAYER_KEY_OFFSET + 0x24);
            log::info!(
                "IDDIAG actor={actor_address:#x} type={character_type:#010x} idx={actor_index} id@1AB40={player_key:#010x} @1AB44={k44:#010x} @1AB64={k64:#010x}"
            );
        }
    }

    let identity = identities()
        .lock()
        .expect("identity map lock poisoned")
        .find_by_id(player_key)
        .cloned()?;

    Some(PlayerIdentityEvent {
        character_name: identity.character_name,
        display_name: identity.display_name,
        character_type,
        party_index: identity.party_index,
        actor_index,
        is_online: identity.is_online,
        sigils: identity.sigils,
        summons: identity.summons,
        overmasteries: identity.overmasteries,
        player_level: identity.player_level,
        abilities: identity.abilities,
        weapon_key: identity.weapon_key,
        master_level: identity.master_level,
        skillboard: identity.skillboard,
        stats: identity.stats,
        weapon_state: identity.weapon_state,
        cap_up_normal: identity.cap_ups.0,
        cap_up_skill: identity.cap_ups.1,
        cap_up_sba: identity.cap_ups.2,
        limit_bonus_cap_normal: identity.limit_bonus_caps.0,
        limit_bonus_cap_skill: identity.limit_bonus_caps.1,
        limit_bonus_cap_sba: identity.limit_bonus_caps.2,
    })
}

/// Builds the outgoing event for an actor resolved through its own embedded
/// record: identity (name, slot, online) comes from the actor itself, the
/// equipment payload from whichever claim [`IdentityStore::equipment_donor`]
/// matched (falling back to the actor's own snapshot).
fn embedded_identity_event(
    mut identity: StoredPlayerIdentity,
    equip: StoredPlayerIdentity,
    character_type: u32,
) -> PlayerIdentityEvent {
    let party_index = identity.party_index.min(3);

    // The claim path blanks AI names for the reason spelled out at
    // [`is_ai_placeholder`]; this path reads the name from the actor's OWN
    // embedded record, which carries the same local-profile name, so it has to
    // apply the same rule. Without it every companion ships the local player's
    // name and the meter renders "<local name> (Zeta)" instead of "AI (Zeta)".
    identity.blank_names_if_ai();

    PlayerIdentityEvent {
        character_name: identity.character_name,
        display_name: identity.display_name,
        character_type,
        party_index,
        actor_index: slot_key(party_index),
        is_online: identity.is_online,
        sigils: if identity.sigils.is_empty() {
            equip.sigils
        } else {
            identity.sigils
        },
        summons: equip.summons,
        overmasteries: equip.overmasteries,
        player_level: equip.player_level,
        abilities: equip.abilities,
        weapon_key: equip.weapon_key,
        master_level: equip.master_level,
        skillboard: equip.skillboard,
        stats: equip.stats,
        weapon_state: equip.weapon_state,
        // From the equipment donor, like every other record-sourced field: the
        // cap-ups belong to whichever record the loadout came off, not to the
        // actor's own half-filled snapshot.
        cap_up_normal: equip.cap_ups.0,
        cap_up_skill: equip.cap_ups.1,
        cap_up_sba: equip.cap_ups.2,
        limit_bonus_cap_normal: equip.limit_bonus_caps.0,
        limit_bonus_cap_skill: equip.limit_bonus_caps.1,
        limit_bonus_cap_sba: equip.limit_bonus_caps.2,
    }
}

/// Clamp helper for record scalars that may hold the empty sentinel or garbage
/// on a half-initialized record: anything over `max` (or the sentinel) reads
/// as 0 ("unknown") rather than leaking a bogus number to the UI.
fn sanity_u32(value: u32, max: u32) -> u32 {
    if value == EMPTY_SIGIL_HASH || value > max {
        0
    } else {
        value
    }
}

/// The record's three damage-cap-up fields, in the builder's own units.
///
/// The builder reads these at `record+0x28/+0x30/+0x34` and scales by 0.01
/// (`FUN_1409c1cf0`), so the scaling is applied HERE and the parser stores what
/// the formula actually adds. A non-finite value reads `None`: it would
/// serialize to JSON null and poison every downstream sum.
///
/// The record reached here is the one the identity path already holds. Whether
/// it is the SAME object the builder reaches through `holder->vfn+0x9f0` is a
/// hypothesis, not a fact — `cap_oracle.rs` prints the builder's own values for
/// the same three fields, and the two agreeing on a live run is what settles it.
/// v2.0.4 offsets; a patch that moves them cannot be caught by any test here.
fn read_record_cap_ups(record: usize) -> (Option<f32>, Option<f32>, Option<f32>) {
    let read = |offset: usize| {
        crate::hooks::diag::read_f32_guarded(record, offset)
            .filter(|v| v.is_finite())
            .map(|v| v * 0.01)
    };
    (read(0x28), read(0x30), read(0x34))
}

/// Sums the record's resolved limit-bonus store (the 400 × 0x38 array at
/// [`RECORD_NODE_ARRAY_OFFSET`]) per damage-cap class — the Mastery/AP-tree
/// term, already resolved to the account's unlock state by the game.
///
/// Every cap-typed entry in this store is an `ap_tree_*` limit-bonus
/// (live-verified 2026-08-10: all 56 distinct cap entry ids across a full
/// party resolve to `ap_tree_*.LimitBonusId`, none to board nodes), so these
/// sums itemize exactly the "Mastery / Collection" slice of the fused record
/// cap-up — the one input `PlayerData` could not carry before.
///
/// Units are the TABLE's percent (684.0 = +684%), NOT [`read_record_cap_ups`]'s
/// builder units: the account rows in the breakdown render table units, and the
/// record channel already fuses this term, so nothing downstream ever adds the
/// two together.
///
/// Each entry holds up to three param slots of `{key, level mask, type, f32
/// value}`; type 106 raises every class and 107 is the healing cap, excluded
/// for the reason `gen-ap-tree-cap-sources.mjs` spells out. An EMPTY store
/// reads `None` — a record that has not loaded yet is "unknown", not an
/// account with nothing unlocked.
fn read_record_limit_bonus_caps(record: usize) -> (Option<f32>, Option<f32>, Option<f32>) {
    use crate::hooks::diag::{read_f32_guarded, read_u32_guarded};

    const PARAM_SLOTS: usize = 3;
    const SLOT_WORDS: usize = 4;
    const TYPE_NORMAL: u32 = 103;
    const TYPE_SKILL: u32 = 104;
    const TYPE_SBA: u32 = 105;
    const TYPE_ALL: u32 = 106;

    let mut used = false;
    let (mut normal, mut skill, mut sba) = (0f32, 0f32, 0f32);
    for n in 0..RECORD_NODE_ARRAY_COUNT {
        let entry = RECORD_NODE_ARRAY_OFFSET + n * RECORD_NODE_ARRAY_STRIDE;
        let id = read_u32_guarded(record, entry);
        if id == 0 || id == EMPTY_SIGIL_HASH {
            continue;
        }
        used = true;
        for slot in 0..PARAM_SLOTS {
            let at = entry + (2 + slot * SLOT_WORDS) * 4;
            let param = read_u32_guarded(record, at);
            if param == 0 || param == EMPTY_SIGIL_HASH {
                continue;
            }
            let Some(value) = read_f32_guarded(record, at + 12).filter(|v| v.is_finite()) else {
                continue;
            };
            match read_u32_guarded(record, at + 8) {
                TYPE_NORMAL => normal += value,
                TYPE_SKILL => skill += value,
                TYPE_SBA => sba += value,
                TYPE_ALL => {
                    normal += value;
                    skill += value;
                    sba += value;
                }
                _ => {}
            }
        }
    }
    if !used {
        return (None, None, None);
    }
    (Some(normal), Some(skill), Some(sba))
}

/// Reads the 4 equipped overmasteries inline in the player record (see
/// [`RECORD_OVERMASTERY_OFFSET`] for the live-verified layout). Empty slots
/// hold the [`EMPTY_SIGIL_HASH`] sentinel and are dropped. Guarded reads.
fn read_record_overmasteries(record: *const usize) -> Vec<protocol::Overmastery> {
    use crate::hooks::diag::{read_f32_guarded, read_u32_guarded};

    let base = record as usize;
    (0..RECORD_OVERMASTERY_COUNT)
        .filter_map(|i| {
            let entry = RECORD_OVERMASTERY_OFFSET + i * RECORD_OVERMASTERY_STRIDE;
            let id = read_u32_guarded(base, entry);
            if id == 0 || id == EMPTY_SIGIL_HASH {
                return None;
            }
            let value = read_f32_guarded(base, entry + 0xC).unwrap_or(0.0);
            Some(protocol::Overmastery {
                id,
                flags: read_u32_guarded(base, entry + 0x4),
                value: if value.is_finite() { value } else { 0.0 },
            })
        })
        .collect()
}

/// Reads the 4 equipped ability ids inline in the player record
/// ([`RECORD_ABILITY_OFFSET`]). Empty slots hold the [`EMPTY_SIGIL_HASH`]
/// sentinel and are dropped. Guarded reads.
fn read_record_abilities(record: *const usize) -> Vec<u32> {
    use crate::hooks::diag::read_u32_guarded;

    let base = record as usize;
    (0..RECORD_ABILITY_COUNT)
        .filter_map(|i| {
            let id = read_u32_guarded(base, RECORD_ABILITY_OFFSET + i * 4);
            (id != 0 && id != EMPTY_SIGIL_HASH).then_some(id)
        })
        .collect()
}

/// Looks up `key` in one of the game's `std::unordered_map`s (MSVC layout, as
/// walked by the game's own accessors: end node at `map+end_off`, bucket array
/// at `map+buckets_off`, u32 mask at `map+mask_off`; each bucket is 0x10 bytes
/// holding {first, last} node pointers; nodes carry the next-link at +0x8, the
/// u32 key at +0x10). Returns the matching NODE pointer (value layout is the
/// caller's business: a value pointer at +0x18, or inline vector begin/end at
/// +0x18/+0x20). Guarded reads throughout, bounded chain walk — never faults,
/// returns `None` on any inconsistency.
fn game_hashmap_find(
    map: usize,
    end_off: usize,
    buckets_off: usize,
    mask_off: usize,
    key: u32,
) -> Option<usize> {
    use crate::hooks::diag::{read_ptr_guarded, read_u32_guarded};

    let mask = read_u32_guarded(map, mask_off);
    let buckets = read_ptr_guarded(map, buckets_off)?;
    let end_node = read_ptr_guarded(map, end_off)?;
    if buckets == 0 {
        return None;
    }
    let bucket = (mask & key) as usize * 0x10;
    let bucket_head = read_ptr_guarded(buckets, bucket)?;
    let mut node = read_ptr_guarded(buckets, bucket + 8)?;
    for _ in 0..64 {
        if node == 0 || node == end_node {
            return None;
        }
        if read_u32_guarded(node, 0x10) == key {
            return Some(node);
        }
        if node == bucket_head {
            return None;
        }
        node = read_ptr_guarded(node, 8)?;
    }
    None
}

/// Resolves the record's equipped weapon to its `weapon.tbl` key name
/// (`WEP_PLPPPP_WW_UU`) by walking the save root's charid-keyed equipped-state
/// map (see [`SAVE_ROOT_RVA`]). The save stores the id as inline ASCII
/// ("PPPP_WW_UU", NUL-padded to 0x10; a short "PPPP_WW" variant exists), so
/// the full key is `"WEP_PL"` + that string. Returns an empty string when the
/// charid has no entry (e.g. remote players) or the text fails validation.
/// Guarded reads — never faults.
fn read_equipped_weapon_key(record: *const usize) -> String {
    use crate::hooks::diag::{read_bytes_guarded, read_ptr_guarded, read_u32_guarded, MODULE_BASE};
    use std::sync::atomic::Ordering;

    let module = MODULE_BASE.load(Ordering::Relaxed);
    if module == 0 {
        return String::new();
    }
    let Some(mgr) = read_ptr_guarded(module, SAVE_ROOT_RVA).filter(|m| *m != 0) else {
        return String::new();
    };
    let charid = read_u32_guarded(record as usize, RECORD_CHARID_OFFSET);
    if charid == 0 || charid == EMPTY_SIGIL_HASH {
        return String::new();
    }
    let Some(node) = game_hashmap_find(mgr, EQ_MAP_END, EQ_MAP_BUCKETS, EQ_MAP_MASK, charid) else {
        return String::new();
    };
    let Some(entry) = read_ptr_guarded(node, 0x18).filter(|v| *v > 0x10000) else {
        return String::new();
    };
    let Some(raw) = read_bytes_guarded(entry, 0, EQ_WEAPON_ASCII_LEN) else {
        return String::new();
    };
    let text: String = raw
        .iter()
        .take_while(|b| **b != 0)
        .map(|b| *b as char)
        .collect();
    // "2700_02_01" / "2900_01": leading PL number then _-separated digit groups.
    let valid = text.len() >= 4
        && text.chars().take(4).all(|c| c.is_ascii_digit())
        && text.chars().all(|c| c.is_ascii_digit() || c == '_');
    if !valid {
        return String::new();
    }
    format!("WEP_PL{text}")
}

/// Reads the record's authoritative stat block (see [`RECORD_STATS_OFFSET`]).
/// Returns `None` when every slot is zero or sentinel (record not yet
/// populated). Cross-referenced against an independent implementation
/// 2026-07-24.
fn read_record_stats(record: *const usize) -> Option<protocol::RecordStats> {
    use crate::hooks::diag::{read_f32_guarded, read_u32_guarded};

    let base = record as usize;
    let clean = |v: u32| if v == EMPTY_SIGIL_HASH { 0 } else { v };
    let sane_f32 = |v: Option<f32>| {
        v.filter(|f| f.is_finite() && *f >= 0.0 && *f < 1e9)
            .unwrap_or(0.0)
    };

    // Head-block level first; fall back to the wire-replicated scalar.
    let level = match sanity_u32(read_u32_guarded(base, RECORD_STATS_OFFSET), 150) {
        0 => sanity_u32(read_u32_guarded(base, RECORD_LEVEL_WIRE_OFFSET), 150),
        lvl => lvl,
    };
    let hp = clean(read_u32_guarded(base, RECORD_STATS_OFFSET + 0x04));
    let attack = clean(read_u32_guarded(base, RECORD_STATS_OFFSET + 0x08));
    let unk_50 = clean(read_u32_guarded(base, RECORD_STATS_OFFSET + 0x0C));
    let stun_power = sane_f32(read_f32_guarded(base, RECORD_STATS_OFFSET + 0x10));
    let critical_rate = sane_f32(read_f32_guarded(base, RECORD_STATS_OFFSET + 0x14));
    let power = clean(read_u32_guarded(base, RECORD_STATS_OFFSET + 0x18));

    if hp == 0 && attack == 0 && power == 0 {
        return None;
    }
    Some(protocol::RecordStats {
        level,
        hp,
        attack,
        unk_50,
        stun_power,
        critical_rate,
        power,
    })
}

/// Parses the 18-u32 weapon-state struct (live-labeled 2026-07-17 against the
/// user's Hraesvelgr — see `protocol::WeaponState` for the field map). The
/// same layout appears at `blob+0x50` (record `+0x5E80` blob, online contexts)
/// and at `row+0x70` of the per-character save rows. Returns `None` unless the
/// weapon id slot holds a plausible hash.
fn parse_weapon_struct(ints: &[u32]) -> Option<protocol::WeaponState> {
    let valid_id = |v: u32| v != 0 && v != EMPTY_SIGIL_HASH;
    let weapon_id = *ints.get(1)?;
    if !valid_id(weapon_id) || weapon_id < 0x10000 {
        return None;
    }
    let clamp = |v: u32, max: u32| if v > max { 0 } else { v };
    let wrightstone_id = ints[14];
    let wrightstone_traits = [(8, 9), (10, 11), (12, 13)]
        .iter()
        .filter(|(id_idx, _)| valid_id(ints[*id_idx]))
        .map(|(id_idx, lvl_idx)| protocol::WeaponTraitPair {
            id: ints[*id_idx],
            level: clamp(ints[*lvl_idx], 99),
        })
        .collect();
    Some(protocol::WeaponState {
        weapon_id,
        exp: ints[4],
        star_level: clamp(ints[5], 9),
        plus_marks: clamp(ints[6], 99),
        awakening_level: clamp(ints[7], 20),
        wrightstone_id: if valid_id(wrightstone_id) {
            wrightstone_id
        } else {
            0
        },
        wrightstone_traits,
        innate_traits: Vec::new(),
    })
}

/// The weapon's active innate skills at `innate_off` (5 sentinel-terminated
/// id slots), with each level taken from the pair array at
/// `innate_off + INNATE_LEVEL_PAIRS_REL` by id lookup — the pairs repeat the
/// slot ids, but the level is only trusted when a pair's id matches, so an
/// unmatched slot degrades to level 0. Guarded reads — never faults.
fn read_innate_traits(src: usize, innate_off: usize) -> Vec<protocol::WeaponTraitPair> {
    use crate::hooks::diag::read_u32_guarded;

    let valid_id = |v: u32| v != 0 && v != EMPTY_SIGIL_HASH;
    let pairs_off = innate_off + INNATE_LEVEL_PAIRS_REL;
    (0..WEAPON_INNATE_SLOTS)
        .map(|i| read_u32_guarded(src, innate_off + i * 4))
        .take_while(|id| valid_id(*id))
        .map(|id| {
            let level = (0..WEAPON_INNATE_SLOTS)
                .find(|j| read_u32_guarded(src, pairs_off + j * 8) == id)
                .map(|j| read_u32_guarded(src, pairs_off + j * 8 + 4))
                .filter(|level| *level <= 99)
                .unwrap_or(0);
            protocol::WeaponTraitPair { id, level }
        })
        .collect()
}

/// Reads the equipped weapon's state for a record.
///
/// Primary source: the record-inline block at record+0x50 — the game's own
/// weapon-state home, filled for every record by `FUN_140a2d8d0` (in-quest
/// modes copy the live weapon-instance table column; town modes copy from the
/// loadout). Active innate skill ids follow at +0x94, already
/// upgrade-resolved (e.g. Catastrophe → Catastrophe Nova). Fallback: the
/// network blob `*(record+0x5E80)+0x50`, which mirrors the same layout. If
/// the chosen struct carries no inline innate ids, they resolve from the
/// weapon-instance table column named by the struct's leading key word.
/// Guarded reads — never faults.
fn read_weapon_state(record: *const usize) -> Option<protocol::WeaponState> {
    use crate::hooks::diag::{read_ptr_guarded, read_u32_guarded, MODULE_BASE};
    use std::sync::atomic::Ordering;

    let valid_id = |v: u32| v != 0 && v != EMPTY_SIGIL_HASH;

    // (state, table key = struct[0], source base, innate-id offset)
    let parse_at = |base: usize,
                    state_off: usize,
                    innate_off: usize|
     -> Option<(protocol::WeaponState, u32, usize, usize)> {
        let ints: Vec<u32> = (0..18)
            .map(|i| read_u32_guarded(base, state_off + i * 4))
            .collect();
        parse_weapon_struct(&ints).map(|state| (state, ints[0], base, innate_off))
    };

    let base = record as usize;
    let (mut state, table_key, src, innate_off) =
        parse_at(base, RECORD_WEAPON_STATE_OFFSET, RECORD_INNATE_OFFSET).or_else(|| {
            let blob =
                read_ptr_guarded(base, RECORD_WEAPON_BLOB_OFFSET).filter(|b| *b > 0x10000)?;
            parse_at(blob, BLOB_WEAPON_STATE_OFFSET, BLOB_INNATE_OFFSET)
        })?;

    state.innate_traits = read_innate_traits(src, innate_off);

    if state.innate_traits.is_empty() && valid_id(table_key) {
        let module = MODULE_BASE.load(Ordering::Relaxed);
        if module != 0 {
            state.innate_traits = resolve_innate_skills(module, table_key)
                .unwrap_or_default()
                .into_iter()
                .filter(|id| valid_id(*id))
                .map(|id| protocol::WeaponTraitPair { id, level: 0 })
                .collect();
        }
    }
    Some(state)
}

/// One-run verifier for the record-inline weapon block (WSDIAG3): dumps
/// record+0x40..0x140 per slot (the 18-u32 struct sits at +0x50, innate ids
/// at +0x94; the words after +0xA8 are the innate-LEVEL candidates — the
/// user's 32/22/12/1 should appear), the record's fill mode (+0x5EAC) and
/// mode-0..2 source key (`*(record+0x5DD0)+0x44`), plus a one-shot dump of
/// every populated weapon-instance table column (key, weapon id,
/// progression, innates). Bounded fires.
#[cfg(feature = "hookdiag")]
fn log_weapon_state_probe(record: *const usize, player_key: u32) {
    use crate::hooks::diag::{first_n, read_ptr_guarded, read_u32_guarded, MODULE_BASE};
    use std::sync::atomic::{AtomicU32, Ordering};

    static FIRES: AtomicU32 = AtomicU32::new(0);
    if !first_n(&FIRES, 16) {
        return;
    }
    let dump = |base: usize, off: usize, n: usize| -> String {
        (0..n)
            .map(|i| format!("{:08x}", read_u32_guarded(base, off + i * 4)))
            .collect::<Vec<_>>()
            .join(" ")
    };

    let base = record as usize;
    let mode = read_u32_guarded(base, RECORD_MODE_OFFSET);
    let src_key = read_ptr_guarded(base, RECORD_WEAPON_SOURCE_OFFSET)
        .map(|p| read_u32_guarded(p, WEAPON_SOURCE_KEY_OFFSET))
        .unwrap_or(0);
    log::info!(
        "WSDIAG3 key={player_key:#010x} mode={mode} srckey={src_key:#x} rec 40..140: {}",
        dump(base, 0x40, 64)
    );

    let module = MODULE_BASE.load(Ordering::Relaxed);
    if module == 0 {
        return;
    }
    let Some(table) = read_ptr_guarded(module, WEAPON_TABLE_RVA).filter(|t| *t != 0) else {
        return;
    };
    // The table is global state — dump its populated columns once per run.
    static TABLE_DUMPED: AtomicU32 = AtomicU32::new(0);
    if first_n(&TABLE_DUMPED, 1) {
        for row in 0..WS_TABLE_ROWS {
            for col in 0..WS_TABLE_COLS {
                let entry =
                    table + WS_TABLE_BASE + row * WS_TABLE_ROW_STRIDE + col * WS_TABLE_COL_STRIDE;
                let key = read_u32_guarded(entry, 0);
                if key == 0 || key == EMPTY_SIGIL_HASH {
                    continue;
                }
                log::info!("WSDIAG3 tbl[{row}.{col}] {}", dump(entry, 0, 24));
            }
        }
    }
}

/// Scans the live weapon-instance table (see [`WEAPON_TABLE_RVA`]) for the
/// column matching `skill_key` and returns its 5 innate skill-id slots — the
/// same table walk the game's weapon-state fillers perform. Guarded reads.
fn resolve_innate_skills(module: usize, skill_key: u32) -> Option<Vec<u32>> {
    use crate::hooks::diag::read_ptr_guarded;
    use crate::hooks::diag::read_u32_guarded;

    let table = read_ptr_guarded(module, WEAPON_TABLE_RVA).filter(|t| *t != 0)?;
    for row in 0..WS_TABLE_ROWS {
        for col in 0..WS_TABLE_COLS {
            let entry =
                table + WS_TABLE_BASE + row * WS_TABLE_ROW_STRIDE + col * WS_TABLE_COL_STRIDE;
            if read_u32_guarded(entry, 0) != skill_key {
                continue;
            }
            if read_u32_guarded(entry, WS_TABLE_VALID_OFFSET) as i32 <= 0 {
                return None;
            }
            return Some(
                (0..WS_TABLE_SKILL_SLOTS)
                    .map(|i| read_u32_guarded(entry, WS_TABLE_DATA_OFFSET + i * 4))
                    .collect(),
            );
        }
    }
    None
}

/// Reads the record's unlocked skillboard (master trait) node effect ids —
/// a guarded-read reimplementation of the game's own query `FUN_140297bc0`
/// (see [`CHARA_POWER_RVA`] for the maps and row layout, decompiled
/// 2026-07-17). The record's inline `{id, bits}` array is snapshotted first so
/// the per-node unlock test is a lookup, not a 400-entry rescan. Mirrors the
/// game's exclusion of small UI/stat ids (<10, 100..110, 200..210). Returns
/// ids in catalog order, capped defensively. Guarded reads — never faults.
fn read_record_skillboard(record: *const usize) -> Vec<u32> {
    use crate::hooks::diag::{read_ptr_guarded, read_u32_guarded, MODULE_BASE};
    use std::sync::atomic::Ordering;

    let module = MODULE_BASE.load(Ordering::Relaxed);
    if module == 0 {
        return Vec::new();
    }
    let Some(mgr) = read_ptr_guarded(module, CHARA_POWER_RVA).filter(|m| *m != 0) else {
        return Vec::new();
    };
    let base = record as usize;
    let charid = read_u32_guarded(base, RECORD_CHARID_OFFSET);
    if charid == 0 || charid == EMPTY_SIGIL_HASH {
        return Vec::new();
    }
    let Some(char_node) = game_hashmap_find(
        mgr,
        SB_CHAR_MAP_END,
        SB_CHAR_MAP_BUCKETS,
        SB_CHAR_MAP_MASK,
        charid,
    ) else {
        return Vec::new();
    };
    let (Some(begin), Some(end)) = (
        read_ptr_guarded(char_node, 0x18),
        read_ptr_guarded(char_node, 0x20),
    ) else {
        return Vec::new();
    };
    if begin == 0 || end < begin {
        return Vec::new();
    }
    // Entries are {u32 key, u32 pad}; a character's board has ~100 nodes, so
    // anything past a few hundred means we misread a pointer — bail.
    let count = (end - begin) / 8;
    if count == 0 || count > 512 {
        return Vec::new();
    }

    // Snapshot the record's inline unlock array: node id -> bit field.
    let unlock: std::collections::HashMap<u32, u32> = (0..RECORD_NODE_ARRAY_COUNT)
        .filter_map(|n| {
            let e = RECORD_NODE_ARRAY_OFFSET + n * RECORD_NODE_ARRAY_STRIDE;
            let id = read_u32_guarded(base, e);
            (id != 0 && id != EMPTY_SIGIL_HASH).then(|| (id, read_u32_guarded(base, e + 4)))
        })
        .collect();
    if unlock.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    for i in 0..count {
        let key = read_u32_guarded(begin, i * 8);
        if key == 0 || key == EMPTY_SIGIL_HASH {
            continue;
        }
        let Some(node) = game_hashmap_find(
            mgr,
            SB_NODE_MAP_END,
            SB_NODE_MAP_BUCKETS,
            SB_NODE_MAP_MASK,
            key,
        ) else {
            continue;
        };
        let Some(row) = read_ptr_guarded(node, 0x18).filter(|v| *v > 0x10000) else {
            continue;
        };
        let effect_id = read_u32_guarded(row, 0x74);
        // The game's own filter: tiny ids are UI/stat placeholder rows.
        if effect_id < 10 || (100..110).contains(&effect_id) || (200..210).contains(&effect_id) {
            continue;
        }
        let bit = read_u32_guarded(row, 0x5C);
        if bit > 0x1F {
            continue;
        }
        let node_id = read_u32_guarded(row, 0x48);
        let unlocked = unlock
            .get(&node_id)
            .is_some_and(|bits| (bits >> bit) & 1 != 0);
        if unlocked {
            out.push(effect_id);
            if out.len() >= 128 {
                break;
            }
        }
    }
    out
}

/// Reads the 4 equipped overmasteries and the character level from the
/// per-character town loadout `*(record+0x5DC8)`. Returns `(vec![], 0)` when the
/// loadout pointer is NULL (in-quest) or unreadable. Each overmastery is
/// `{u32 id, u32 level_bits}`; the level_bits flag is carried through in
/// [`protocol::Overmastery::flags`] (bit N → level N+1) with `value` left 0.0 —
/// the loadout stores only id+level, not the in-game computed magnitude. Empty
/// slots hold the [`EMPTY_SIGIL_HASH`] sentinel and are dropped. Guarded reads.
fn read_loadout_overmasteries_and_level(record: *const usize) -> (Vec<protocol::Overmastery>, u32) {
    use crate::hooks::diag::{read_ptr_guarded, read_u32_guarded};

    let Some(loadout) = read_ptr_guarded(record as usize, RECORD_LOADOUT_OFFSET) else {
        return (Vec::new(), 0);
    };
    if loadout == 0 {
        return (Vec::new(), 0);
    }

    let overmasteries = (0..LOADOUT_OVERMASTERY_COUNT)
        .filter_map(|i| {
            let entry = LOADOUT_OVERMASTERY_OFFSET + i * LOADOUT_OVERMASTERY_STRIDE;
            let id = read_u32_guarded(loadout, entry);
            if id == 0 || id == EMPTY_SIGIL_HASH {
                return None;
            }
            Some(protocol::Overmastery {
                id,
                flags: read_u32_guarded(loadout, entry + 0x4),
                value: 0.0,
            })
        })
        .collect();

    let player_level = read_u32_guarded(loadout, LOADOUT_LEVEL_OFFSET);

    (overmasteries, player_level)
}

/// Reads the 4 equipped-summon entries inline in the player record (see
/// [`RECORD_SUMMON_OFFSET`] for the live-verified layout). Empty slots hold the
/// [`EMPTY_SIGIL_HASH`] sentinel and are dropped. Guarded reads — never faults,
/// safe on any record the identity hook receives.
fn read_record_summons(record: *const usize) -> Vec<protocol::EquippedSummon> {
    use crate::hooks::diag::read_u32_guarded;

    let base = record as usize;
    (0..RECORD_SUMMON_COUNT)
        .filter_map(|i| {
            let entry = RECORD_SUMMON_OFFSET + i * RECORD_SUMMON_STRIDE;
            let summon_id = read_u32_guarded(base, entry);
            if summon_id == 0 || summon_id == EMPTY_SIGIL_HASH {
                return None;
            }
            Some(protocol::EquippedSummon {
                summon_id,
                main_trait_id: read_u32_guarded(base, entry + 0x8),
                main_trait_level: read_u32_guarded(base, entry + 0x10),
                bonus_id: read_u32_guarded(base, entry + 0xC),
                bonus_level: read_u32_guarded(base, entry + 0x14),
            })
        })
        .collect()
}

/// Reads the player-key from a concrete combat actor via ReadProcessMemory so an
/// invalid/short actor range fails the read rather than faulting the game thread.
fn read_actor_player_key(actor: *const usize) -> Option<u32> {
    let mut player_key = 0u32;
    let mut bytes_read = 0usize;
    let result = unsafe {
        ReadProcessMemory(
            HANDLE(-1),
            actor.byte_add(ACTOR_PLAYER_KEY_OFFSET).cast::<c_void>(),
            (&mut player_key as *mut u32).cast::<c_void>(),
            std::mem::size_of::<u32>(),
            Some(&mut bytes_read),
        )
    };

    if result.is_err()
        || bytes_read != std::mem::size_of::<u32>()
        || player_key == 0
        || player_key == INVALID_PLAYER_KEY
    {
        return None;
    }

    Some(player_key)
}

/// Reads the stable identity fields from a snapshot. Field offsets match the
/// [`SigilList`] layout (verified surviving in v2.0.2): is_online @ 0x1C8,
/// character_name @ 0x1E8, display_name @ 0x208, party_index @ 0x22C.
unsafe fn read_player_identity(snapshot: *const u8) -> Option<StoredPlayerIdentity> {
    if snapshot.is_null() {
        return None;
    }

    // The value may come out of game memory without being a pointer at all —
    // the actor-embedded record slot on an actor that carries no record holds
    // arbitrary data (2026-07-18 in-quest crash: 0x000005b1000005b0). Probe the
    // whole struct range up front; every read below stays inside it except the
    // display-name buffer, which checked_raw() guards itself.
    if !crate::hooks::diag::readable(snapshot as usize, std::mem::size_of::<SigilList>()) {
        return None;
    }

    let list = &*(snapshot as *const SigilList);

    let is_online = list.is_online;
    let party_index = list.party_index;

    // Reject obviously-bogus snapshots (garbage pointer / wrong struct) so we
    // never cache a junk identity.
    if is_online > 1 || party_index > 3 {
        return None;
    }

    let display_name =
        VBuffer(std::ptr::addr_of!(list.display_name) as *const usize).checked_raw()?;

    // A real player always has a display name; an empty one means this snapshot
    // is not a resolvable identity yet.
    if display_name.as_bytes().is_empty() {
        return None;
    }

    let character_name = CStr::from_bytes_until_nul(&list.character_name)
        .ok()
        .map(|cstr| cstr.to_owned())
        .unwrap_or_else(|| CString::new("").expect("empty CString is valid"));

    Some(StoredPlayerIdentity {
        character_name,
        display_name,
        party_index: party_index as u8,
        is_online: is_online != 0,
        sigils: read_snapshot_sigils(snapshot),
        // Filled by the caller from the record/loadout/save maps; the snapshot
        // carries none of this data.
        summons: Vec::new(),
        overmasteries: Vec::new(),
        player_level: 0,
        abilities: Vec::new(),
        weapon_key: String::new(),
        master_level: 0,
        skillboard: Vec::new(),
        stats: None,
        weapon_state: None,
        cap_ups: (None, None, None),
        limit_bonus_caps: (None, None, None),
    })
}

/// The party slot from an identity snapshot, applying exactly the acceptance
/// checks [`read_player_identity`] applies and allocating nothing.
///
/// The slot-key path — which is what damage, stun and status attribution all
/// resolve through — needs one `u8`, but reaching it through the full read
/// built a `Vec<Sigil>` and two `CString`s per call, on a game thread, per
/// event. The checks are duplicated rather than shared because the full read
/// wants the values and this wants only the verdict; the tests assert the two
/// agree, which is the property that actually matters.
unsafe fn snapshot_party_index(snapshot: *const u8) -> Option<u8> {
    if snapshot.is_null() {
        return None;
    }
    if !crate::hooks::diag::readable(snapshot as usize, std::mem::size_of::<SigilList>()) {
        return None;
    }

    let list = &*(snapshot as *const SigilList);
    if list.is_online > 1 || list.party_index > 3 {
        return None;
    }

    // A real player always has a display name; an empty or unreadable one means
    // this snapshot is not a resolvable identity yet.
    let name_len = VBuffer(std::ptr::addr_of!(list.display_name) as *const usize).checked_len()?;
    if name_len == 0 {
        return None;
    }

    Some(list.party_index as u8)
}

/// Reads the sigil entries at the head of the identity snapshot (v2.0.2 layout:
/// 13 x 0x24-byte entries at offset 0, same allocation as the verified name/party
/// fields). Unequipped slots hold the 0x887AE0B0 sentinel and are dropped, as is
/// anything implausible — the 13th entry overlaps fields the pre-2.0 struct used
/// differently, so a strict filter keeps a mispopulated slot from leaking junk.
unsafe fn read_snapshot_sigils(snapshot: *const u8) -> Vec<protocol::Sigil> {
    let entries = std::slice::from_raw_parts(snapshot as *const SigilEntry, SNAPSHOT_SIGIL_COUNT);

    entries
        .iter()
        .filter(|entry| {
            entry.sigil_id != 0
                && entry.sigil_id != EMPTY_SIGIL_HASH
                && entry.first_trait_id != 0
                && entry.sigil_level <= 100
        })
        .map(|entry| protocol::Sigil {
            first_trait_id: entry.first_trait_id,
            first_trait_level: entry.first_trait_level,
            second_trait_id: entry.second_trait_id,
            second_trait_level: entry.second_trait_level,
            sigil_id: entry.sigil_id,
            equipped_character: entry.equipped_character,
            sigil_level: entry.sigil_level,
            acquisition_count: entry.acquisition_count,
            notification_enum: entry.notification_enum,
        })
        .collect()
}

/// hookdiag: one line per RefreshPlayerIdentity fire — the record's mode enum
/// (+0x5EAC, values 0..=5 per the v2.0.2 decompile of FUN_140a2b600), the id at
/// +0x5EA8 the hook currently keys identities by, its u32 neighbors, and the
/// snapshot's name/party/online. All reads guarded; never faults.
#[cfg(feature = "hookdiag")]
fn log_identity_record(record: *const usize, snapshot: *const u8, player_key: u32) {
    use crate::hooks::diag::read_u32_guarded;

    let base = record as usize;
    let mode = read_u32_guarded(base, 0x5EAC);
    let n_a4 = read_u32_guarded(base, 0x5EA4);
    let n_b0 = read_u32_guarded(base, 0x5EB0);
    let (name, party, online) = match unsafe { read_player_identity(snapshot) } {
        Some(identity) => (
            identity.display_name.to_string_lossy().into_owned(),
            identity.party_index as i32,
            identity.is_online as i32,
        ),
        None => ("<unreadable>".to_string(), -1, -1),
    };
    log::info!(
        "IDDIAG record={base:#x} mode@5EAC={mode} id@5EA8={player_key:#010x} n@5EA4={n_a4:#010x} n@5EB0={n_b0:#010x} party={party} online={online} name={name}"
    );
}

// The slot-key scheme lives in `protocol` (it crosses the wire in
// `actor_index` — the parser must recognize keys with the SAME definitions the
// hook builds them with). Live context for this crate: the game's own actor
// index (`+0x170`) and player key (`+0x1AB40`) are what proved CHARACTER-scoped
// (2026-07-18: both Maglielles carried actor_index 2368344264 / id 0x25d46f4b).
pub(crate) use protocol::{is_player_slot_key, player_slot_key as slot_key};

/// Full identity read from the actor's own embedded record (`actor+0x15030`).
/// `None` for non-player actors (enemies, pets) — their record slot holds
/// arbitrary bytes and the guarded snapshot parse rejects it.
fn embedded_identity_struct(actor: usize) -> Option<StoredPlayerIdentity> {
    use crate::hooks::diag::read_ptr_guarded;

    let record = actor.checked_add(ACTOR_RECORD_OFFSET)?;
    let snapshot = read_ptr_guarded(record, PLAYER_IDENTITY_OFFSET)?;
    if snapshot == 0 {
        return None;
    }
    unsafe { read_player_identity(snapshot as *const u8) }
}

/// The per-player slot key for an actor, when its embedded record resolves.
/// This is what damage/SBA/stun attribution uses instead of the shared
/// character-scoped index.
pub(crate) fn player_slot_key_for_actor(actor: *const usize) -> Option<u32> {
    use crate::hooks::diag::read_ptr_guarded;

    if actor.is_null() {
        return None;
    }
    // Deliberately NOT `embedded_identity_struct`: this path wants only the
    // slot, and the full read allocates a `Vec<Sigil>` and two `CString`s to
    // hand back values that are thrown away — on a game thread, per event, and
    // now once per damage hit for the victim gate too. `snapshot_party_index`
    // applies the identical acceptance checks; the tests assert the two agree.
    let record = (actor as usize).checked_add(ACTOR_RECORD_OFFSET)?;
    let snapshot = read_ptr_guarded(record, PLAYER_IDENTITY_OFFSET)?;
    if snapshot == 0 {
        return None;
    }
    unsafe { snapshot_party_index(snapshot as *const u8) }.map(slot_key)
}

#[allow(dead_code)] // used by the hookdiag SBAUPD/SBAPOLL probes
pub(crate) fn actor_embedded_identity(actor: usize) -> Option<(u8, bool, String)> {
    let identity = embedded_identity_struct(actor)?;
    Some((
        identity.party_index,
        identity.is_online,
        identity.display_name.to_string_lossy().into_owned(),
    ))
}

/// hookdiag: per-actor embedded-record probe (online same-character collapse).
/// One line per distinct damage actor: the character-scoped ids the current
/// resolution uses (idx@0x170, id@0x1AB40, flags@0x1AB64) NEXT TO the actor's
/// own embedded record (+0x15030: valid flag, record id, mode) and that
/// record's identity snapshot (party/online/name). If the embedded record
/// resolves per-player identity in BOTH solo and online lobbies, it becomes
/// the production keying fix. Guarded reads; never faults.
#[cfg(feature = "hookdiag")]
static ARDIAG_SEEN: std::sync::Mutex<Vec<(usize, u32)>> = std::sync::Mutex::new(Vec::new());

/// Reset the ARDIAG dedup set (called from the quest-load hook). Without this the
/// 64-entry budget fills during town/solo play and the online quest's actors —
/// the interesting ones — never get logged (proven by the 2026-07-18 run).
#[cfg(feature = "hookdiag")]
pub(crate) fn reset_ardiag_seen() {
    if let Ok(mut seen) = ARDIAG_SEEN.try_lock() {
        seen.clear();
    }
}

#[cfg(feature = "hookdiag")]
fn log_actor_record_probe(actor: usize, character_type: u32, actor_index: u32) {
    use crate::hooks::diag::{read_ptr_guarded, read_u32_guarded};

    let record = actor + ACTOR_RECORD_OFFSET;
    // Dedup by (actor, record mode) — NOT by actor alone: the embedded record is
    // REWRITTEN in place when a lobby forms (2026-07-18: the same record read
    // "mode=1 online=0 Manmoth" in town and "mode=4 online=1 <lobby name>" later),
    // so a mode change must re-log the actor or the online identities are never
    // captured. Mode read first (guarded, cheap) to build the key.
    let mode = read_u32_guarded(record, 0x5EAC);
    {
        let Ok(mut seen) = ARDIAG_SEEN.try_lock() else {
            return;
        };
        if seen.iter().any(|e| *e == (actor, mode)) {
            return;
        }
        if seen.len() >= 64 {
            return;
        }
        seen.push((actor, mode));
    }

    let idx170 = read_u32_guarded(actor, 0x170);
    let id_ab40 = read_u32_guarded(actor, ACTOR_PLAYER_KEY_OFFSET);
    let flags_ab64 = read_u32_guarded(actor, ACTOR_FLAGS_OFFSET);
    let valid = read_u32_guarded(record, RECORD_VALID_OFFSET) & 0xFF;
    let rec_id = read_u32_guarded(record, PLAYER_KEY_OFFSET);
    let snapshot = read_ptr_guarded(record, PLAYER_IDENTITY_OFFSET).unwrap_or(0);
    let (party, online, name) = match actor_embedded_identity(actor) {
        Some((party, online, name)) => (party as i32, online as i32, name),
        None => (-1, -1, "<unresolved>".to_string()),
    };
    log::info!(
        "ARDIAG actor={actor:#x} type={character_type:#010x} idx={actor_index} idx170={idx170:#x} \
         id@1AB40={id_ab40:#010x} flags@1AB64={flags_ab64:#010x} rec={record:#x} valid={valid} \
         rec_id={rec_id:#010x} mode={mode} snap={snapshot:#x} party={party} online={online} name={name}"
    );
}

/// hookdiag: one line per loadout->profile serialization — the source loadout's
/// character, party index and its four summon-entry slots. Answers whether the
/// ACTIVE loadout (as opposed to the town roster copies) ever carries the
/// equipped summons, and when profiles are (re)built. Guarded reads; first 40
/// calls only.
#[cfg(feature = "hookdiag")]
fn log_profile_build(profile: *mut u8, loadout: *mut u8) {
    use crate::hooks::diag::{first_n, read_u32_guarded};
    use std::sync::atomic::AtomicU32;

    static CALLS: AtomicU32 = AtomicU32::new(0);
    if !first_n(&CALLS, 40) {
        return;
    }
    let lp = loadout as usize;
    let charid = read_u32_guarded(lp, 0x3518);
    let party = read_u32_guarded(lp, 0x3514);
    let mut summons = String::new();
    for i in 0..4usize {
        let e = 0x33EC + i * 0x10;
        summons.push_str(&format!(
            " s{i}={:08x}/{:08x}/{:08x}/{:08x}",
            read_u32_guarded(lp, e),
            read_u32_guarded(lp, e + 4),
            read_u32_guarded(lp, e + 8),
            read_u32_guarded(lp, e + 0xC)
        ));
    }
    log::info!(
        "PRDIAG serialize loadout={lp:#x} charid={charid:#010x} party={party:#x} profile={:#x}{summons}",
        profile as usize
    );
}

/// hookdiag: master-level / overmastery probe. Ghidra (record dispatcher
/// FUN_140a23e70): the current master-level row is selected from the vector at
/// `DAT_147c24a78+0x430` by comparing row+0x54 against the SUM of the two ints
/// at `DAT_147c24a78+0x12A244` / `+0x12A248` (master level/exp candidates —
/// dump the neighborhood). The record also carries 4 × 0x10 overmastery entries
/// at +0x58B8 `{?, level_bits, id, ?}` (bit N -> level, same decode as the
/// loadout pairs) — dump those too. Guarded reads. The 2026-07-17 run proved a
/// fixed first-48 budget burns out at startup BEFORE save data populates
/// (mgr ints read 0/hash, all record overmasteries still sentinel), so sample
/// the first few fires AND every 256th thereafter to catch post-load state.
#[cfg(feature = "hookdiag")]
fn log_progress_probe(record: *const usize) {
    use crate::hooks::diag::{read_ptr_guarded, read_u32_guarded, MODULE_BASE};
    use std::sync::atomic::{AtomicU32, Ordering};

    static CALLS: AtomicU32 = AtomicU32::new(0);
    let call = CALLS.fetch_add(1, Ordering::Relaxed);
    if call >= 8 && call % 256 != 0 {
        return;
    }

    let base = record as usize;
    let mut om = String::new();
    for i in 0..4usize {
        let e = 0x58B8 + i * 0x10;
        om.push_str(&format!(
            " om{i}={:08x}/{:08x}/{:08x}/{:08x}",
            read_u32_guarded(base, e),
            read_u32_guarded(base, e + 4),
            read_u32_guarded(base, e + 8),
            read_u32_guarded(base, e + 0xC)
        ));
    }
    log::info!("MLDIAG record={base:#x}{om}");

    let module = MODULE_BASE.load(Ordering::Relaxed);
    if module == 0 {
        return;
    }
    // CORRECTED 07-17 (dispatcher re-decompile): the master-exp ints live in
    // the save-root global (+0x12A244 + +0x12A248, -1 = unloaded), NOT the
    // chara-power global as previously recorded — the earlier all-zero reads
    // were the wrong global. The dispatcher sums them and walks the
    // chara_master_exp row vector @ +0x430/0x438 (threshold @ row+0x54).
    //
    // Use the named consts: these were left as raw 2.0.2 literals through the
    // 2.0.3 pass, so this probe read a stale global for a whole game version.
    let Some(mgr) = read_ptr_guarded(module, SAVE_ROOT_RVA) else {
        return;
    };
    if mgr == 0 {
        return;
    }
    let row: Vec<String> = (0x12A230..0x12A270)
        .step_by(4)
        .map(|off| format!("{:08x}", read_u32_guarded(mgr, off)))
        .collect();
    log::info!(
        "MLDIAG mgr24980={mgr:#x} @12A230..12A270: {}",
        row.join(" ")
    );

    // The dispatcher also resolves the record's charid in a map at
    // DAT_147c24980 +0x40 (end) / +0x50 (buckets) / +0x68 (mask); the value
    // struct carries an instance id at +0x8C (equipped-equipment candidate).
    // Walk it for this record's charid and dump the value head.
    let charid_w = read_u32_guarded(base, 0x5B10);
    if charid_w != 0 && charid_w != 0x887AE0B0 {
        let mask = read_u32_guarded(mgr, 0x68);
        if let (Some(buckets), Some(end_node)) =
            (read_ptr_guarded(mgr, 0x50), read_ptr_guarded(mgr, 0x40))
        {
            let bucket = (mask & charid_w) as usize * 0x10;
            if let (Some(mut node), Some(bucket_head)) = (
                read_ptr_guarded(buckets, bucket + 8),
                read_ptr_guarded(buckets, bucket),
            ) {
                for _ in 0..16 {
                    if node == 0 || node == end_node {
                        break;
                    }
                    if read_u32_guarded(node, 0x10) == charid_w {
                        if let Some(val) = read_ptr_guarded(node, 0x18) {
                            if val > 0x10000 {
                                let v: Vec<String> = (0..0x40_usize)
                                    .map(|i| format!("{:08x}", read_u32_guarded(val, i * 4)))
                                    .collect();
                                log::info!(
                                    "MLDIAG charid={charid_w:#010x} eq={val:#x} @0..100: {}",
                                    v.join(" ")
                                );
                                // Resolve the entry's id slots through the
                                // FNV-1a(u32) instance map at mgr24980
                                // +0x100 (end) / +0x110 (buckets) / +0x128
                                // (u64 mask) — the weapon's progression state
                                // (level 1-150 / awakening 0-8 /
                                // transcendence 0-8 / +0-99) should be in the
                                // resolved value (vector of u32 pairs @
                                // node+0x18..+0x20).
                                for off in [0x3C_usize, 0x1C, 0x20, 0x24, 0x28, 0x2C, 0x30] {
                                    let id = read_u32_guarded(val, off);
                                    if id == 0 || id == 0x887AE0B0 {
                                        continue;
                                    }
                                    log_instance_resolve(mgr, id, off);
                                }
                            }
                        }
                        break;
                    }
                    if node == bucket_head {
                        break;
                    }
                    let Some(next) = read_ptr_guarded(node, 8) else {
                        break;
                    };
                    node = next;
                }
            }
        }
    }

    // The dispatcher's master-level walk (gated on record+0x5B34 & 4) writes
    // its derived values to record+0x5B2C/0x5B60/0x5B64/0x5B68/0x5B6C — dump
    // that whole region: if the walk ran, the record itself carries usable
    // master-level data even though the mgr+0x12A244/48 candidates read 0.
    let derived: Vec<String> = (0x5B2C..0x5B70)
        .step_by(4)
        .map(|off| format!("{:08x}", read_u32_guarded(base, off)))
        .collect();
    log::info!(
        "MLDIAG record={base:#x} derived@5B2C..5B70: {}",
        derived.join(" ")
    );

    // Per-character progression: CharaPower keeps a charid-keyed map at
    // +0xAA8 (end) / +0xAB8 (buckets) / +0xAD0 (mask) — the same
    // std::unordered_map layout as the loadout-preset map (end/+0x10/+0x28).
    // Its value should hold the character's master level/exp and unlocked
    // master-trait nodes (the solo-play skillboard source). Dump the value
    // head raw for this record's charid.
    let charid = read_u32_guarded(base, 0x5B10);
    if charid == 0 || charid == 0x887AE0B0 {
        return;
    }
    let mask = read_u32_guarded(mgr, 0xAD0);
    let (Some(buckets), Some(end_node)) =
        (read_ptr_guarded(mgr, 0xAB8), read_ptr_guarded(mgr, 0xAA8))
    else {
        return;
    };
    let bucket = (mask & charid) as usize * 0x10;
    let (Some(mut node), Some(bucket_head)) = (
        read_ptr_guarded(buckets, bucket + 8),
        read_ptr_guarded(buckets, bucket),
    ) else {
        return;
    };
    for _ in 0..16 {
        if node == 0 || node == end_node {
            break;
        }
        if read_u32_guarded(node, 0x10) == charid {
            let head: Vec<String> = (0x14..0x94_usize)
                .step_by(4)
                .map(|off| format!("{:08x}", read_u32_guarded(node, off)))
                .collect();
            log::info!(
                "MLDIAG charid={charid:#010x} cpnode={node:#x} @14..94: {}",
                head.join(" ")
            );
            if let Some(val) = read_ptr_guarded(node, 0x18) {
                if val > 0x10000 {
                    let v: Vec<String> = (0..0x20_usize)
                        .map(|i| format!("{:08x}", read_u32_guarded(val, i * 4)))
                        .collect();
                    log::info!(
                        "MLDIAG charid={charid:#010x} cpval={val:#x} @0..80: {}",
                        v.join(" ")
                    );
                }
            }
            // Hunt the per-character master level (party = 50/40/38/47):
            // scan the node's value struct for small nonzero dwords past the
            // unlocked-node id array — levels/exp should stand out from the
            // 32-bit hashes that fill the rest.
            let smalls: Vec<String> = (0x14..0x800_usize)
                .step_by(4)
                .filter_map(|off| {
                    let v = read_u32_guarded(node, off);
                    (v > 0 && v <= 0x100000).then(|| format!("{off:#x}={v:#x}"))
                })
                .take(24)
                .collect();
            log::info!("MLDIAG charid={charid:#010x} smalls: {}", smalls.join(" "));
            break;
        }
        if node == bucket_head {
            break;
        }
        let Some(next) = read_ptr_guarded(node, 8) else {
            break;
        };
        node = next;
    }

    // One-shot: scan the three save-root objects for the party's known
    // master levels — a dword == 50 (0x32) with a 5 within the next 0x20
    // bytes is a distinctive {level, stars} fingerprint. Candidates are
    // narrowed next run against the party's 50/40/38/47.
    // Not a one-shot: the 07-17 one-shot fired at boot on zeroed objects and
    // burned the whole game-relaunch cycle. Rescan on a slow periodic stride
    // whenever loaded data is present (~50ms per scan, negligible).
    static SCAN_TICK: AtomicU32 = AtomicU32::new(0);
    if read_u32_guarded(base, 0x5AC0) != 0 && SCAN_TICK.fetch_add(1, Ordering::Relaxed) % 1024 == 0
    {
        // `0x7c23f48` is a 2.0.2-era literal no pass has ever re-derived, so it is
        // stale on 2.0.4 and this probe reads nothing through it. Left un-guessed
        // rather than shifted by an assumed section delta; hookdiag-only, so no
        // production path depends on it. Re-derive before trusting its output.
        for global in [0x7c23f48_usize, SAVE_ROOT_RVA, CHARA_POWER_RVA] {
            let Some(agg) = read_ptr_guarded(module, global) else {
                continue;
            };
            if agg == 0 {
                continue;
            }
            let mut hits: Vec<String> = Vec::new();
            for off in (0..0x130000_usize).step_by(4) {
                let v = read_u32_guarded(agg, off);
                // {u32 level=50, u32 stars=5 nearby}, packed {u16 50, u16 5},
                // or float 50.0f — three plausible encodings of M.Lvl 50+5*.
                if v == 0x32 {
                    let star = (off + 4..off + 0x24)
                        .step_by(4)
                        .any(|o| read_u32_guarded(agg, o) == 5);
                    if star {
                        hits.push(format!("u32@{off:#x}"));
                    }
                } else if v == 0x0005_0032 {
                    hits.push(format!("u16pair@{off:#x}"));
                } else if v == 0x4248_0000 {
                    hits.push(format!("f32@{off:#x}"));
                }
                if hits.len() >= 40 {
                    break;
                }
            }
            log::info!(
                "MLDIAG scan global={global:#x} agg={agg:#x} lvl50 hits: {}",
                hits.join(" ")
            );
        }
        // 07-17: CharaPower agg+0x30100 = {u64 level=50, u64 stars=5, u64 15}
        // — the master-level block for ONE character. The party's other
        // levels (40/38/47) are not within ±0x200, so per-character blocks
        // sit at some larger stride. Scan for the distinctive triple
        // {1..=60, 0..=5, 15} at 8-byte alignment to find them all.
        if let Some(cp) = read_ptr_guarded(module, CHARA_POWER_RVA) {
            if cp != 0 {
                let mut hits: Vec<String> = Vec::new();
                for off in (0..0x130000_usize).step_by(8) {
                    let lvl = read_u32_guarded(cp, off);
                    if !(1..=60).contains(&lvl) {
                        continue;
                    }
                    let stars = read_u32_guarded(cp, off + 8);
                    let cap = read_u32_guarded(cp, off + 0x10);
                    if stars <= 5 && cap == 0xF && read_u32_guarded(cp, off + 4) == 0 {
                        hits.push(format!("{off:#x}={lvl}/{stars}"));
                        if hits.len() >= 30 {
                            break;
                        }
                    }
                }
                log::info!("MLDIAG cp triple-scan: {}", hits.join(" "));
            }
        }
    }
}

/// hookdiag: read the record's inline 400 × 0x38 array as the RESOLVED
/// limit-bonus store it turned out to be.
///
/// The first pass of this probe dumped raw words, and the 2026-08-09 live run
/// decoded the whole stride from what came back. An entry is one `limit_bonus`
/// the player has unlocked, followed by THREE param slots of four words each —
/// 2 + 3 × 4 = 14 words = 0x38, with nothing left over:
///
/// ```text
///   w0        limit_bonus.Key            (64/64 matched the table, live)
///   w1        unlock mask (0x0101, 0x0303, 0x1f1f ... a bit run per byte)
///   w2+4k     limit_bonus_param.Key      EMPTY_SIGIL_HASH when the slot is unused
///   w3+4k     the slot's own level mask  (matches w1's low byte)
///   w4+4k     limit_bonus_param's TYPE ID — 103/104/105/106 are the damage caps
///   w5+4k     the value, as f32          (type 100 + 2.0 = "Skill Damage +2%")
/// ```
///
/// That w4 equals the type id this repo reads at table row `+0x48` is an
/// independent, live confirmation of that column — the shipped `.headers` puts
/// it elsewhere.
///
/// **The game has already done the summation.** If a cap-typed slot appears
/// here, the Mastery term can be READ rather than reconstructed from which
/// nodes are unlocked, and `src/assets/ap-tree-cap-sources.json` drops back to
/// being the ceiling this can be checked against.
///
/// So the only open question is whether cap slots are in here at all. The first
/// run could not say: it printed the first 64 entries of 360 and every one of
/// them was an `ap_tree_atk` bonus, which is a truncated sample, NOT an absence.
/// This version answers it over all 400 — a histogram of every type id seen
/// (small, complete) plus the full detail of only the cap-typed slots (few).
///
/// Guarded reads throughout. One line per player load.
#[cfg(feature = "hookdiag")]
fn log_record_node_array(record: *const usize, skillboard: &[u32]) {
    use crate::hooks::diag::read_u32_guarded;

    /// `limit_bonus_param` type ids for the three per-class damage caps and the
    /// all-class one. 107 is the HEALING cap and is deliberately not here.
    const CAP_TYPE_IDS: [u32; 4] = [103, 104, 105, 106];
    /// Param slots per entry, and the words each slot spans.
    const PARAM_SLOTS: usize = 3;
    const SLOT_WORDS: usize = 4;

    let base = record as usize;
    let mut used = 0usize;
    // type id -> how many slots carry it. A histogram is what makes this
    // complete over all 400 without dumping 400 lines.
    let mut types: Vec<(u32, u32)> = Vec::new();
    let mut caps: Vec<String> = Vec::new();

    for n in 0..RECORD_NODE_ARRAY_COUNT {
        let entry = RECORD_NODE_ARRAY_OFFSET + n * RECORD_NODE_ARRAY_STRIDE;
        let id = read_u32_guarded(base, entry);
        if id == 0 || id == EMPTY_SIGIL_HASH {
            continue;
        }
        used += 1;
        let unlock = read_u32_guarded(base, entry + 4);

        for slot in 0..PARAM_SLOTS {
            let at = entry + (2 + slot * SLOT_WORDS) * 4;
            let param = read_u32_guarded(base, at);
            if param == 0 || param == EMPTY_SIGIL_HASH {
                continue;
            }
            let type_id = read_u32_guarded(base, at + 8);
            match types.iter_mut().find(|(id, _)| *id == type_id) {
                Some((_, count)) => *count += 1,
                None => types.push((type_id, 1)),
            }
            if CAP_TYPE_IDS.contains(&type_id) {
                caps.push(format!(
                    "{id:#010x}/{unlock:#06x} p={param:#010x} t={type_id} v={}",
                    f32::from_bits(read_u32_guarded(base, at + 12))
                ));
            }
        }
    }

    if used == 0 {
        log::info!("APDIAG record={base:#x} array empty (record not loaded yet)");
        return;
    }
    types.sort_unstable();
    log::info!(
        "APDIAG record={base:#x} used={used}/{RECORD_NODE_ARRAY_COUNT} sbNodes={} capSlots={} \
         types=[{}] caps=[{}]",
        skillboard.len(),
        caps.len(),
        types
            .iter()
            .map(|(id, count)| format!("{id}x{count}"))
            .collect::<Vec<_>>()
            .join(" "),
        caps.join(" ")
    );
}

/// hookdiag: resolve a save-instance id through the FNV-1a(u32) instance map
/// inside `*(DAT_147c24980)`: end node @ +0x100, buckets @ +0x110, u64 mask
/// @ +0x128 (hash = FNV-1a over the id's 4 bytes, per the dispatcher
/// decompile at FUN_140a23e70). The matched node's value is a vector
/// (base @ node+0x18, end @ node+0x20) of 8-byte {u32,u32} pairs — dump the
/// first 16 pairs to expose the weapon/equipment progression state.
#[cfg(feature = "hookdiag")]
fn log_instance_resolve(mgr: usize, id: u32, src_off: usize) {
    use crate::hooks::diag::{read_ptr_guarded, read_u32_guarded, read_u64_guarded};

    const PRIME: u64 = 0x100000001B3;
    let mut h: u64 = 0xCBF29CE484222325;
    for b in id.to_le_bytes() {
        h = (h ^ b as u64).wrapping_mul(PRIME);
    }
    let mask = read_u64_guarded(mgr, 0x128).unwrap_or(0);
    let (Some(buckets), Some(end_node)) =
        (read_ptr_guarded(mgr, 0x110), read_ptr_guarded(mgr, 0x100))
    else {
        return;
    };
    let bucket = ((h & mask) as usize) * 0x10;
    let (Some(mut node), Some(bucket_head)) = (
        read_ptr_guarded(buckets, bucket + 8),
        read_ptr_guarded(buckets, bucket),
    ) else {
        return;
    };
    for _ in 0..16 {
        if node == 0 || node == end_node {
            break;
        }
        if read_u32_guarded(node, 0x10) == id {
            let (Some(arr), Some(arr_end)) =
                (read_ptr_guarded(node, 0x18), read_ptr_guarded(node, 0x20))
            else {
                break;
            };
            let count = arr_end.saturating_sub(arr) / 8;
            let pairs: Vec<String> = (0..count.min(16))
                .map(|i| {
                    format!(
                        "{:08x}/{:08x}",
                        read_u32_guarded(arr, i * 8),
                        read_u32_guarded(arr, i * 8 + 4)
                    )
                })
                .collect();
            log::info!(
                "WPDIAG inst src+{src_off:#x} id={id:#010x} n={count}: {}",
                pairs.join(" ")
            );
            return;
        }
        if node == bucket_head {
            break;
        }
        let Some(next) = read_ptr_guarded(node, 8) else {
            break;
        };
        node = next;
    }
    log::info!("WPDIAG inst src+{src_off:#x} id={id:#010x} UNRESOLVED");
}

/// hookdiag: skillboard (mastery node) probe. Ghidra (record dispatcher
/// FUN_140a23e70, in-quest cases 1/2): the profile blob at `record+0x5DB8`
/// carries the player's skillboard state as **50 u32 node ids at blob+0x74..
/// 0x13C** (`blob[0x1d..0x4f]`, loop bound 0x32); each id is looked up in the
/// CharaPower map @ `DAT_147c24a78+0x330/0x348` and applied as an effect
/// (`value+0x18 -> row`, effect id @ row+0x74, category 4). The ids should be
/// `skillboard_layout.tbl` Keys for the record's character (verify against
/// the in-game skillboard screen; names via skillboard_effect ->
/// TXT_SB_NAME_PL####_SP###). Blob header for orientation: +0 charid,
/// +4 -> record+0x5B18, +0x10.. 13 sigil instance keys (-> record+0x5AC0),
/// +0x44.. 4 u32s (-> record+0x5AF4), +0x54 -> record+0x5B04.
/// Guarded reads; first 3 dumps per blob pointer, plus a periodic override
/// (every 512th call) so post-load state is captured even if the early dumps
/// land before save data populates.
#[cfg(feature = "hookdiag")]
fn log_skillboard_probe(record: *const usize) {
    use crate::hooks::diag::{first_n_per_key, read_ptr_guarded, read_u32_guarded};
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEEN: std::sync::Mutex<Vec<(usize, u32)>> = std::sync::Mutex::new(Vec::new());
    static CALLS: AtomicU32 = AtomicU32::new(0);

    let base = record as usize;
    let Some(blob) = read_ptr_guarded(base, 0x5DB8) else {
        return;
    };
    if blob == 0 {
        return;
    }
    let periodic = CALLS.fetch_add(1, Ordering::Relaxed) % 512 == 0;
    if !first_n_per_key(&SEEN, blob, 3) && !periodic {
        return;
    }

    let mode = read_u32_guarded(base, 0x5EAC);
    let head: Vec<String> = (0..0x1D_usize)
        .map(|i| format!("{:08x}", read_u32_guarded(blob, i * 4)))
        .collect();
    log::info!(
        "SBDIAG record={base:#x} mode={mode} blob={blob:#x} head@0..74: {}",
        head.join(" ")
    );
    let nodes: Vec<String> = (0..0x32_usize)
        .map(|i| format!("{:08x}", read_u32_guarded(blob, 0x74 + i * 4)))
        .collect();
    log::info!("SBDIAG blob={blob:#x} nodes@74..13C: {}", nodes.join(" "));
}

/// hookdiag: weapon probe (WPDIAG). Ghidra (2026-07-17 session): the weapon
/// save data lives in the save aggregate `*(DAT_147c24980)`:
/// - charid-keyed hashmap @ +0x129B08 (end node) / +0x129B18 (buckets) /
///   +0x129B30 (mask) — value node+0x18 points at a per-character array of
///   **15 x 0x190-byte weapon records**: weapon id @ +0x60 (empty sentinel
///   0x887AE0B0), stat block @ +0x70..0xB0 (level/exp/uncap candidates),
///   5 u32s @ +0xB0..0xC4 (wrightstone id + trait ids, matched against the
///   8-column table at DAT_147c24af8+0x370).
/// - `FUN_143cc5da0` copies the active weapon's state into
///   `*(record+0x5E80) + 0x50` (a ~0xAC-byte change-tracked struct), i.e. the
///   player record carries an equipped-weapon blob pointer at +0x5E80, right
///   after the identity snapshot (+0x5E60).
/// Dumps both: the record blob (header + weapon state) and, for the record's
/// charid (@ +0x5B10), every populated save entry. Guarded reads; 3 dumps per
/// blob pointer plus a periodic override.
#[cfg(feature = "hookdiag")]
fn log_weapon_probe(record: *const usize) {
    use crate::hooks::diag::{first_n_per_key, read_ptr_guarded, read_u32_guarded, MODULE_BASE};
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEEN: std::sync::Mutex<Vec<(usize, u32)>> = std::sync::Mutex::new(Vec::new());
    static CALLS: AtomicU32 = AtomicU32::new(0);

    const EMPTY_SENTINEL: u32 = 0x887AE0B0;

    let base = record as usize;
    let periodic = CALLS.fetch_add(1, Ordering::Relaxed) % 128 == 0;

    // Pointer survey: log the record's blob pointers even when NULL, so a
    // silent probe distinguishes "blob never allocated" from "read failed".
    // Periodic, not quota'd (see PART1B note below).
    static PTR_TICK: AtomicU32 = AtomicU32::new(0);
    if PTR_TICK.fetch_add(1, Ordering::Relaxed) % 256 == 0 {
        let p5db8 = read_ptr_guarded(base, 0x5DB8).unwrap_or(usize::MAX);
        let p5e60 = read_ptr_guarded(base, 0x5E60).unwrap_or(usize::MAX);
        let p5e80 = read_ptr_guarded(base, 0x5E80).unwrap_or(usize::MAX);
        let p5ea0 = read_ptr_guarded(base, 0x5EA0).unwrap_or(usize::MAX);
        let mode = read_u32_guarded(base, 0x5EAC);
        log::info!(
            "WPDIAG record={base:#x} mode={mode} ptrs 5DB8={p5db8:#x} 5E60={p5e60:#x} 5E80={p5e80:#x} 5EA0={p5ea0:#x}"
        );
    }

    // Part 1b: the record's instance-key neighborhood. +0x5AC0 holds the 13
    // sigil instance keys; the 4 u32s at +0x5AF4..0x5B04 (also copied to
    // snapshot+0x1D4) are unknowns — prime candidates for the equipped
    // weapon/wrightstone instance references. Dump record+0x5AF0..0x5B40 and
    // the snapshot tail +0x1D4..0x250.
    // No consumable quotas during investigation: boot-time refreshes of
    // empty records were eating the per-key fire budget and each wasted
    // capture costs a full game relaunch. Sample continuously instead —
    // whenever the record has loaded data (sigil keys present), on a light
    // periodic stride.
    let keys_loaded = read_u32_guarded(base, 0x5AC0) != 0;
    static PART1B: AtomicU32 = AtomicU32::new(0);
    if keys_loaded && PART1B.fetch_add(1, Ordering::Relaxed) % 64 == 0 {
        // Extended to +0x5B70: the dispatcher uses record+0x5B60 as the
        // per-character MASTER LEVEL input and +0x5B44 as another gated
        // input; +0x5B2C/+0x5B64..6C receive derived values.
        let neigh: Vec<String> = (0x5AF0..0x5B70_usize)
            .step_by(4)
            .map(|off| format!("{:08x}", read_u32_guarded(base, off)))
            .collect();
        log::info!(
            "WPDIAG record={base:#x} keys@5AF0..5B70: {}",
            neigh.join(" ")
        );
        if let Some(snap) = read_ptr_guarded(base, 0x5E60) {
            if snap != 0 {
                let tail: Vec<String> = (0x1D4..0x250_usize)
                    .step_by(4)
                    .map(|off| format!("{:08x}", read_u32_guarded(snap, off)))
                    .collect();
                log::info!("WPDIAG snap={snap:#x} tail@1D4..250: {}", tail.join(" "));
            }
        }
    }

    // Part 1: the record's equipped-weapon blob @ +0x5E80.
    if let Some(blob) = read_ptr_guarded(base, 0x5E80) {
        if blob != 0 && (first_n_per_key(&SEEN, blob, 3) || periodic) {
            let mode = read_u32_guarded(base, 0x5EAC);
            let head: Vec<String> = (0..0x14_usize)
                .map(|i| format!("{:08x}", read_u32_guarded(blob, i * 4)))
                .collect();
            log::info!(
                "WPDIAG record={base:#x} mode={mode} blob={blob:#x} head@0..50: {}",
                head.join(" ")
            );
            let state: Vec<String> = (0..0x2C_usize)
                .map(|i| format!("{:08x}", read_u32_guarded(blob, 0x50 + i * 4)))
                .collect();
            log::info!("WPDIAG blob={blob:#x} state@50..100: {}", state.join(" "));
            // FUN_140a31030: blob+0x120 is handed to the save root, and
            // blob+0x5ED8 / +0x5F18 receive computed stats (fed by the
            // master-level value at record+0x5B60) — player-attribute
            // candidates (HP/ATK etc.).
            let sect: Vec<String> = (0..0x10_usize)
                .map(|i| format!("{:08x}", read_u32_guarded(blob, 0x120 + i * 4)))
                .collect();
            log::info!("WPDIAG blob={blob:#x} sect@120..160: {}", sect.join(" "));
            let stats: Vec<String> = (0..0x20_usize)
                .map(|i| format!("{:08x}", read_u32_guarded(blob, 0x5ED8 + i * 4)))
                .collect();
            log::info!(
                "WPDIAG blob={blob:#x} stats@5ED8..5F58: {}",
                stats.join(" ")
            );
        }
    }

    // Part 2: the per-character weapon save array in the save aggregate.
    if !periodic {
        return;
    }
    let module = MODULE_BASE.load(Ordering::Relaxed);
    if module == 0 {
        return;
    }

    // Part 3: the WeaponIdSaveList inline entries in the summon-store
    // aggregate: *(DAT_147c23f48)+0xD378, 512 x 0xC entries (ctor fills key
    // with the 0x887AE0B0 sentinel). Now that the save-map keys are known to
    // be charids, a populated entry {charid, weapon_id, extra} would be the
    // per-character equipped weapon. One dump per periodic fire.
    static PART3: AtomicU32 = AtomicU32::new(0);
    if PART3.fetch_add(1, Ordering::Relaxed) % 4 == 0 {
        // Stale 2.0.2-era literal — see the note on the same global above.
        if let Some(agg) = read_ptr_guarded(module, 0x7c23f48) {
            if agg != 0 {
                let mut lines = 0usize;
                for i in 0..512usize {
                    let e = 0xD378 + i * 0xC;
                    let key = read_u32_guarded(agg, e);
                    if key == 0 || key == EMPTY_SENTINEL {
                        continue;
                    }
                    let a = read_u32_guarded(agg, e + 4);
                    let b = read_u32_guarded(agg, e + 8);
                    log::info!("WPDIAG wsave[{i}] key={key:#010x} a={a:#010x} b={b:#010x}");
                    lines += 1;
                    if lines >= 48 {
                        log::info!("WPDIAG wsave truncated at 48 entries");
                        break;
                    }
                }
                log::info!("WPDIAG wsave agg={agg:#x} populated~{lines}");
            }
        }
    }
    let Some(mgr) = read_ptr_guarded(module, SAVE_ROOT_RVA) else {
        return;
    };
    if mgr == 0 {
        return;
    }
    let charid = read_u32_guarded(base, 0x5B10);
    if charid == 0 || charid == EMPTY_SENTINEL {
        return;
    }
    let mask = read_u32_guarded(mgr, 0x129B30);
    let Some(buckets) = read_ptr_guarded(mgr, 0x129B18) else {
        return;
    };
    let Some(end_node) = read_ptr_guarded(mgr, 0x129B08) else {
        return;
    };
    let bucket = (mask & charid) as usize * 0x10;
    let Some(mut node) = read_ptr_guarded(buckets, bucket + 8) else {
        return;
    };
    let Some(bucket_head) = read_ptr_guarded(buckets, bucket) else {
        return;
    };
    let mut found = 0usize;
    for _ in 0..16 {
        if node == 0 || node == end_node {
            break;
        }
        if read_u32_guarded(node, 0x10) == charid {
            let Some(arr) = read_ptr_guarded(node, 0x18) else {
                break;
            };
            if arr == 0 {
                break;
            }
            for w in 0..15usize {
                let e = w * 0x190;
                let id = read_u32_guarded(arr, e + 0x60);
                if id == 0 || id == EMPTY_SENTINEL {
                    continue;
                }
                found += 1;
                let row: Vec<String> = (0x60..0xC8_usize)
                    .step_by(4)
                    .map(|off| format!("{:08x}", read_u32_guarded(arr, e + off)))
                    .collect();
                log::info!(
                    "WPDIAG charid={charid:#010x} wpn[{w}]@60..C8: {}",
                    row.join(" ")
                );
            }
            // 07-17 live run: arrays resolved but read all-empty at +0x60.
            // Discriminate wrong-offset vs truly-empty: dump slot 0 raw and
            // count nonzero dwords across a wide window (2x the 15*0x190
            // array, in case the real stride/base differs).
            let raw0: Vec<String> = (0..0x64_usize)
                .map(|i| format!("{:08x}", read_u32_guarded(arr, i * 4)))
                .collect();
            log::info!(
                "WPDIAG charid={charid:#010x} arr[0]@0..190: {}",
                raw0.join(" ")
            );
            let nonzero = (0..0x2EE0_usize)
                .step_by(4)
                .filter(|&off| read_u32_guarded(arr, off) != 0)
                .count();
            log::info!(
                "WPDIAG charid={charid:#010x} arr={arr:#x} populated={found}/15 nonzero_dwords@0..2EE0={nonzero}"
            );
            break;
        }
        if node == bucket_head {
            break;
        }
        let Some(next) = read_ptr_guarded(node, 8) else {
            break;
        };
        node = next;
    }
}

/// hookdiag: dump the loadout-struct regions suspected to hold the expansion
/// equipment, for live verification against the in-game equip screens.
///
/// Ghidra evidence (v2.0.2 analyzed DB): the loadout struct `*(record+0x5DC8)`
/// is initialized by FUN_1407a0be0, which sets FOUR 0x10-byte entries at
/// +0x33EC..+0x342C to the empty-sigil sentinel (`{u32,u32,u32,u8,u8}` — the
/// suspected 4 equipped summons: kind/main-trait/equip-bonus/levels), and the
/// serializer FUN_1407a1dc0 walks 400 x 0x20-byte entries at +0x8..+0x3208
/// (`{u32 effect_id, u8 rank}` — the suspected skillboard/master-trait nodes),
/// matching them against the per-character skillboard node list keyed by the
/// charid hash at +0x3518. +0x3208..0x32A4 is an id/flag + stat block (weapon /
/// overmastery candidates). All reads guarded; rate-limited per loadout pointer.
#[cfg(feature = "hookdiag")]
fn log_loadout_probe(record: *const usize) {
    use crate::hooks::diag::{first_n_per_key, read_ptr_guarded, read_u32_guarded};

    static SEEN: std::sync::Mutex<Vec<(usize, u32)>> = std::sync::Mutex::new(Vec::new());

    let base = record as usize;
    let Some(loadout) = read_ptr_guarded(base, 0x5DC8) else {
        return;
    };
    if loadout == 0 {
        // In-quest records (mode 0) may carry no loadout — log the pointer
        // neighborhood ONCE per record instead so the mode-0 data path (instance
        // key array @ +0x5DD0 and its neighbors) can be mapped live.
        if first_n_per_key(&SEEN, base, 1) {
            let mode = read_u32_guarded(base, 0x5EAC);
            let ptrs: Vec<String> = (0x5DC8..0x5E48)
                .step_by(8)
                .map(|off| format!("{:x}", read_ptr_guarded(base, off).unwrap_or(0)))
                .collect();
            log::info!(
                "LODIAG record={base:#x} mode={mode} loadout=NULL ptrs@5DC8..5E48: {}",
                ptrs.join(" ")
            );
            if let Some(keys) = read_ptr_guarded(base, 0x5DD0) {
                if keys != 0 {
                    let ks: Vec<String> = (0..16usize)
                        .map(|i| format!("{:08x}", read_u32_guarded(keys, i * 4)))
                        .collect();
                    log::info!("LODIAG record={base:#x} keys@5DD0: {}", ks.join(" "));
                }
            }
        }
        return;
    }
    if !first_n_per_key(&SEEN, loadout, 3) {
        return;
    }

    let charid = read_u32_guarded(loadout, 0x3518);
    let party = read_u32_guarded(loadout, 0x3514);
    log::info!(
        "LODIAG loadout={loadout:#x} record={base:#x} charid@3518={charid:#010x} party@3514={party:#x}"
    );

    // 4 id/flag pairs at +0x3208 (flags are uncap-coded 0..=9 by the serializer).
    for i in 0..4usize {
        let id = read_u32_guarded(loadout, 0x3208 + i * 8);
        let flags = read_u32_guarded(loadout, 0x320C + i * 8);
        log::info!(
            "LODIAG pair[{i}]@{:#x} id={id:#010x} flags={flags:#010x}",
            0x3208 + i * 8
        );
    }

    // Stat/equip block between the id pairs and the sigil array.
    let stat: Vec<String> = (0x3228..0x32A4)
        .step_by(4)
        .map(|off| format!("{:08x}", read_u32_guarded(loadout, off)))
        .collect();
    log::info!("LODIAG stat@3228..32A4: {}", stat.join(" "));

    // The four dwords just before the summon entries (ctor inits them from a
    // float constant) plus the 4 suspected equipped-summon entries.
    let floats: Vec<String> = (0..4usize)
        .map(|i| format!("{:08x}", read_u32_guarded(loadout, 0x33DC + i * 4)))
        .collect();
    log::info!("LODIAG f32s@33DC: {}", floats.join(" "));
    for i in 0..4usize {
        let e = 0x33EC + i * 0x10;
        let a = read_u32_guarded(loadout, e);
        let b = read_u32_guarded(loadout, e + 4);
        let c = read_u32_guarded(loadout, e + 8);
        let lv = read_u32_guarded(loadout, e + 0xC);
        log::info!("LODIAG summon[{i}]@{e:#x} a={a:#010x} b={b:#010x} c={c:#010x} lv={lv:#010x}");
    }

    // Skillboard entries: 400 x 0x20 at +0x8. Count populated, log the first 12.
    let mut populated = 0u32;
    let mut logged = 0u32;
    for i in 0..400usize {
        let e = 0x8 + i * 0x20;
        let id = read_u32_guarded(loadout, e);
        if id == 0 || id == EMPTY_SIGIL_HASH {
            continue;
        }
        populated += 1;
        if logged < 12 {
            let v0 = read_u32_guarded(loadout, e + 4);
            let v1 = read_u32_guarded(loadout, e + 8);
            let v2 = read_u32_guarded(loadout, e + 0xC);
            log::info!("LODIAG sb[{i}] id={id:#010x} +4={v0:#010x} +8={v1:#010x} +c={v2:#010x}");
            logged += 1;
        }
    }
    log::info!("LODIAG sb populated={populated}/400");

    // Unmapped regions, for master-level / summon-level hunting.
    let tail1: Vec<String> = (0x3430..0x3470)
        .step_by(4)
        .map(|off| format!("{:08x}", read_u32_guarded(loadout, off)))
        .collect();
    log::info!("LODIAG tail@3430: {}", tail1.join(" "));
    let tail2: Vec<String> = (0x3508..0x3574)
        .step_by(4)
        .map(|off| format!("{:08x}", read_u32_guarded(loadout, off)))
        .collect();
    log::info!("LODIAG tail@3508: {}", tail2.join(" "));
}

#[cfg(test)]
mod tests {
    use super::*;

    // The exact "snapshot pointer" value from the 2026-07-18 in-quest crash dump:
    // the slot at record+PLAYER_IDENTITY_OFFSET held two adjacent u32s
    // (0x5b0/0x5b1), not a pointer, because that actor carries no identity record
    // at ACTOR_RECORD_OFFSET. Unmapped in any normal process.
    const CRASH_SNAPSHOT_PTR: usize = 0x0000_05b1_0000_05b0;

    #[test]
    fn read_player_identity_rejects_unmapped_snapshot() {
        let identity = unsafe { read_player_identity(CRASH_SNAPSHOT_PTR as *const u8) };
        assert!(identity.is_none());
    }

    // The record layout from the 2026-07-18 WSDIAG3 in-quest dumps (records
    // 0x0d21b430 / 0x9b15cfb1 / 0xdd7a151e / 0x627bcb0d): innate ids at
    // +0x94, a zero word at +0xF0, and the id-repeating {id, level} pair
    // array at +0xF4 carrying the live-confirmed levels 32/22/12/1.
    fn innate_buffer(pair_ids: [u32; 4]) -> Vec<u8> {
        let ids = [
            0x1e1cecce_u32,
            0xa8a3163b,
            0xdc584f60,
            0x57e8a93f,
            EMPTY_SIGIL_HASH,
        ];
        let levels = [32_u32, 22, 12, 1];
        let mut buf = vec![0u8; 0x200];
        for (i, id) in ids.iter().enumerate() {
            buf[0x94 + i * 4..0x94 + i * 4 + 4].copy_from_slice(&id.to_le_bytes());
        }
        for (i, (id, level)) in pair_ids.iter().zip(levels).enumerate() {
            buf[0xF4 + i * 8..0xF4 + i * 8 + 4].copy_from_slice(&id.to_le_bytes());
            buf[0xF4 + i * 8 + 4..0xF4 + i * 8 + 8].copy_from_slice(&level.to_le_bytes());
        }
        buf
    }

    #[test]
    fn read_innate_traits_takes_levels_from_the_matching_pair_array() {
        let buf = innate_buffer([0x1e1cecce, 0xa8a3163b, 0xdc584f60, 0x57e8a93f]);
        let traits = read_innate_traits(buf.as_ptr() as usize, 0x94);
        let pairs: Vec<(u32, u32)> = traits.iter().map(|t| (t.id, t.level)).collect();
        assert_eq!(
            pairs,
            vec![
                (0x1e1cecce, 32),
                (0xa8a3163b, 22),
                (0xdc584f60, 12),
                (0x57e8a93f, 1)
            ]
        );
    }

    /// A full-size fake record whose resolved limit-bonus store holds the given
    /// entries. Full size matters: the reader walks all 400 entry slots, so a
    /// short buffer would have it reading neighbouring heap memory as entries.
    fn limit_bonus_record(entries: &[(u32, &[(u32, u32, f32)])]) -> Vec<u8> {
        let mut buf = vec![
            0u8;
            RECORD_NODE_ARRAY_OFFSET
                + RECORD_NODE_ARRAY_COUNT * RECORD_NODE_ARRAY_STRIDE
        ];
        for (n, (id, slots)) in entries.iter().enumerate() {
            let entry = RECORD_NODE_ARRAY_OFFSET + n * RECORD_NODE_ARRAY_STRIDE;
            buf[entry..entry + 4].copy_from_slice(&id.to_le_bytes());
            for (slot, (param, type_id, value)) in slots.iter().enumerate() {
                let at = entry + (2 + slot * 4) * 4;
                buf[at..at + 4].copy_from_slice(&param.to_le_bytes());
                buf[at + 8..at + 12].copy_from_slice(&type_id.to_le_bytes());
                buf[at + 12..at + 16].copy_from_slice(&value.to_le_bytes());
            }
        }
        buf
    }

    #[test]
    fn read_record_limit_bonus_caps_sums_the_cap_typed_slots_per_class() {
        // Two entries; the "all classes" type 106 raises every class, the
        // healing cap 107 and the untyped ATK-style slot are ignored.
        let buf = limit_bonus_record(&[
            (0x11111111, &[(0xA1, 103, 2.0), (0xA2, 104, 3.0)]),
            (
                0x22222222,
                &[(0xA3, 105, 5.0), (0xA4, 106, 1.5), (0xA5, 107, 9.0)],
            ),
            (0x33333333, &[(0xA6, 1, 40.0)]),
        ]);
        assert_eq!(
            read_record_limit_bonus_caps(buf.as_ptr() as usize),
            (Some(3.5), Some(4.5), Some(6.5))
        );
    }

    #[test]
    fn read_record_limit_bonus_caps_reads_none_from_an_empty_store() {
        // An empty array is a record that has not loaded yet, not an account
        // with nothing unlocked — it must read "unknown", never +0%.
        let buf = limit_bonus_record(&[]);
        assert_eq!(
            read_record_limit_bonus_caps(buf.as_ptr() as usize),
            (None, None, None)
        );
    }

    #[test]
    fn read_record_limit_bonus_caps_skips_sentinels_and_garbage_values() {
        // Sentinel entry ids and param keys are empty slots; a non-finite value
        // is a misread that must not poison the sum. A store that holds only
        // such entries still counts as loaded (used > 0), so the sums are a
        // genuine zero rather than unknown.
        let buf = limit_bonus_record(&[
            (EMPTY_SIGIL_HASH, &[(0xA1, 103, 50.0)]),
            (
                0x44444444,
                &[
                    (EMPTY_SIGIL_HASH, 103, 50.0),
                    (0, 104, 50.0),
                    (0xA2, 105, f32::NAN),
                ],
            ),
        ]);
        assert_eq!(
            read_record_limit_bonus_caps(buf.as_ptr() as usize),
            (Some(0.0), Some(0.0), Some(0.0))
        );
    }

    #[test]
    fn read_innate_traits_keeps_level_zero_when_pair_ids_do_not_match() {
        // Layout drift (pair array holding different ids) must never attach a
        // wrong level — ids still come through, levels fall back to 0.
        let buf = innate_buffer([0xdeadbeef, 0xdeadbeef, 0xdeadbeef, 0xdeadbeef]);
        let traits = read_innate_traits(buf.as_ptr() as usize, 0x94);
        assert_eq!(traits.len(), 4);
        assert!(traits.iter().all(|t| t.level == 0));
    }

    /// A minimal claimable identity; only the fields the donor tests assert on.
    fn identity(
        name: &str,
        party_index: u8,
        is_online: bool,
        master_level: u32,
    ) -> StoredPlayerIdentity {
        StoredPlayerIdentity {
            character_name: CString::new("").unwrap(),
            display_name: CString::new(name).unwrap(),
            party_index,
            is_online,
            sigils: Vec::new(),
            summons: Vec::new(),
            overmasteries: Vec::new(),
            player_level: 0,
            abilities: Vec::new(),
            weapon_key: String::new(),
            master_level,
            skillboard: Vec::new(),
            stats: None,
            weapon_state: None,
            cap_ups: (None, None, None),
            limit_bonus_caps: (None, None, None),
        }
    }

    // The 2026-07-18 "Tweyen wearing Maglielle's build" bug (installed-app log
    // 238): the local town-roster AI Maglielle record (id 0x25d46f4b) claimed
    // party slot 3 while the remote player "Sylmorn" (record id 0x718e1a14)
    // occupied that slot in the lobby. Enrichment joined on slot alone, so
    // Sylmorn's meter row shipped with Maglielle's weapon/abilities/master-level.
    const TOWN_AI_ID: u32 = 0x25d4_6f4b;
    const REMOTE_PLAYER_ID: u32 = 0x718e_1a14;

    // Two players on the SAME character share the character-scoped record id at
    // +0x5EA8 (live-observed: "both Maglielles carried ... id 0x25d46f4b"), so an
    // id-keyed donor join returned one player's build for both. The store keys on
    // (party slot, online) instead — the two players always occupy distinct slots.
    #[test]
    fn same_character_players_each_resolve_their_own_build() {
        let mut store = IdentityStore::default();
        // Slot 0 and slot 1, same character => identical record id.
        store.claim(TOWN_AI_ID, identity("A", 0, false, 55));
        store.claim(TOWN_AI_ID, identity("B", 1, false, 12));

        assert_eq!(
            store
                .equipment_donor(0, false)
                .expect("slot 0 must resolve its own build")
                .master_level,
            55
        );
        assert_eq!(
            store
                .equipment_donor(1, false)
                .expect("slot 1 must resolve its own build")
                .master_level,
            12
        );
    }

    // The claim for one same-character sibling must not evict the other's slot.
    #[test]
    fn claiming_a_same_id_sibling_slot_keeps_the_first() {
        let mut store = IdentityStore::default();
        store.claim(TOWN_AI_ID, identity("A", 0, false, 55));
        store.claim(TOWN_AI_ID, identity("B", 1, false, 12));

        assert!(store.equipment_donor(0, false).is_some());
    }

    // Anti-contamination (the original id-join's purpose), preserved: a local AI
    // town record and a remote player can occupy the same party slot; the online
    // flag separates them so neither is dressed in the other's build.
    #[test]
    fn remote_player_and_local_ai_on_one_slot_keep_their_own_builds() {
        let mut store = IdentityStore::default();
        store.claim(REMOTE_PLAYER_ID, identity("Sylmorn", 3, true, 42));
        // The town AI record refreshes later on the same slot (latest claim wins
        // for its own online bucket — but it must not dress the remote player).
        store.claim(TOWN_AI_ID, identity("", 3, false, 1));

        assert_eq!(
            store
                .equipment_donor(3, true)
                .expect("the remote player resolves its own claim")
                .master_level,
            42
        );
        assert_eq!(
            store
                .equipment_donor(3, false)
                .expect("the AI companion resolves its own claim")
                .master_level,
            1
        );
    }

    #[test]
    fn equipment_donor_returns_none_for_an_unclaimed_slot() {
        let mut store = IdentityStore::default();
        store.claim(TOWN_AI_ID, identity("", 3, false, 1));

        assert!(store.equipment_donor(0, false).is_none());
        // Same slot, wrong online-ness is also a miss.
        assert!(store.equipment_donor(3, true).is_none());
    }

    // The legacy fallback path (actor with no embedded record) still resolves by
    // the raw record id.
    #[test]
    fn find_by_id_resolves_the_legacy_fallback() {
        let mut store = IdentityStore::default();
        store.claim(REMOTE_PLAYER_ID, identity("Sylmorn", 3, true, 42));

        assert_eq!(
            store
                .find_by_id(REMOTE_PLAYER_ID)
                .expect("the fallback resolves by record id")
                .master_level,
            42
        );
        assert!(store.find_by_id(0xDEAD_BEEF).is_none());
    }

    #[test]
    fn read_player_identity_accepts_readable_snapshot() {
        // Heap-allocated zeroed snapshot with just the fields a resolvable
        // identity needs: an inline display name ("Gran"; max_size <= 0xf takes
        // the inline-buffer path). Zeroed sigil slots are filtered, zeroed
        // is_online/party_index pass the bounds checks.
        let mut snapshot = vec![0u8; std::mem::size_of::<SigilList>()];
        snapshot[0x208..0x20C].copy_from_slice(b"Gran");
        snapshot[0x218..0x220].copy_from_slice(&4usize.to_le_bytes()); // used_size
        snapshot[0x220..0x228].copy_from_slice(&0xfusize.to_le_bytes()); // max_size

        let identity = unsafe { read_player_identity(snapshot.as_ptr()) }
            .expect("a readable snapshot with a name must still resolve");
        assert_eq!(identity.display_name.as_bytes(), b"Gran");
        assert_eq!(identity.party_index, 0);
        assert!(!identity.is_online);
        assert!(identity.sigils.is_empty());
    }

    // `snapshot_party_index` is the allocation-free half of
    // `read_player_identity` — the only part the slot-key path needs. It must
    // accept and reject exactly what the full read does, or attribution would
    // differ between the fast and slow paths.
    #[test]
    fn snapshot_party_index_rejects_the_unmapped_snapshot() {
        assert_eq!(
            unsafe { snapshot_party_index(CRASH_SNAPSHOT_PTR as *const u8) },
            None
        );
    }

    #[test]
    fn snapshot_party_index_matches_the_full_identity_read() {
        let mut snapshot = vec![0u8; std::mem::size_of::<SigilList>()];
        snapshot[0x208..0x20C].copy_from_slice(b"Gran");
        snapshot[0x218..0x220].copy_from_slice(&4usize.to_le_bytes()); // used_size
        snapshot[0x220..0x228].copy_from_slice(&0xfusize.to_le_bytes()); // max_size
        snapshot[0x22C..0x230].copy_from_slice(&2u32.to_le_bytes()); // party_index

        let full = unsafe { read_player_identity(snapshot.as_ptr()) }
            .expect("the full read resolves this snapshot");
        assert_eq!(
            unsafe { snapshot_party_index(snapshot.as_ptr()) },
            Some(full.party_index)
        );
        assert_eq!(full.party_index, 2);
    }

    #[test]
    fn snapshot_party_index_rejects_a_nameless_snapshot() {
        // An empty display name means the snapshot is not a resolvable identity
        // yet. The full read turns it away, so this must too — accepting it
        // would hand out a party slot key for an actor that has no identity.
        let mut snapshot = vec![0u8; std::mem::size_of::<SigilList>()];
        snapshot[0x220..0x228].copy_from_slice(&0xfusize.to_le_bytes()); // max_size

        assert!(unsafe { read_player_identity(snapshot.as_ptr()) }.is_none());
        assert_eq!(unsafe { snapshot_party_index(snapshot.as_ptr()) }, None);
    }

    #[test]
    fn snapshot_party_index_rejects_an_out_of_range_party_slot() {
        let mut snapshot = vec![0u8; std::mem::size_of::<SigilList>()];
        snapshot[0x208..0x20C].copy_from_slice(b"Gran");
        snapshot[0x218..0x220].copy_from_slice(&4usize.to_le_bytes());
        snapshot[0x220..0x228].copy_from_slice(&0xfusize.to_le_bytes());
        snapshot[0x22C..0x230].copy_from_slice(&9u32.to_le_bytes()); // not a party slot

        assert!(unsafe { read_player_identity(snapshot.as_ptr()) }.is_none());
        assert_eq!(unsafe { snapshot_party_index(snapshot.as_ptr()) }, None);
    }

    // An AI companion's own embedded record carries the LOCAL profile's name (the
    // snapshot fills it from the local save), so the meter labelled all three
    // companions with the local player's name — "Manmoth (Zeta)" instead of
    // "AI (Zeta)". The claim path has always blanked those names; the embedded
    // record became the PRIMARY resolution path on 2026-07-18 and read the name
    // raw, so the blanking stopped reaching the event the meter renders.
    #[test]
    fn ai_companion_from_its_embedded_record_emits_a_blank_name() {
        let ai = identity("Manmoth", 2, false, 0);
        let event = embedded_identity_event(ai.clone(), ai, 0);

        assert_eq!(
            event.display_name.as_bytes(),
            b"",
            "an offline non-slot-0 record is an AI companion; the frontend keys \
             its \"(AI)\" marker off the empty name"
        );
        assert_eq!(event.character_name.as_bytes(), b"");
    }

    // Streamer-mode safety and the online case: a real player must never be
    // mislabelled as AI. Remote players are flagged online, the local player owns
    // slot 0 — both keep the name their record carries.
    #[test]
    fn real_players_from_their_embedded_record_keep_their_names() {
        let remote = identity("Sylmorn", 3, true, 0);
        let event = embedded_identity_event(remote.clone(), remote, 0);
        assert_eq!(event.display_name.as_bytes(), b"Sylmorn");

        let local = identity("Manmoth", 0, false, 0);
        let event = embedded_identity_event(local.clone(), local, 0);
        assert_eq!(event.display_name.as_bytes(), b"Manmoth");
    }
}
