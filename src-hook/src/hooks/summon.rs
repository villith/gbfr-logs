//! Source-parent resolution: maps a sub-entity, pet, avatar or summon damage
//! source to the player who owns it.
//!
//! Every arm dereferences across objects, so every read is guarded and a stale
//! offset fails closed rather than faulting the game thread.
//!
//! The summon class table, the extra arms and the vtable-RVA cross-check were
//! cross-referenced against an independent implementation (2026-07-24). The
//! offsets are facts about the game binary, not copied code.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::hooks::diag;
use crate::hooks::GetEntityHashID0x58;

/// Static RVAs of the summon-base body classes that store their summoner at
/// +0xFE8, sorted for `binary_search`. Compared against
/// `*(summon) - MODULE_BASE` — a plain read and compare, never a vfunc call on
/// a swept pointer. Goes stale on every game patch by design; a miss fails
/// closed and warns once.
/// v2.0.4 values, re-derived 2026-08-07. Each was recovered by walking MSVC RTTI
/// forward from the class name, which survives recompiles: the `.?AV<Class>@@`
/// string locates the TypeDescriptor, the Complete Object Locator carrying that
/// TD's RVA *at subobject offset 0* is the primary one (these classes each have
/// ~20 COLs, one per base of a deep multiple-inheritance hierarchy — only the
/// offset-0 subobject's vtable is what `*(summon)` yields), and the qword
/// pointing at that COL sits at vtable-8. Every entry was then round-tripped back
/// through the same walk to confirm the RVA yields the class it came from, so
/// these are read facts, not the +0x12A0 section shift applied arithmetically.
///
/// NOT in this list, deliberately: `So6300` (2.0.5 vtable 0x5C59AA0) and `So1a01`
/// (0x5E7F230). The RTTI walk finds them alongside the others and they are NOT new
/// 2.0.4 classes — they were already absent here. Whether they belong is a
/// question of whether they store their summoner at +0xFE8, which has not been
/// checked; adding them unverified would mis-attribute damage, so they stay out
/// until someone decompiles them.
const SUMMON_BASE_VTABLE_RVAS: &[usize] = &[
    0x59C3430, // BehaviorSummonObjectBase (generic/data-driven body)
    0x5C56530, // So0000  Lucilius
    0x5C57750, // So4e00  Albacore
    0x5C5A1C0, // So6400  Wheel of Fate
    0x5C5B370, // So0200  Rolan
    0x5C5C500, // So2001  Silverslime var.
    0x5C5E780, // So4502  Lilith var.
    0x5E72D60, // So4500  Lilith
    0x5E759A0, // So4c00  Managarmr Nihilla
    0x5E76B30, // So1d00  Quakadile
    0x5E77C20, // So9200  Beelzebub
    0x5E78DB0, // So0d00  Goblin Soldier
    0x5E79F40, // So4f00  Hope-Filled Skydwellers
    0x5E7B0F0, // So5600  Mellose Clan
    0x5E7C2A0, // So5700  Crew Alliance Rafale
    0x5E7D450, // So5f01  Cat var.
    0x617D090, // So1100  Goblin Warrior
    0x617D430, // So1100Base (generic body)
];

/// One-shot latch so a patch that moves these vtables logs once per session
/// rather than once per hit.
static SUMMON_VTABLE_RVA_WARNED: AtomicBool = AtomicBool::new(false);

fn warn_stale_rva_once(flag: &AtomicBool, what: &str) {
    if flag.swap(true, Ordering::Relaxed) {
        return;
    }
    log::warn!("{what} did not match — offsets may be stale after a game patch");
    #[cfg(feature = "console")]
    println!("WARNING: {what} did not match — offsets may be stale after a game patch");
}

/// True iff `summon`'s vtable RVA is a known summon-base body class. All reads
/// guarded, fails closed, no vfunc call.
fn is_summon_base_vtable(summon: *const usize) -> bool {
    let base = diag::MODULE_BASE.load(Ordering::Relaxed);
    if base == 0 {
        return false;
    }
    let Some(vtable) = diag::read_ptr_guarded(summon as usize, 0) else {
        return false;
    };
    let Some(rva) = vtable.checked_sub(base) else {
        return false;
    };
    SUMMON_BASE_VTABLE_RVAS.binary_search(&rva).is_ok()
}

