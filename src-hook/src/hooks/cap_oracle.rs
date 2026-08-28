//! Damage-cap ORACLE. Verification only — never compiled into a release hook.
//!
//! Records the game's own per-term cap contributions so the pure reproduction
//! in `src-tauri/src/parser/v1/cap/` can be diffed against ground truth. The
//! reproduction is what ships; this exists to prove it right.
//!
//! Gated behind [`BuildGuard`]: the chokepoint it detours serves EVERY
//! parameter query in the game, and recording unconditionally is not
//! affordable on the game thread.

use anyhow::{anyhow, Result};
use retour::static_detour;

use crate::hooks::damage::within_module_image;
use crate::hooks::diag::{read_f32_guarded, read_ptr_guarded, read_u32_guarded};
use crate::process::Process;

/// The single chokepoint every itemized cap contribution passes through
/// (`FUN_142333240`). Walks a provider's effect entries, matches each
/// descriptor's param id against the requested one, and sums the values.
///
/// Direct-entry form: preceding `ret`/`int3` padding, cursor on the entry.
/// Note the padding here is `c3 cc` — ONE int3, not the four the other
/// direct-entry sigs in this repo anchor on; `c3 cc cc cc cc` matches nothing.
/// The prologue alone is a common idiom (4 matches), so the pattern runs on
/// into the body through the `[rcx+0x20]`/`[rcx+0x28]` entry-range load.
/// sigscan 2026-08-08: exactly 1 match, target_rva=0x2333240 (v2.0.4).
const CAP_TERM_SITE_SIG: &str = "c3 cc ' 41 57 41 56 41 55 41 54 56 57 55 53 48 81 ec 88 00 00 00 \
     c5 f8 29 74 24 70 4c 89 4c 24 60 48 c7 02 00 00 00 00 4c 8b 69 20 48 8b 69 28";

/// The DamageInstance builder (`FUN_1409c1cf0`), which computes the cap as
/// `(int)((1 + Σ cap-up − Σ cap-down) × trunc(baseCap))`. Detoured only to arm
/// [`BuildGuard`] around it, so the chokepoint detour records the terms of
/// THIS hit and nothing else.
///
/// Its `push`/`sub`/`vmovaps` prologue is shared with 58 other functions, so
/// the pattern runs on through the xmm spill block to the distinctive
/// `lea r14,[rcx+0x22f0]`.
/// sigscan 2026-08-08: exactly 1 match, target_rva=0x9c1cf0 (v2.0.4).
const DAMAGE_INSTANCE_BUILD_SIG: &str =
    "cc cc cc cc ' 55 41 57 41 56 41 55 41 54 56 57 53 48 81 ec 58 01 00 00 \
     48 8d ac 24 80 00 00 00 c5 78 29 bd c0 00 00 00 c5 78 29 b5 b0 00 00 00 \
     c5 78 29 ad a0 00 00 00 c5 78 29 a5 90 00 00 00 c5 78 29 9d 80 00 00 00 \
     c5 78 29 55 70 c5 78 29 4d 60 c5 78 29 45 50 c5 f8 29 7d 40 c5 f8 29 75 30 \
     48 c7 45 28 fe ff ff ff 4c 89 c3 48 89 d6 48 89 cf 4c 8d b1 f0 22 00 00";

/// `FUN_1406cf390` — the aggregator over the four global effect-provider lists
/// (`+0xb860`/`+0xb868` per list, which its prologue loads). The builder calls
/// it twice, `fVar4 = f(2, …)` and `fVar5 = f(0x22, …)`.
///
/// Detoured ONLY to cross-check the chokepoint walk: this function funnels
/// through `FUN_142333240`, so its return is by construction the sum of the
/// per-provider totals already recorded. Agreement proves the walk attributes
/// exactly what the game summed; it adds no new term.
/// sigscan 2026-08-08: exactly 1 match, target_rva=0x6cf390 (v2.0.4).
const EFFECT_LIST_AGGREGATE_SIG: &str =
    "cc cc cc cc ' 41 57 41 56 41 55 41 54 56 57 55 53 48 83 ec 78 \
     c5 f8 29 7c 24 60 c5 f8 29 74 24 50 4c 89 ce 44 89 c7 48 89 d3 89 cd";

/// `FUN_140766ad0` — the second aggregator (container `DAT_147c1f518`), whose
/// result reaches the formula as `afStack_140[0]` (param 2) and
/// `afStack_138[0]` (param 0x22). Also funnels through `FUN_142333240`.
/// sigscan 2026-08-08: exactly 1 match, target_rva=0x766ad0 (v2.0.4).
const CONTAINER_AGGREGATE_SIG: &str =
    "cc cc cc cc ' 41 57 41 56 41 55 41 54 56 57 55 53 48 81 ec 98 00 00 00 \
     c5 78 29 84 24 80 00 00 00 c5 f8 29 7c 24 70 c5 f8 29 74 24 60 \
     4c 89 4c 24 50 48 89 54 24 48 48 c7 02 00 00 00 00";