/// The owner is stored as an entity-handle triple `{idx, CEntityInfo*, serial}`.
/// The handle can be legitimately empty, so require a non-zero index before
/// trusting the pointer. Returns the pointer offset to follow, or `None`.
#[inline(always)]
pub(crate) fn gated_parent_offset(
    source: *const usize,
    idx_offset: usize,
    ptr_offset: usize,
) -> Option<usize> {
    // read_u32_guarded returns 0 both for a failed read and an empty handle;
    // both must fail closed, so one check covers them.
    match diag::read_u32_guarded(source as usize, idx_offset) {
        0 => None,
        _ => Some(ptr_offset),
    }
}

/// Reads and validates the vfunc slot at `offset`, returning the function
/// pointer. Closes the wild-call window when a parent offset goes stale.
///
/// A caller invoking the vfunc on a resolved pointer MUST call through this
/// returned pointer and never re-walk the vtable raw: the guard and a raw
/// re-walk are two reads with a gap, and on a recycled allocation that gap is
/// the probe-crash class of bug.
pub(crate) fn validated_vfunc(instance: *const usize, offset: usize) -> Option<*const usize> {
    let vtable = diag::read_ptr_guarded(instance as usize, 0)?;
    if vtable == 0 {
        return None;
    }
    match diag::read_ptr_guarded(vtable, offset) {
        Some(slot) if slot != 0 => Some(slot as *const usize),
        _ => None,
    }
}

/// Discard-the-pointer form of [`validated_vfunc`] for call sites that only
/// need to know the slot is safe to walk.
pub(crate) fn vfunc_slot_readable(instance: *const usize, offset: usize) -> bool {
    validated_vfunc(instance, offset).is_some()
}

/// Invokes the GetEntityHashID vfunc through an already-guarded pointer from
/// [`validated_vfunc`], avoiding a second raw vtable walk.
pub(crate) fn actor_type_id_via(instance: *const usize, type_fn: *const usize) -> u32 {
    let mut type_id: u32 = 0;
    unsafe {
        let func: GetEntityHashID0x58 = std::mem::transmute(type_fn);
        func(instance, &mut type_id as *mut u32);
    }
    type_id
}

/// Pl1900 (Id, human form) actor type hash.
const ID_HUMAN_TYPE: u32 = 0x8056ABCD;
/// Pl2000 (Id, dragon form) actor type hash.
pub(crate) const ID_DRAGON_TYPE: u32 = 0xF5755C0E;
const ID_DRAGON_PARENT_ENTITY_OFFSET: usize = 0x1CA98;

// Returns the specified instance of the parent entity.
// ptr+offset: Entity, then +0x70: m_pSpecifiedInstance (Pl0700, Pl1200, ...).
//
// Both hops are SEH-guarded: these parent-link offsets are version-fragile, and
// a stale one (or a pet/form instance smaller than the offset) previously meant
// a raw deref of unmapped memory on the game thread — the silent-freeze class
// of bug. A failed read just leaves the child actor ungrouped.
#[inline(always)]
pub(crate) fn parent_specified_instance_at(
    actor_ptr: *const usize,
    offset: usize,
) -> Option<*const usize> {
    let entity = diag::read_ptr_guarded(actor_ptr as usize, offset)?;
    if entity == 0 {
        return None;
    }

    let parent = diag::read_ptr_guarded(entity, 0x70)?;
    (parent != 0).then_some(parent as *const usize)
}

/// Summon body classes that store their summoner as a validated entity handle
/// `{+0xFE0 idx, +0xFE8 CEntityInfo*}`. Enemy-cast summons resolve to an enemy
/// owner, which fails closed downstream — no player is credited, which is
/// correct.
const SUMMON_BODY_HASHES: &[u32] = &[
    0xD2E5407A, // So0000  Lucilius
    0x6D068BDE, // So0200  Rolan
    // The three Primal Burst beasts. Alone among the game's 77 summon classes
    // they carry a `TXT_SMN_So####_ASCE` burst-attack name and have no
    // `summon.tbl` row — they cannot be equipped and called, so their damage is
    // a Primal Burst (four back-to-back SBAs with the gauge available). Without
    // an arm here their hits resolve to no summoner and the parser drops them.
    0x5418B8F8, // So0300  Proto Bahamut (burst: Catastrophe)
    0x32776C5B, // So0400  Proto Bahamut, Transcendent Blue (burst: Azure Ruin)
    0x870A9DFE, // So0500  Excavallion (burst: Desert Flare)
    0x69893920, // So0d00  Goblin Soldier
    0x1D3EDC63, // So1100  Goblin Warrior
    0xAE913DE3, // So1100Base (generic summon body)
    0xDFEC5706, // So1d00  Quakadile
    0x34894579, // So2001  Silverslime var.
    0x1DB19581, // So4500  Lilith
    0x9F394F85, // So4502  Lilith var.
    0x65294C5C, // So4c00  Managarmr Nihilla
    0x18617D59, // So4e00  Albacore
    0x925ADE1B, // So4f00  Hope-Filled Skydwellers
    0x6093301C, // So5600  Mellose Clan
    0xA22E16CF, // So5700  Crew Alliance Rafale
    0x0F617FF0, // So5f01  Cat var.
    0xF065D8B8, // So6400  Wheel of Fate
    0x5395CE93, // So9200  Beelzebub
    0xB0792857, // BehaviorSummonObjectBase (generic summon body)
];

/// True iff `type_id` is one of the summon body classes above. Only sixteen of
/// the game's seventy-four summons own a body class; the rest share a generic
/// one, so a true answer here does NOT identify which summon was called.
pub(crate) fn is_summon_body(type_id: u32) -> bool {
    SUMMON_BODY_HASHES.contains(&type_id)
}

/// `SummonUnitData.id_`, then `SummonObjectData.id_` as the cross-check. Both
/// are named by the game's own reflection tables and both carried the same
/// value on every body observed live (2026-07-24); reading the second only when
/// the first fails costs nothing on the happy path.
const SUMMON_UNIT_ID_OFFSET: usize = 0x11C0;
const SUMMON_OBJECT_ID_OFFSET: usize = 0x1020;

/// The high half of a summon body id, tagging it as a summon object. Anything
/// else means the offset is stale and the low half is not a summon number.
const SUMMON_ID_CATEGORY: u32 = 0x010D;

/// The concrete summon behind a body, as its `So####` class hash.
///
/// A body id reads `0x010D_<summon><unit>`: the low byte selects the unit
/// within the summon, the byte above it the summon. Live: Silverslime's body
/// reports `0x010D2001` and Goldslime's `0x010D2101`, while BOTH report class
/// hash `0x34894579` — the class can never separate them, this can. Every one
/// of the game's seventy-four summon classes is `So####00`, so clearing the
/// unit byte turns any unit's id into its summon's class id.
///
/// Returns `None` when the id does not look like a summon object, so a stale
/// offset after a game patch leaves rows generic rather than misnamed.
pub(crate) fn concrete_summon_class(source: *const usize) -> Option<u32> {
    let id = match diag::read_u32_guarded(source as usize, SUMMON_UNIT_ID_OFFSET) {
        id if id >> 16 == SUMMON_ID_CATEGORY => id,
        _ => diag::read_u32_guarded(source as usize, SUMMON_OBJECT_ID_OFFSET),
    };
    if id >> 16 != SUMMON_ID_CATEGORY {
        warn_stale_rva_once(&SUMMON_ID_WARNED, "summon body id offsets");
        return None;
    }

    Some(crate::hooks::gamehash::game_xxhash32(&summon_class_id(
        (id & 0xFF00) as u16,
    )))
}

/// Renders a summon number as the `So####` id string the engine hashes.
/// Lowercase hex, because that is how the game spells them (`So0d00`).
fn summon_class_id(summon: u16) -> [u8; 6] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut id = *b"So0000";
    for (i, shift) in [12, 8, 4, 0].into_iter().enumerate() {
        id[2 + i] = HEX[((summon >> shift) & 0xF) as usize];
    }
    id
}

/// One-shot latch for a body-id offset that stops looking like a summon id.
static SUMMON_ID_WARNED: AtomicBool = AtomicBool::new(false);