/// Which aggregator a recorded total came from.
pub(crate) const AGG_EFFECT_LISTS: u8 = 0;
pub(crate) const AGG_CONTAINER: u8 = 1;

/// Provider layout, read off the `FUN_142333240` decompile: the effect entries
/// are the half-open pointer range `[provider+0x20, provider+0x28)` at stride
/// `0x40`, and each entry holds its effect descriptor at `entry+0x38`.
const PROVIDER_ENTRIES_BEGIN: usize = 0x20;
const PROVIDER_ENTRIES_END: usize = 0x28;
const ENTRY_STRIDE: usize = 0x40;
const ENTRY_DESCRIPTOR: usize = 0x38;

/// The descriptor's own pre-filter field: the game skips any entry whose
/// `descriptor+0xc` is >= 2 before it even asks for the param id.
const DESCRIPTOR_KIND_FIELD: usize = 0xc;

/// Virtual slot returning the descriptor's param id. A bare `this`-only getter
/// — the same call the game makes one instruction earlier in its own loop,
/// which is why calling it here is affordable and side-effect free.
const DESCRIPTOR_PARAM_ID_SLOT: usize = 0x38;

/// Param ids this oracle cares about (see `damage-cap-formula-re`): 2 is
/// DMG-cap-up, 0x22 is DMG-cap-down (subtracted by the builder).
const PARAM_ID_CAP_UP: i32 = 2;
const PARAM_ID_CAP_DOWN: i32 = 0x22;

/// A shifted layout after a patch must not spin the game thread. Real
/// providers carry a handful of entries; anything past this is garbage.
const MAX_ENTRIES: usize = 256;

/// Buff-sourced cap terms do NOT pass the chokepoint; the builder sums them
/// itself over a status list, so the oracle has to walk the same list to see
/// them. All of this is read off the `FUN_1409c1cf0` decompile.
///
/// The holder is reached through the builder's FIRST argument, not the
/// DamageInstance: `*(arg1 + 0x2300)`.
const BUILD_STATUS_HOLDER: usize = 0x2300;
/// The holder's status vector. The decompile indexes a `longlong *` as
/// `[0x15f]`/`[0x160]`, which are POINTER indices — in bytes, `+0xAF8`/`+0xB00`.
const HOLDER_STATUS_LIST_BEGIN: usize = 0xAF8;
/// End of that vector; see [`HOLDER_STATUS_LIST_BEGIN`].
const HOLDER_STATUS_LIST_END: usize = 0xB00;

/// `__RTDynamicCast(inptr, VfDelta, SrcType, TargetType, isReference)`, entry
/// confirmed by `SymbolAt 0x496026c` → `FUN_14496026c` (v2.0.4).
///
/// The cast is load-bearing, not a filter that could be skipped: the game calls
/// the value virtual on the pointer the cast RETURNS, and under multiple
/// inheritance that is an adjusted subobject pointer. Calling the slot on the
/// raw `StatusBase*` would read a different vtable.
const RT_DYNAMIC_CAST_RVA: usize = 0x4960a4c; // 2.0.4: 0x496026c
/// Source type for that cast — `StatusBase::RTTI_Type_Descriptor` (v2.0.5).
const STATUS_BASE_TD_RVA: usize = 0x6ebe620; // 2.0.4: 0x6ebe2d0
/// Target type — `IStatusDamageLimitBuff::RTTI_Type_Descriptor` (v2.0.5),
/// round-tripped through `SymbolAt.java`.
const DAMAGE_LIMIT_BUFF_TD_RVA: usize = 0x6e61560; // 2.0.4: 0x6e613d0
/// Virtual slot on the cast-to interface returning the buff's cap contribution.
const DAMAGE_LIMIT_BUFF_VALUE_SLOT: usize = 8;
/// u32 `status.tbl` StatusId, the same offset `status.rs` reads.
const STATUS_ID_OFFSET: usize = 0x50;
/// Bound on the status walk, for the same reason [`MAX_ENTRIES`] exists.
const MAX_STATUSES: usize = 256;

/// The holder virtual returning the player record the cap-up fields live on
/// (`lVar24 = (**(code **)(*plVar23 + 0x9f0))(plVar23)`). A `this`-only getter.
const HOLDER_PLAYER_RECORD_SLOT: usize = 0x9f0;