/// The actor type to publish for a damage source: the concrete summon's class
/// hash when the source is a summon body we can resolve, else the type the
/// game reported.
///
/// This is deliberately NOT folded into the type used for owner attribution —
/// that still has to match `SUMMON_BODY_HASHES` to find the summoner, and a
/// concrete hash is not in that table.
#[inline(always)]
pub fn published_actor_type(source_type_id: u32, source: *const usize) -> u32 {
    if !is_summon_body(source_type_id) {
        return source_type_id;
    }
    concrete_summon_class(source).unwrap_or(source_type_id)
}

/// Handle offsets on a summon body: index gate, then the entity pointer.
const SUMMON_OWNER_IDX_OFFSET: usize = 0xFE0;
const SUMMON_OWNER_PTR_OFFSET: usize = 0xFE8;

/// Two-hop attribution: source -> parent summon (re-typed by vtable RVA, no
/// vfunc call on a swept pointer) -> summoner. Fails closed at every step.
fn two_hop_summoner(
    source: *const usize,
    hop1_idx_off: usize,
    hop1_ptr_off: usize,
) -> Option<*const usize> {
    let hop1 = gated_parent_offset(source, hop1_idx_off, hop1_ptr_off)?;
    let summon = parent_specified_instance_at(source, hop1)?;

    // Re-type WITHOUT a vfunc call. A miss is a stale-RVA breadcrumb.
    if !is_summon_base_vtable(summon) {
        warn_stale_rva_once(&SUMMON_VTABLE_RVA_WARNED, "summon-base vtable RVAs");
        return None;
    }

    let hop2 = gated_parent_offset(summon, SUMMON_OWNER_IDX_OFFSET, SUMMON_OWNER_PTR_OFFSET)?;
    let owner = parent_specified_instance_at(summon, hop2)?;

    // The terminal must be a real player, else credit nobody.
    if !vfunc_slot_readable(owner, 0x58) {
        return None;
    }
    crate::hooks::player::player_slot_key_for_actor(owner)?;
    Some(owner)
}

/// Resolves a keyless sub-entity source to its owner's instance pointer.
/// Returns `None` for any source we do not own a mapping for, and for every
/// failed read along the way.
fn resolve_source_parent_ptr(source_type_id: u32, source: *const usize) -> Option<*const usize> {
    // Two-hop arms: owner handle -> parent summon -> +0xFE8 summoner. Handled
    // ahead of the table because they yield a resolved pointer, not an offset.
    match source_type_id {
        // SoAhrimanBaseLaser
        0x8FE0DF11 => return two_hop_summoner(source, 0x4F0, 0x4F8),
        // We8090 / We8091 / We8170 (em8000 "Seofon" sword entities)
        0xAE1F95D9 | 0xAE1E9FFC | 0x2E0DE3A8 => {
            return two_hop_summoner(source, SUMMON_OWNER_IDX_OFFSET, SUMMON_OWNER_PTR_OFFSET)
        }
        _ => {}
    }

    let parent_offset = match source_type_id {
        // Pl0700Ghost -> Pl0700 (Ferry). v2.0.2 moved the owner-entity link
        // 0xE48 -> 0xE58; with the old offset ALL ghost damage was dropped.
        0x2AF678E8 => 0xE58,
        // Pl0700GhostSatellite (Umlauf) -> Pl0700. Same -0x20 shift.
        0x8364C8BC => 0x4E8,
        // Wp2290 -> Pl2200 (Seofon's Avatar, actions 900-904).
        0x5B1AB457 => 0x4E0,
        // Pl0600PlantRose -> Pl0600.
        0x69C0CA71 => 0x7E0,
        // Wp1890 -> Pl1800 (Cagliostro's sled; also the damage actor for Pain
        // Train and Alexandria). The owner handle is empty in some sled states,
        // so gate on the handle index at +0x550.
        0xC9F45042 => gated_parent_offset(source, 0x550, 0x558)?,
        // Summon bodies -> summoner, via the validated entity handle.
        hash if SUMMON_BODY_HASHES.contains(&hash) => {
            gated_parent_offset(source, SUMMON_OWNER_IDX_OFFSET, SUMMON_OWNER_PTR_OFFSET)?
        }
        // Pl8000 (controllable-summon spawner) -> summoner; same handle idiom.
        0x3B5133C4 => gated_parent_offset(source, 0x23E0, 0x23E8)?,
        _ => return None,
    };

    let parent = parent_specified_instance_at(source, parent_offset)?;
    // The fetched value may not be a live entity, and the type-id vfunc makes a
    // vtable call — probe the slot first so a stale offset fails closed.
    if !vfunc_slot_readable(parent, 0x58) {
        return None;
    }
    Some(parent)
}

/// The HUMAN (`Pl1900`) body behind Id's dragon form, when `source` is the
/// `Pl2000` body — the same parent link [`get_source_parent`]'s dragon case
/// resolves. `None` for every other actor type: this is deliberately NOT a
/// generic owner walk, it exists so the per-hit source-state snapshot can read
/// the one owned body whose owner is a verified `Pl####` layout.
#[inline(always)]
pub(crate) fn dragon_form_owner(source_type_id: u32, source: *const usize) -> Option<*const usize> {
    if source_type_id != ID_DRAGON_TYPE {
        return None;
    }
    parent_specified_instance_at(source, ID_DRAGON_PARENT_ENTITY_OFFSET)
}

/// The parent actor to credit a damage source to: `(type id, index, instance)`.
///
/// `Pl2000` is handled ahead of the generic path because Id's dragon form
/// carries its own player key, so it must be forced back onto the human type
/// and the owner's index — the slot-scoped remap behind the recruited-Id
/// empty-slot-4 fix. Do not fold it into the table.
#[inline(always)]
pub fn get_source_parent(
    source_type_id: u32,
    source: *const usize,
) -> Option<(u32, u32, *const usize)> {
    if source_type_id == ID_DRAGON_TYPE {
        let parent_instance = parent_specified_instance_at(source, ID_DRAGON_PARENT_ENTITY_OFFSET)?;
        let parent_idx = diag::read_ptr_guarded(parent_instance as usize, 0x170)? as u32;
        return Some((ID_HUMAN_TYPE, parent_idx, parent_instance));
    }

    let parent = resolve_source_parent_ptr(source_type_id, source)?;
    // Call the type-id vfunc through the guarded slot, never a raw re-walk:
    // the guard-to-use gap on a recycled allocation is the probe-crash class.
    let type_fn = validated_vfunc(parent, 0x58)?;
    Some((
        actor_type_id_via(parent, type_fn),
        crate::hooks::actor_idx(parent),
        parent,
    ))
}

#[cfg(test)]
mod tests {
    use super::gated_parent_offset;

    #[test]
    fn gated_offset_returns_the_pointer_offset_when_the_handle_index_is_set() {
        let mut actor = vec![0u8; 0x100];
        unsafe {
            actor
                .as_mut_ptr()
                .byte_add(0x10)
                .cast::<u32>()
                .write_unaligned(7);
        }
        assert_eq!(
            gated_parent_offset(actor.as_ptr().cast::<usize>(), 0x10, 0x18),
            Some(0x18)
        );
    }

    #[test]
    fn gated_offset_rejects_an_empty_handle_index() {
        let actor = vec![0u8; 0x100];
        assert_eq!(
            gated_parent_offset(actor.as_ptr().cast::<usize>(), 0x10, 0x18),
            None
        );
    }

    #[test]
    fn gated_offset_rejects_an_unmapped_actor_without_dereferencing() {
        assert_eq!(
            gated_parent_offset(1usize as *const usize, 0x10, 0x18),
            None
        );
    }

    #[test]
    fn resolves_parent_through_the_entity_link() {
        // Layout: actor+OFFSET -> entity, entity+0x70 -> parent instance.
        let parent = Box::new(0usize);
        let parent_ptr = &*parent as *const usize;
        let mut entity = vec![0u8; 0x78];
        let mut actor = vec![0u8; 0x1CA98 + std::mem::size_of::<usize>()];

        unsafe {
            entity
                .as_mut_ptr()
                .byte_add(0x70)
                .cast::<*const usize>()
                .write_unaligned(parent_ptr);
            actor
                .as_mut_ptr()
                .byte_add(0x1CA98)
                .cast::<*const u8>()
                .write_unaligned(entity.as_ptr());
        }

        assert_eq!(
            super::parent_specified_instance_at(actor.as_ptr().cast::<usize>(), 0x1CA98),
            Some(parent_ptr)
        );
    }