/// The record's three cap-up fields, in percent (the builder scales by 0.01).
/// Which one applies is chosen by the DamageInstance's class flags — this is
/// the Normal / Skill / Skybound-Art split `lang/en/overmasteries.json` names.
const RECORD_CAP_UP_NORMAL: usize = 0x28;
const RECORD_CAP_UP_SKILL: usize = 0x30;
const RECORD_CAP_UP_SBA: usize = 0x34;
/// `instance+0xf0` bits selecting between them: neither = Normal, `0x10000` =
/// Skill, `0x40000` = Skybound Art (and 0x40000 wins over 0x10000).
const CLASS_FLAG_SKILL: u32 = 0x10000;
const CLASS_FLAG_SBA: u32 = 0x40000;

/// DamageInstance fields, all from the builder/chokepoint decompiles.
const INSTANCE_ATTACK_RATE: usize = 0xdc;
const INSTANCE_CLASS_FLAGS: usize = 0xf0;
const INSTANCE_ACTION_ID: usize = 0x16c;
const INSTANCE_DAMAGE_FLOOR: usize = 0x2b8;
const INSTANCE_DAMAGE_CAP: usize = 0x2bc;

type CapTermSiteFunc =
    unsafe extern "system" fn(*const usize, *mut usize, i32, usize, u32, usize, u32, usize);
type DamageInstanceBuildFunc =
    unsafe extern "system" fn(*const usize, *const usize, *const usize) -> usize;
type EffectListAggregateFunc =
    unsafe extern "system" fn(i32, *const usize, u32, *const usize, usize, usize) -> u64;
type ContainerAggregateFunc = unsafe extern "system" fn(
    *const usize,
    *mut usize,
    i32,
    *const usize,
    u32,
    *const usize,
    u32,
    usize,
);

static_detour! {
    /// 8 args, confirmed from the prologue: rcx/rdx/r8d/r9 plus all four stack
    /// slots — `[rsp+0xf0]` (32-bit), `[rsp+0xf8]` (64), `[rsp+0x100]` (32),
    /// `[rsp+0x108]` (64), which after 8 pushes and a 0x88 frame are args 5-8.
    static CapTermSite: unsafe extern "system" fn(
        *const usize, *mut usize, i32, usize, u32, usize, u32, usize);
    /// 3 args: the prologue moves rcx/rdx/r8 into rdi/rsi/rbx and never reads
    /// r9. `rdx` is the DamageInstance — it is what receives `[rsi+0x2d8]`.
    static DamageInstanceBuild: unsafe extern "system" fn(
        *const usize, *const usize, *const usize) -> usize;
    /// 6 args (`mov rsi,r9` / `mov edi,r8d` / `mov rbx,rdx` / `mov ebp,ecx`),
    /// arg1 the param id.
    ///
    /// Returns **u64, not f32**: the epilogue is `vextractps eax,xmm6,0` +
    /// `vextractps ecx,xmm6,1`, i.e. it packs the float PAIR into rax rather
    /// than returning in xmm0. Declaring f32 here would read an unrelated
    /// register and silently invent a cross-check that proves nothing.
    static EffectListAggregate: unsafe extern "system" fn(
        i32, *const usize, u32, *const usize, usize, usize) -> u64;
    /// 8 args; writes its float pair through the `out` pointer in rdx, which
    /// the prologue zeroes (`mov qword [rdx],0`).
    static ContainerAggregate: unsafe extern "system" fn(
        *const usize, *mut usize, i32, *const usize, u32, *const usize, u32, usize);
}

thread_local! {
    /// The DamageInstance currently being built on this thread, or `None`.
    ///
    /// Save/restore rather than set/clear, for the same reason `sba.rs`'s
    /// `HitGuard` does it: builds nest, and clearing on drop would erase the
    /// enclosing build and stop recording for the rest of it.
    ///
    /// All access goes through `try_with`, so a call during thread teardown
    /// degrades to "nothing armed" instead of panicking inside the game.
    static BUILDING: std::cell::Cell<Option<*const usize>> =
        const { std::cell::Cell::new(None) };

    /// Terms recorded during the current build, in call order:
    /// `(descriptor_vtable_rva, param_id, value)`.
    static TERMS: std::cell::RefCell<Vec<(u32, i32, f32)>> =
        const { std::cell::RefCell::new(Vec::new()) };

    /// What each aggregator returned during the current build:
    /// `(source, param_id, value)`. The cross-check for [`TERMS`].
    static AGGREGATES: std::cell::RefCell<Vec<(u8, i32, f32)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Arms recording for the duration of one DamageInstance build.
pub(crate) struct BuildGuard(
    Option<*const usize>,
    Vec<(u32, i32, f32)>,
    Vec<(u8, i32, f32)>,
);

impl BuildGuard {
    pub(crate) fn arm(damage_instance: *const usize) -> Self {
        let previous = BUILDING
            .try_with(|c| c.replace(Some(damage_instance)))
            .unwrap_or(None);
        // Both buffers are parked for the same reason the slot is: builds nest,
        // each buffer is flat, and an inner build's emit drains them. Without
        // this, the inner line would carry the enclosing build's records and
        // the enclosing line would then be missing them — one hit's cap
        // reported against another's, which is worse than reporting neither.
        let parked_terms = TERMS
            .try_with(|t| std::mem::take(&mut *t.borrow_mut()))
            .unwrap_or_default();
        let parked_aggregates = AGGREGATES
            .try_with(|a| std::mem::take(&mut *a.borrow_mut()))
            .unwrap_or_default();
        BuildGuard(previous, parked_terms, parked_aggregates)
    }

    pub(crate) fn current() -> Option<*const usize> {
        BUILDING.try_with(|c| c.get()).ok().flatten()
    }
}

impl Drop for BuildGuard {
    fn drop(&mut self) {
        let _ = BUILDING.try_with(|c| c.set(self.0));
        let parked_terms = std::mem::take(&mut self.1);
        let _ = TERMS.try_with(|t| *t.borrow_mut() = parked_terms);
        let parked_aggregates = std::mem::take(&mut self.2);
        let _ = AGGREGATES.try_with(|a| *a.borrow_mut() = parked_aggregates);
    }
}

/// Records one aggregator's return. A no-op when nothing is armed.
pub(crate) fn record_aggregate(source: u8, param_id: i32, value: f32) {
    if BuildGuard::current().is_none() {
        return;
    }
    let _ = AGGREGATES.try_with(|a| a.borrow_mut().push((source, param_id, value)));
}

/// Drains the recorded aggregator returns.
pub(crate) fn take_aggregates() -> Vec<(u8, i32, f32)> {
    AGGREGATES
        .try_with(|a| std::mem::take(&mut *a.borrow_mut()))
        .unwrap_or_default()
}

/// `Σ chokepoint terms − Σ aggregator returns`, for one param id.
///
/// Zero means the per-provider walk accounts for exactly what the game's own
/// aggregators returned, which is the only thing this oracle can prove about
/// its own correctness — both aggregators funnel through the chokepoint, so
/// this compares two views of the SAME contributions rather than adding new
/// ones.
///
/// The two param ids are kept apart on purpose: they enter the builder's
/// formula with opposite signs, so netting them together could cancel a real
/// disagreement into an apparent match.
pub(crate) fn aggregate_drift(
    terms: &[(u32, i32, f32)],
    aggregates: &[(u8, i32, f32)],
    param_id: i32,
) -> f32 {
    let terms_sum: f32 = terms
        .iter()
        .filter(|(_, id, _)| *id == param_id)
        .map(|(_, _, value)| value)
        .sum();
    let aggregate_sum: f32 = aggregates
        .iter()
        .filter(|(_, id, _)| *id == param_id)
        .map(|(_, _, value)| value)
        .sum();
    terms_sum - aggregate_sum
}

/// Records one contribution. A no-op when nothing is armed, which is the
/// common case — the chokepoint serves the whole game.
pub(crate) fn record_term(descriptor_vtable_rva: u32, param_id: i32, value: f32) {
    if BuildGuard::current().is_none() {
        return;
    }
    let _ = TERMS.try_with(|t| {
        t.borrow_mut()
            .push((descriptor_vtable_rva, param_id, value))
    });
}

/// Drains the recorded terms, leaving the buffer clean for the next build.
pub(crate) fn take_terms() -> Vec<(u32, i32, f32)> {
    TERMS
        .try_with(|t| std::mem::take(&mut *t.borrow_mut()))
        .unwrap_or_default()
}

/// The descriptor vtable RVAs in `provider` whose param id matches `param_id`,
/// i.e. exactly the entries the game just summed.
///
/// Replicates the chokepoint's own accept test in the same order: the plain
/// `descriptor+0xc < 2` pre-filter first, then the virtual param-id getter.
/// Every read is guarded, so a layout that shifted under a patch yields an
/// empty list rather than faulting the game thread.
///
/// It deliberately does NOT replicate the applicability predicate at the
/// condition's `vfn+0x58`, so this can over-report: an entry whose condition
/// rejected it still appears. That only ever widens a participant set, which
/// costs attribution, never a wrong value — see `run_term_site` for why the
/// value is only ever attributed to a set of size one.
fn participant_vtables(provider: usize, param_id: i32) -> Vec<u32> {
    let mut found = Vec::new();
    let module_base = crate::hooks::diag::MODULE_BASE.load(std::sync::atomic::Ordering::Relaxed);
    if module_base == 0 {
        return found;
    }

    let (Some(begin), Some(end)) = (
        read_ptr_guarded(provider, PROVIDER_ENTRIES_BEGIN),
        read_ptr_guarded(provider, PROVIDER_ENTRIES_END),
    ) else {
        return found;
    };
    if begin == 0 || end < begin || (end - begin) % ENTRY_STRIDE != 0 {
        return found;
    }
    if (end - begin) / ENTRY_STRIDE > MAX_ENTRIES {
        return found;
    }

    let mut entry = begin;
    while entry < end {
        if let Some(vtable_rva) = matching_descriptor(entry, param_id, module_base) {
            found.push(vtable_rva);
        }
        entry += ENTRY_STRIDE;
    }
    found
}

/// The descriptor vtable RVA for one entry, if that entry's descriptor answers
/// `param_id`. `None` for every unreadable or non-matching step.
fn matching_descriptor(entry: usize, param_id: i32, module_base: usize) -> Option<u32> {
    let descriptor = read_ptr_guarded(entry, ENTRY_DESCRIPTOR).filter(|d| *d != 0)?;
    if read_u32_guarded(descriptor, DESCRIPTOR_KIND_FIELD) >= 2 {
        return None;
    }

    let vtable = read_ptr_guarded(descriptor, 0).filter(|v| *v != 0)?;
    // The vtable must live inside the module image, or the "descriptor" is not
    // one and its slot is not a function. Cheapest possible check before a call
    // through a pointer read out of game memory.
    let vtable_rva = vtable.checked_sub(module_base)?;
    if vtable_rva == 0 || vtable_rva > u32::MAX as usize {
        return None;
    }
    let slot = read_ptr_guarded(vtable, DESCRIPTOR_PARAM_ID_SLOT).filter(|s| *s != 0)?;
    slot.checked_sub(module_base)
        .filter(|r| *r <= u32::MAX as usize)?;

    // Safe by construction: `slot` was read out of a vtable that lies inside
    // the module, and this is the same nullary getter the game itself calls on
    // this descriptor in the loop we are shadowing.
    let get_param_id: unsafe extern "system" fn(usize) -> i32 =
        unsafe { std::mem::transmute(slot) };
    let id = unsafe { get_param_id(descriptor) };
    (id == param_id).then_some(vtable_rva as u32)
}

/// The cap contribution of each status the attacker carries, as
/// `(status_id, value)`.
///
/// Replicates the builder's own `fVar43` loop: walk the holder's status vector,
/// dynamic-cast each entry to `IStatusDamageLimitBuff`, and call virtual slot
/// `+8` on the pointer the cast returns. Statuses that are not damage-limit
/// buffs cast to null and cost one call each.
fn buff_terms(context: usize) -> Vec<(u32, f32)> {
    let mut found = Vec::new();
    let module_base = crate::hooks::diag::MODULE_BASE.load(std::sync::atomic::Ordering::Relaxed);
    if module_base == 0 {
        return found;
    }

    let Some(holder) = read_ptr_guarded(context, BUILD_STATUS_HOLDER).filter(|h| *h != 0) else {
        return found;
    };
    let (Some(begin), Some(end)) = (
        read_ptr_guarded(holder, HOLDER_STATUS_LIST_BEGIN),
        read_ptr_guarded(holder, HOLDER_STATUS_LIST_END),
    ) else {
        return found;
    };
    let stride = std::mem::size_of::<usize>();
    if begin == 0 || end < begin || (end - begin) % stride != 0 {
        return found;
    }
    let count = (end - begin) / stride;
    if count == 0 || count > MAX_STATUSES {
        return found;
    }
    // One probe for the whole array rather than one per slot, for the reason
    // status.rs documents: IsBadReadPtr takes an exception path on unmapped
    // memory, and this walk runs on a game thread.
    if !crate::hooks::diag::readable(begin, count * stride) {
        return found;
    }

    let cast: unsafe extern "system" fn(usize, i32, usize, usize, i32) -> usize =
        unsafe { std::mem::transmute(module_base + RT_DYNAMIC_CAST_RVA) };
    let source_type = module_base + STATUS_BASE_TD_RVA;
    let target_type = module_base + DAMAGE_LIMIT_BUFF_TD_RVA;

    for index in 0..count {
        let status = unsafe { ((begin + index * stride) as *const usize).read() };
        if status == 0 {
            continue;
        }
        // The cast reads the object's vtable and the COL behind it, so both
        // must be there before it is called — a freed slot would otherwise
        // fault (or throw __non_rtti_object) inside the game thread.
        let Some(vtable) = read_ptr_guarded(status, 0).filter(|v| *v != 0) else {
            continue;
        };
        if read_ptr_guarded(vtable.wrapping_sub(8), 0)
            .filter(|c| *c != 0)
            .is_none()
        {
            continue;
        }

        let buff = unsafe { cast(status, 0, source_type, target_type, 0) };
        if buff == 0 {
            continue;
        }
        let Some(buff_vtable) = read_ptr_guarded(buff, 0).filter(|v| *v != 0) else {
            continue;
        };
        let Some(slot) =
            read_ptr_guarded(buff_vtable, DAMAGE_LIMIT_BUFF_VALUE_SLOT).filter(|s| *s != 0)
        else {
            continue;
        };

        let value_of: unsafe extern "system" fn(usize) -> f32 =
            unsafe { std::mem::transmute(slot) };
        let value = unsafe { value_of(buff) };
        if value != 0.0 {
            found.push((read_u32_guarded(status, STATUS_ID_OFFSET), value));
        }
    }
    found
}

/// Arms recording across one DamageInstance build and emits the oracle line.
fn run_build(a1: *const usize, a2: *const usize, a3: *const usize) -> usize {
    // Armed for the whole call, restored on drop: the chokepoint runs as a
    // synchronous callee, so anything it records inside this frame belongs to
    // this hit.
    let _armed = BuildGuard::arm(a2);
    let result = unsafe { DamageInstanceBuild.call(a1, a2, a3) };
    // After the call: the builder has finished with the list, and a status the
    // hit itself applied is not part of the cap it just computed. The record
    // read is also deliberately after it, so the game's own spinlock around
    // that structure has been released.
    let buffs = buff_terms(a1 as usize);
    let overmastery = overmastery_terms(a1 as usize, a2 as usize);
    emit_oracle_line(a2 as usize, &buffs, overmastery);
    result
}

/// Records what one provider contributed to this hit's cap.
///
/// The game's function sums into a SINGLE output — `*out` holds the running
/// total, rewritten after every accepted entry — so there is no per-entry value
/// to read here. Attributing that total to each participant would multiply the
/// contribution by the participant count, which is the double-counting the
/// plan's live check exists to catch. Instead the total is attributed only when
/// exactly one entry answered, which is provably that entry's own value;
/// otherwise it is recorded against vtable 0, the honest "unattributed" bucket
/// that Plan B renders as residual rather than as a guessed name.
#[allow(clippy::too_many_arguments)]
fn run_term_site(
    provider: *const usize,
    out: *mut usize,
    param_id: i32,
    a4: usize,
    a5: u32,
    a6: usize,
    a7: u32,
    a8: usize,
) {
    // Cheap gate FIRST: this fires for every parameter query in the game, and
    // only cap-up (2) and cap-down (0x22) matter here.
    let armed = BuildGuard::current().is_some()
        && (param_id == PARAM_ID_CAP_UP || param_id == PARAM_ID_CAP_DOWN);
    unsafe { CapTermSite.call(provider, out, param_id, a4, a5, a6, a7, a8) };
    if !armed {
        return;
    }

    // The summed pair the call just wrote. Read through the guarded helper:
    // a shifted layout after a patch must read None, not fault the game thread.
    let Some(total) = read_f32_guarded(out as usize, 0) else {
        return;
    };
    if total == 0.0 {
        return;
    }

    let participants = participant_vtables(provider as usize, param_id);
    match participants.as_slice() {
        [only] => record_term(*only, param_id, total),
        _ => record_term(0, param_id, total),
    }
}

/// The overmastery-style cap-up the builder reads off the player record, as
/// `(selected_class, selected_value, normal, skill, sba)` in builder units
/// (already scaled by 0.01).
///
/// This is the `fVar28` half of the record path. The `fVar27` half is NOT here:
/// it comes from a second, hash-mapped record the builder finds under a
/// spinlock by the type tag `0xDC584F60`, and replicating that walk hastily
/// would produce a silently-wrong oracle — worse than a missing one.
///
/// All three fields are emitted, not just the selected one, so a capture can
/// check the selection rule itself rather than assuming it.
fn overmastery_terms(
    context: usize,
    instance: usize,
) -> Option<(&'static str, f32, f32, f32, f32)> {
    let module_base = crate::hooks::diag::MODULE_BASE.load(std::sync::atomic::Ordering::Relaxed);
    if module_base == 0 {
        return None;
    }
    let holder = read_ptr_guarded(context, BUILD_STATUS_HOLDER).filter(|h| *h != 0)?;
    let vtable = read_ptr_guarded(holder, 0).filter(|v| *v != 0)?;
    if !within_module_image(vtable, module_base) {
        return None;
    }
    let slot = read_ptr_guarded(vtable, HOLDER_PLAYER_RECORD_SLOT).filter(|s| *s != 0)?;
    if !within_module_image(slot, module_base) {
        return None;
    }

    // Same `this`-only getter the builder calls, on the same object.
    let get_record: unsafe extern "system" fn(usize) -> usize =
        unsafe { std::mem::transmute(slot) };
    let record = unsafe { get_record(holder) };
    if record == 0 {
        return None;
    }

    let normal = read_f32_guarded(record, RECORD_CAP_UP_NORMAL)? * 0.01;
    let skill = read_f32_guarded(record, RECORD_CAP_UP_SKILL)? * 0.01;
    let sba = read_f32_guarded(record, RECORD_CAP_UP_SBA)? * 0.01;

    let class_flags = read_u32_guarded(instance, INSTANCE_CLASS_FLAGS);
    let (name, selected) = if class_flags & CLASS_FLAG_SBA != 0 {
        ("sba", sba)
    } else if class_flags & CLASS_FLAG_SKILL != 0 {
        ("skill", skill)
    } else {
        ("normal", normal)
    };
    Some((name, selected, normal, skill, sba))
}

/// Records what `FUN_1406cf390` returned for this param id.
fn run_effect_list_aggregate(
    param_id: i32,
    a2: *const usize,
    a3: u32,
    a4: *const usize,
    a5: usize,
    a6: usize,
) -> u64 {
    let packed = unsafe { EffectListAggregate.call(param_id, a2, a3, a4, a5, a6) };
    // Lane 0 of the packed pair — the term the builder actually uses.
    record_aggregate(AGG_EFFECT_LISTS, param_id, f32::from_bits(packed as u32));
    packed
}

/// Records what `FUN_140766ad0` wrote through its out pointer.
#[allow(clippy::too_many_arguments)]
fn run_container_aggregate(
    container: *const usize,
    out: *mut usize,
    param_id: i32,
    a4: *const usize,
    a5: u32,
    a6: *const usize,
    a7: u32,
    a8: usize,
) {
    unsafe { ContainerAggregate.call(container, out, param_id, a4, a5, a6, a7, a8) };
    if let Some(value) = read_f32_guarded(out as usize, 0) {
        record_aggregate(AGG_CONTAINER, param_id, value);
    }
}

/// One line per built DamageInstance, carrying the cap the builder itself
/// produced so a capture can be checked against the reproduction without a
/// second data source.
fn emit_oracle_line(
    instance: usize,
    buffs: &[(u32, f32)],
    overmastery: Option<(&'static str, f32, f32, f32, f32)>,
) {
    let terms = take_terms();
    let aggregates = take_aggregates();
    if terms.is_empty() && buffs.is_empty() && aggregates.is_empty() {
        return;
    }
    let overmastery_csv = match overmastery {
        Some((name, selected, normal, skill, sba)) => {
            format!("sel:{name}:{selected:.6},n:{normal:.6},s:{skill:.6},b:{sba:.6}")
        }
        None => String::new(),
    };
    let terms_csv = terms
        .iter()
        .map(|(rva, id, value)| format!("{rva:#x}:{id:#x}:{value:.6}"))
        .collect::<Vec<_>>()
        .join(",");
    let buffs_csv = buffs
        .iter()
        .map(|(status_id, value)| format!("{status_id}:{value:.6}"))
        .collect::<Vec<_>>()
        .join(",");
    let aggregates_csv = aggregates
        .iter()
        .map(|(source, id, value)| format!("{source}:{id:#x}:{value:.6}"))
        .collect::<Vec<_>>()
        .join(",");

    log::info!(
        "CAPORACLE t={} inst={:#x} action={} rate={:.3} class_flags={:#x} cap={} floor={} \
         terms=[{}] buffs=[{}] agg=[{}] drift_up={:.6} drift_down={:.6} om=[{}]",
        crate::hooks::diag::ms(),
        instance,
        read_u32_guarded(instance, INSTANCE_ACTION_ID),
        read_f32_guarded(instance, INSTANCE_ATTACK_RATE).unwrap_or(f32::NAN),
        read_u32_guarded(instance, INSTANCE_CLASS_FLAGS),
        read_u32_guarded(instance, INSTANCE_DAMAGE_CAP),
        read_u32_guarded(instance, INSTANCE_DAMAGE_FLOOR),
        terms_csv,
        buffs_csv,
        aggregates_csv,
        aggregate_drift(&terms, &aggregates, PARAM_ID_CAP_UP),
        aggregate_drift(&terms, &aggregates, PARAM_ID_CAP_DOWN),
        overmastery_csv,
    );
}

pub(crate) struct CapOracleHook;

impl CapOracleHook {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn setup(&self, process: &Process) -> Result<()> {
        let term_site = process
            .search_address(CAP_TERM_SITE_SIG)
            .map_err(|e| anyhow!("cap_oracle: term-site sig failed: {e:?}"))?;
        let build = process
            .search_address(DAMAGE_INSTANCE_BUILD_SIG)
            .map_err(|e| anyhow!("cap_oracle: builder sig failed: {e:?}"))?;
        let effect_lists = process
            .search_address(EFFECT_LIST_AGGREGATE_SIG)
            .map_err(|e| anyhow!("cap_oracle: effect-list aggregator sig failed: {e:?}"))?;
        let container = process
            .search_address(CONTAINER_AGGREGATE_SIG)
            .map_err(|e| anyhow!("cap_oracle: container aggregator sig failed: {e:?}"))?;

        unsafe {
            let term_site: CapTermSiteFunc = std::mem::transmute(term_site);
            CapTermSite.initialize(term_site, run_term_site)?;
            CapTermSite.enable()?;

            let build: DamageInstanceBuildFunc = std::mem::transmute(build);
            DamageInstanceBuild.initialize(build, run_build)?;
            DamageInstanceBuild.enable()?;

            let effect_lists: EffectListAggregateFunc = std::mem::transmute(effect_lists);
            EffectListAggregate.initialize(effect_lists, run_effect_list_aggregate)?;
            EffectListAggregate.enable()?;

            let container: ContainerAggregateFunc = std::mem::transmute(container);
            ContainerAggregate.initialize(container, run_container_aggregate)?;
            ContainerAggregate.enable()?;
        }

        Ok(())
    }
}

#[cfg(any(feature = "eject", test))]
pub(crate) fn disable() {
    super::disable_quiet("CapTermSite", &CapTermSite);
    super::disable_quiet("DamageInstanceBuild", &DamageInstanceBuild);
    super::disable_quiet("EffectListAggregate", &EffectListAggregate);
    super::disable_quiet("ContainerAggregate", &ContainerAggregate);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arming_restores_the_previous_frame_on_drop() {
        assert_eq!(BuildGuard::current(), None);
        let outer = BuildGuard::arm(0x1000 as *const usize);
        assert_eq!(BuildGuard::current(), Some(0x1000 as *const usize));
        {
            let inner = BuildGuard::arm(0x2000 as *const usize);
            assert_eq!(BuildGuard::current(), Some(0x2000 as *const usize));
            drop(inner);
        }
        // The enclosing build must survive the nested one — a clear-on-drop
        // here would silently stop recording for the rest of the outer build.
        assert_eq!(BuildGuard::current(), Some(0x1000 as *const usize));
        drop(outer);
        assert_eq!(BuildGuard::current(), None);
    }

    #[test]
    fn drift_is_zero_when_the_terms_account_for_both_aggregators() {
        let terms = vec![(0xaaa, 2, 0.15), (0xbbb, 2, 0.35), (0xccc, 2, 0.50)];
        // The two aggregators between them returned exactly that sum.
        let aggregates = vec![(AGG_EFFECT_LISTS, 2, 0.50), (AGG_CONTAINER, 2, 0.50)];
        assert_eq!(aggregate_drift(&terms, &aggregates, 2), 0.0);
    }

    #[test]
    fn a_term_the_walk_missed_shows_as_negative_drift() {
        let terms = vec![(0xaaa, 2, 0.15)];
        let aggregates = vec![(AGG_EFFECT_LISTS, 2, 0.40), (AGG_CONTAINER, 2, 0.0)];
        // Σ terms - Σ aggregates: the walk saw 0.25 less than the game summed.
        assert!((aggregate_drift(&terms, &aggregates, 2) - -0.25).abs() < 1e-6);
    }

    #[test]
    fn drift_ignores_the_other_param_id() {
        // A cap-DOWN aggregate must not be netted against cap-UP terms; they are
        // opposite signs in the builder's formula and mixing them would hide a
        // real disagreement.
        let terms = vec![(0xaaa, 2, 0.30)];
        let aggregates = vec![(AGG_EFFECT_LISTS, 2, 0.30), (AGG_CONTAINER, 0x22, 0.90)];
        assert_eq!(aggregate_drift(&terms, &aggregates, 2), 0.0);
        assert!((aggregate_drift(&[], &aggregates, 0x22) - -0.90).abs() < 1e-6);
    }

    #[test]
    fn a_nested_build_does_not_drain_the_enclosing_builds_terms() {
        let outer = BuildGuard::arm(0x1000 as *const usize);
        record_term(0xaaa, 2, 1.0);
        {
            let inner = BuildGuard::arm(0x2000 as *const usize);
            record_term(0xbbb, 2, 2.0);
            // The inner build's line carries ONLY the inner build's terms.
            assert_eq!(take_terms(), vec![(0xbbb, 2, 2.0)]);
            drop(inner);
        }
        // ...and the enclosing build still has its own, untouched.
        assert_eq!(take_terms(), vec![(0xaaa, 2, 1.0)]);
        drop(outer);
    }

    #[test]
    fn recording_is_ignored_when_nothing_is_armed() {
        assert_eq!(BuildGuard::current(), None);
        record_term(0xdead, 2, 1.5);
        assert!(take_terms().is_empty());
    }

    #[test]
    fn recorded_terms_come_back_in_call_order() {
        let _armed = BuildGuard::arm(0x1000 as *const usize);
        record_term(0xaaa, 2, 1.5);
        record_term(0xbbb, 0x22, 0.25);
        assert_eq!(take_terms(), vec![(0xaaa, 2, 1.5), (0xbbb, 0x22, 0.25)]);
        // Taking drains, so the next build starts clean.
        assert!(take_terms().is_empty());
    }
}