    #[test]
    fn null_entity_link_yields_no_parent() {
        let actor = vec![0u8; 0x100];
        assert_eq!(
            super::parent_specified_instance_at(actor.as_ptr().cast(), 0x40),
            None
        );
    }

    #[test]
    fn two_hop_fails_closed_when_the_first_handle_is_empty() {
        let actor = vec![0u8; 0x1100];
        assert_eq!(
            super::two_hop_summoner(actor.as_ptr().cast(), 0x4F0, 0x4F8),
            None
        );
    }

    #[test]
    fn two_hop_arms_route_through_the_two_hop_resolver() {
        // All four two-hop sources must fail closed on a zeroed actor rather than
        // falling through to the single-hop table.
        let actor = vec![0u8; 0x1100];
        for hash in [0x8FE0DF11u32, 0xAE1F95D9, 0xAE1E9FFC, 0x2E0DE3A8] {
            assert_eq!(
                super::resolve_source_parent_ptr(hash, actor.as_ptr().cast()),
                None,
                "hash {hash:#X} should fail closed"
            );
        }
    }

    #[test]
    fn summon_class_hashes_are_unique() {
        // A duplicate arm in the match is a silent compile-time shadow: the second
        // never fires, so one summon's damage would vanish with no error.
        let mut hashes = super::SUMMON_BODY_HASHES.to_vec();
        let before = hashes.len();
        hashes.sort_unstable();
        hashes.dedup();
        assert_eq!(before, hashes.len(), "duplicate hash in SUMMON_BODY_HASHES");
        assert_eq!(before, 21, "expected 21 summon body classes");
    }

    #[test]
    fn primal_burst_classes_are_summon_bodies() {
        // A Primal Burst is dealt by its own So class, not by the summon body an
        // ordinary summon call uses. Without an arm here the hit resolves to no
        // owner and the parser drops it, so Primal Burst damage never reaches a
        // meter row at all. The expected hashes are the game's own — they key
        // the `TXT_SMN_So####_ASCE` rows of its summon text table.
        for (id, hash) in [
            (&b"So0300"[..], 0x5418_B8F8u32), // Catastrophe
            (&b"So0400"[..], 0x3277_6C5B),    // Azure Ruin
            (&b"So0500"[..], 0x870A_9DFE),    // Desert Flare
        ] {
            assert_eq!(
                crate::hooks::gamehash::game_xxhash32(id),
                hash,
                "{} hashed wrong",
                String::from_utf8_lossy(id)
            );
            assert!(
                super::is_summon_body(hash),
                "{} missing from SUMMON_BODY_HASHES",
                String::from_utf8_lossy(id)
            );
        }

        // The parser filters Primal Burst damage out of the meters using
        // protocol's copy of these hashes. Assert the two lists are the same set
        // so a body added here can never silently go unfiltered there.
        let mut shared = protocol::PRIMAL_BURST_BODY_HASHES.to_vec();
        shared.sort_unstable();
        let mut expected = vec![0x5418_B8F8u32, 0x3277_6C5B, 0x870A_9DFE];
        expected.sort_unstable();
        assert_eq!(
            shared, expected,
            "protocol::PRIMAL_BURST_BODY_HASHES drifted from the bodies checked above"
        );
    }

    #[test]
    fn summon_body_source_with_empty_handle_resolves_to_nothing() {
        // So0000 with a zeroed +0xFE0 handle index must fail closed.
        let actor = vec![0u8; 0x1100];
        assert_eq!(
            super::resolve_source_parent_ptr(0xD2E5407A, actor.as_ptr().cast()),
            None
        );
    }

    #[test]
    fn vtable_rva_table_is_sorted_for_binary_search() {
        // binary_search silently returns wrong answers on an unsorted slice, and a
        // wrong answer here means crediting damage to the wrong player.
        let table = super::SUMMON_BASE_VTABLE_RVAS;
        assert!(
            table.windows(2).all(|w| w[0] < w[1]),
            "SUMMON_BASE_VTABLE_RVAS must be sorted ascending with no duplicates"
        );
    }

    #[test]
    fn unknown_vtable_is_rejected_when_module_base_is_unset() {
        // MODULE_BASE is 0 in the test binary, so every lookup must fail closed.
        let obj = vec![0u8; 0x40];
        assert!(!super::is_summon_base_vtable(obj.as_ptr().cast()));
    }

    #[test]
    fn unmapped_actor_fails_without_dereferencing() {
        assert_eq!(
            super::parent_specified_instance_at(1usize as *const usize, 0),
            None
        );
    }

    /// A summon body with the given ids at the two reflection-named offsets.
    fn summon_body(unit_id: u32, object_id: u32) -> Vec<u8> {
        let mut body = vec![0u8; 0x1C10];
        for (offset, id) in [
            (super::SUMMON_UNIT_ID_OFFSET, unit_id),
            (super::SUMMON_OBJECT_ID_OFFSET, object_id),
        ] {
            body[offset..offset + 4].copy_from_slice(&id.to_le_bytes());
        }
        body
    }

    #[test]
    fn renders_a_summon_number_as_its_lowercase_id_string() {
        // "So0d00", not "So0D00" — the hash is taken over the exact spelling,
        // so a case slip yields a hash that names nothing.
        assert_eq!(&super::summon_class_id(0x0d00), b"So0d00");
        assert_eq!(&super::summon_class_id(0x2100), b"So2100");
        assert_eq!(&super::summon_class_id(0x9200), b"So9200");
    }

    #[test]
    fn resolves_the_two_slimes_that_share_one_body_class() {
        // The live case this exists for: both bodies report class 0x34894579,
        // and only the unit byte of the id tells them apart.
        let silverslime = summon_body(0x010D_2001, 0x010D_2001);
        let goldslime = summon_body(0x010D_2101, 0x010D_2101);

        assert_eq!(
            super::concrete_summon_class(silverslime.as_ptr().cast()),
            Some(0x2D17_9019),
            "So2000 Silverslime"
        );
        assert_eq!(
            super::concrete_summon_class(goldslime.as_ptr().cast()),
            Some(0x6BCA_42AB),
            "So2100 Goldslime"
        );
    }

    #[test]
    fn falls_back_to_the_object_id_when_the_unit_id_is_unset() {
        let body = summon_body(0, 0x010D_2101);
        assert_eq!(
            super::concrete_summon_class(body.as_ptr().cast()),
            Some(0x6BCA_42AB)
        );
    }

    #[test]
    fn a_summon_that_owns_its_class_resolves_back_to_that_class() {
        // Lucilius is one of the sixteen already named from its body class;
        // resolution must not change what it publishes.
        let body = summon_body(0x010D_0000, 0x010D_0000);
        assert_eq!(
            super::concrete_summon_class(body.as_ptr().cast()),
            Some(0xD2E5_407A)
        );
    }

    #[test]
    fn an_id_without_the_summon_category_resolves_to_nothing() {
        // What a stale offset looks like after a game patch: the bytes read
        // fine but are not a summon id, and naming a row from them would be a
        // confident lie. Generic beats wrong.
        let body = summon_body(0xDEAD_BEEF, 0xDEAD_BEEF);
        assert_eq!(super::concrete_summon_class(body.as_ptr().cast()), None);
    }

    #[test]
    fn published_type_passes_non_summon_sources_through_untouched() {
        let body = summon_body(0x010D_2101, 0x010D_2101);
        // A player actor is not a summon body: publish exactly what was read,
        // without even looking at the offsets.
        assert_eq!(
            super::published_actor_type(super::ID_DRAGON_TYPE, body.as_ptr().cast()),
            super::ID_DRAGON_TYPE
        );
    }

    #[test]
    fn published_type_keeps_the_body_class_when_the_id_does_not_resolve() {
        let body = summon_body(0, 0);
        assert_eq!(
            super::published_actor_type(0xB079_2857, body.as_ptr().cast()),
            0xB079_2857,
            "unresolved bodies must keep falling back to the generic label"
        );
    }

    #[test]
    fn published_type_swaps_a_shared_body_class_for_the_concrete_summon() {
        let body = summon_body(0x010D_2001, 0x010D_2001);
        assert_eq!(
            super::published_actor_type(0x3489_4579, body.as_ptr().cast()),
            0x2D17_9019
        );
    }

    #[test]
    fn published_type_survives_an_unmapped_source() {
        assert_eq!(
            super::published_actor_type(0xB079_2857, 1usize as *const usize),
            0xB079_2857
        );
    }
}
