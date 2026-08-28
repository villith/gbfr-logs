use std::sync::atomic::Ordering;

use anyhow::{anyhow, Result};
use protocol::Message;
use retour::static_detour;

use crate::{event, process::Process};

use super::{actor_idx, actor_type_id, globals::SBA_OFFSET};

// v2.0.2, decompiler-verified (FUN_140bb8840): the gauge-update function takes ELEVEN
// arguments — rcx=SBA component*, xmm1=f32 gauge delta, r8d=u32, r9b=u8, then seven stack
// args: u8, f32, u8, u8, u8, u8, u8. All of them are genuinely read in the body (param_6
// is a float added into the gauge math; params 7-11 gate branches). The old 6-arg
// declaration truncated param_6 (f32) to u8 and never marshalled params 7-11, so re-calling
// the original corrupted the in-game gauge (it went negative). We pass all eleven through.
type OnSBAUpdateFunc =
    unsafe extern "system" fn(*const usize, f32, u32, u8, u8, f32, u8, u8, u8, u8, u8) -> usize;
type OnSBAAttemptFunc = unsafe extern "system" fn(*const usize, f32) -> usize;
type OnCheckSBACollisionFunc = unsafe extern "system" fn(*const usize, f32) -> usize;
type OnContinueSBAChainFunc = unsafe extern "system" fn(*const usize, *const usize) -> usize;
type OnRemoteSBAUpdateFunc =
    unsafe extern "system" fn(*const usize, *const usize, f32, f32) -> usize;
/// The per-hit gauge GRANT virtual (see `OnSBAGrantHook`).
/// 3 args, all read by the body per the decompiler — arity must match or the
/// detour faults the game.
type OnSBAGrantFunc = unsafe extern "system" fn(*const usize, *const usize, u32) -> usize;
/// The register-hit gate (see `OnSBARegisterHitHook`). Ghidra v2.0.3
/// (entry 0x9adaa0): arity 2 — `(rcx, rdx=damage instance)`; no incoming
/// r8/r9/stack args are read. Returns void in the game; the `usize` return
/// here is harmless (we just forward whatever is in rax).
type OnSBARegisterHitFunc = unsafe extern "system" fn(*const usize, *const usize) -> usize;

static_detour! {
    static OnSBAUpdate: unsafe extern "system" fn(*const usize, f32, u32, u8, u8, f32, u8, u8, u8, u8, u8) -> usize;
    static OnSBAAttempt: unsafe extern "system" fn(*const usize, f32) -> usize;
    static OnCheckSBACollision: unsafe extern "system" fn(*const usize, f32) -> usize;
    static OnContinueSBAChain: unsafe extern "system" fn(*const usize, *const usize) -> usize;
    static OnRemoteSBAUpdate: unsafe extern "system" fn(*const usize, *const usize, f32, f32) -> usize;
    static OnSBARegisterHit: unsafe extern "system" fn(*const usize, *const usize) -> usize;
}

// PRODUCTION: the per-hit gauge-grant virtual (v2.0.3 entry rva 0x9b41b0).
static_detour! {
    static OnSBAGrant: unsafe extern "system" fn(*const usize, *const usize, u32) -> usize;
}

#[cfg(any(feature = "eject", test))]
pub(super) fn disable() {
    super::disable_quiet("OnSBAUpdate", &OnSBAUpdate);
    super::disable_quiet("OnSBAAttempt", &OnSBAAttempt);
    super::disable_quiet("OnCheckSBACollision", &OnCheckSBACollision);
    super::disable_quiet("OnContinueSBAChain", &OnContinueSBAChain);
    super::disable_quiet("OnRemoteSBAUpdate", &OnRemoteSBAUpdate);
    super::disable_quiet("OnSBARegisterHit", &OnSBARegisterHit);
    super::disable_quiet("OnJustGuardGrant", &OnJustGuardGrant);
    super::disable_quiet("OnEffectGrant", &OnEffectGrant);
    super::disable_quiet("OnGaugePercentGrant", &OnGaugePercentGrant);
    super::disable_quiet("OnQuestStartGauge", &OnQuestStartGauge);
    super::disable_quiet("OnQuestStartGaugeSolo", &OnQuestStartGaugeSolo);
    super::disable_quiet("OnVtableGrant191be40", &OnVtableGrant191be40);
    super::disable_quiet("OnVtableGrant33bbc20", &OnVtableGrant33bbc20);
    super::disable_quiet("OnVtableGrant33bc050", &OnVtableGrant33bc050);
    super::disable_quiet("OnNetGaugeGrant", &OnNetGaugeGrant);
    super::disable_quiet("OnSBAGrant", &OnSBAGrant);
}

// v2.0.2: call-follow sig at the unique gauge-update call site, resolving to the clean
// entry 0xbb8840 (sigscan: 1 match). Arity fixed to the decompiler-verified 11 args (see
// OnSBAUpdateFunc above) — the previous 6-arg declaration corrupted the in-game gauge.
// The sig is a call-follow, so it tracks the entry across patches: that same function is
// rva 0xbb1fc0 on v2.0.3, which is what the register-hit chain below ends at.
const ON_HANDLE_SBA_UPDATE_SIG: &str = "48 89 f1 c5 f8 28 ce 41 89 d8 e8 $ { ' } c4 c1 78 2e f8";

// hookdiag: direct-entry signature for the per-hit gauge-grant virtual
// FUN_1409b41b0 (v2.0.3 rva 0x9b41b0; static RE claims ProcessDamageEvent →
// source vtable +0x70 → +0x80 → THIS → FUN_140bb1c00 → our hooked gauge
// update). Anchored on the int3 padding before the entry plus the prologue
// through the first `vmovaps [rbp+0x3c0],xmm15` (the xmm15 spill is what
// disambiguates it from the one same-prologue sibling at 0x89600).
// sigscan 2026-08-04: exactly 1 match, cursor_rva=0x9b41b0.
const ON_SBA_GRANT_SIG: &str = "cc cc cc cc ' 55 41 57 41 56 41 55 41 54 56 57 53 48 81 ec 58 04 00 00 48 8d ac 24 80 00 00 00 c5 78 29 bd c0 03 00 00";

// PRODUCTION: direct-entry signature for the register-hit gate (v2.0.3 entry
// rva 0x9adaa0), the function that sits directly above the gauge chain — its
// synchronous callee chain 0x9adaa0 -> 0x9add30 -> 0xbb1c00 -> 0xbb1fc0 ends
// at the very function ON_HANDLE_SBA_UPDATE_SIG resolves to. Anchored on the
// prologue plus the first act (`cmp byte [rdx+0x158],0` / `jnz` past the SBA
// logic), cursor at the entry.
// sigscan 2026-08-04: exactly 1 match, cursor_rva=0x9adaa0.
const ON_SBA_REGISTER_HIT_SIG: &str = "' 41 57 41 56 41 55 41 54 56 57 55 53 48 83 ec 38 80 ba 58 01 00 00 00 0f 85 ? ? ? ? c5 fa 10 05 ? ? ? ?";

thread_local! {
    /// The damage instance of the hit currently being registered on this
    /// thread. The game's gauge update is a synchronous callee of the register
    /// -hit gate (verified: gate -> wrapper -> gauge-add -> gauge-update), so
    /// whatever is parked here when the gauge moves is the hit that moved it.
    ///
    /// Raw pointer, decoded only by the reader: this gate fires on every
    /// registered hit while the gauge moves on a small fraction of them, so
    /// decoding here would throw nearly all the work away. Sound because the
    /// value only ever lives inside the guard's scope, which is inside the
    /// detour frame that owns the object.
    ///
    /// Residual: a gauge rise that did NOT come through the gate but happens
    /// to run nested inside a gate call on the same thread (a scripted award
    /// fired from within hit processing) reads the parked hit and is credited
    /// to it. Far narrower than the time window this replaced — it needs the
    /// same thread AND an enclosing gate frame, not merely proximity — but not
    /// impossible.
    static PENDING_HIT: std::cell::Cell<Option<*const usize>> =
        const { std::cell::Cell::new(None) };
}

/// Parks a damage instance on [`PENDING_HIT`] and restores the PREVIOUS value
/// on drop. SAVE/RESTORE, not set/clear: register-hit calls can nest (a hit
/// that triggers a reaction that registers another hit), and a clear-on-drop
/// would erase the enclosing hit and misattribute everything after it.
///
/// All TLS access goes through `try_with` so a call during thread teardown
/// degrades to "nothing parked" instead of panicking inside the game.
struct HitGuard(Option<*const usize>);

impl HitGuard {
    fn park(damage_instance: *const usize) -> Self {
        HitGuard(
            PENDING_HIT
                .try_with(|c| c.replace(Some(damage_instance)))
                .unwrap_or(None),
        )
    }

    /// The currently parked hit, if any (`None` also covers TLS teardown).
    fn current() -> Option<*const usize> {
        PENDING_HIT.try_with(|c| c.get()).ok().flatten()
    }
}

impl Drop for HitGuard {
    fn drop(&mut self) {
        let _ = PENDING_HIT.try_with(|c| c.set(self.0));
    }
}

/// PRODUCTION detour of the register-hit gate (v2.0.3 entry 0x9adaa0, two
/// static callers): parks the hit's damage instance (`a2`) on [`PENDING_HIT`]
/// for the duration of the original call, so the gauge-update hook — which the
/// game runs as a synchronous callee of this gate — can name the hit that
/// moved the gauge. Forwards both args verbatim; no other side effects.
pub struct OnSBARegisterHitHook;

impl OnSBARegisterHitHook {
    pub fn new() -> Self {
        Self
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        if let Ok(on_sba_register_hit_original) = process.search_address(ON_SBA_REGISTER_HIT_SIG) {
            #[cfg(feature = "console")]
            println!("found on sba register hit");

            unsafe {
                let func: OnSBARegisterHitFunc = std::mem::transmute(on_sba_register_hit_original);
                OnSBARegisterHit.initialize(func, |a1, a2| Self::run(a1, a2))?;
                OnSBARegisterHit.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find sba_register_hit"));
        }

        Ok(())
    }

    fn run(a1: *const usize, a2: *const usize) -> usize {
        #[cfg(feature = "hookdiag")]
        {
            use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, AtomicOrdering::Relaxed) + 1;
            if n <= 4 || n % 256 == 0 {
                log::info!(
                    "SBAGATE n={n} tid={:?} nested={}",
                    std::thread::current().id(),
                    HitGuard::current().is_some(),
                );
            }
        }

        let _guard = HitGuard::park(a2);
        unsafe { OnSBARegisterHit.call(a1, a2) }
    }
}

// ---------------------------------------------------------------------------
// v2.0.2 slot-poll path (remote SBA recovery, derived 2026-07-17)
//
// The remote-SBA-update hook's signature AND the SBA_OFFSET global both died in
// 2.0.2 (log: "Could not find on_remote_sba_update" / "Could not find sba
// offset"), so remote players' gauges are invisible in online lobbies. Instead
// of chasing the network handler, replicate how the game's own party-wide SBA
// appliers (FUN_141b0dd80 / FUN_1431f1f40, xrefs of the verified gauge-update
// entry 0xbb8840) reach EVERY party member's gauge:
//
//   handle_i  = DAT_147036{7f0,808,820,838}      (4 party-slot entity handles,
//               0x18 stride: {u32 index+1, pad, entity*, u64 id})
//   validate    against the entity table DAT_1470214e8 (+0x20 entity array,
//               +0x48 id array, both indexed by index-1)   [FUN_1406d2490]
//   specified = *(entity + 0x70)
//   component = std-map-find(specified + 0xC0, type_id)    [FUN_140936870]
//               where type_id = DAT_147ab3f50 (runtime static-init counter;
//               its _Init_thread guard at +0x54 reads -1 once initialized)
//   gauge     = *(f32*)(component + 0x7C)   (same field the local update hook
//               reads; component+0x10 = specified instance backref)
//
// All of it is plain data walking — replicated below with guarded reads, no
// game code called. SBAPOLL probe first; production events once live-verified.
// ---------------------------------------------------------------------------
/// v2.0.3 RVAs (re-derived 2026-07-31; the 2.0.2 values are in the trailing
/// comments). The patch shifted every data global by -0x3040, but each of these
/// was read back out of the new binary rather than shifted arithmetically:
/// the slot-handle table from the `vmovaps xmm0,[rip+..]` in all three party-wide
/// appliers (they agree), and the entity table + component-type id from the
/// handle→component resolver those appliers call (2.0.2 `FUN_1406d2490`, now at
/// rva 0x6cbb30). The resolver's shape is unchanged — it still validates against
/// the entity table's +0x48 id / +0x20 entity arrays, takes the specified actor
/// at +0x70, and looks the component up in the +0xC0 map.
const SBA_SLOT_HANDLES_RVA: usize = 0x7034c60; // 2.0.4: 0x7034a40
const SBA_SLOT_HANDLE_STRIDE: usize = 0x18;
const ENTITY_TABLE_RVA: usize = 0x701f948; // 2.0.4: 0x701f728
const SBA_COMPONENT_TYPE_RVA: usize = 0x7ab25b0; // 2.0.4: 0x7ab2190
/// Session-mode global: `DAT_147c54810` is a pointer; the game's own online checks
/// read `*(int*)(ptr + 4) == 3` (seen in FUN_143029580 and the result-screen router
/// decompiles). Logged per poll to mark the online→offline transition an AFK
/// conversion causes — the embedded records do NOT flip (2026-07-18 run: allies
/// still read `online=1` during the offline tail), so this global is the candidate
/// production signal for "currently an online lobby".
#[cfg(feature = "hookdiag")]
const SESSION_MODE_PTR_RVA: usize = 0x7c52cd0; // 2.0.4: 0x7c52a50
const ON_ATTEMPT_SBA_SIG: &str = "e8 $ { ' } 48 8d 8e ? ? ff ff c7 44 24 38 00 00 80 3f";
const ON_CHECK_SBA_COLLISION_SIG: &str = "e8 $ { ' } 84 c0 0f 85 f0 00 00 ? 8b 8e ? ? ff ff";
const ON_CONTINUE_SBA_CHAIN_SIG: &str = "e8 $ { ' } 48 8b 53 ? 48 8d 82 ? ? ? ?";
const ON_HANDLE_REMOTE_SBA_UPDATE_SIG: &str =
    "48 8b 8f ? ? ? ? 4c 89 e2 e8 $ { ' } e9 ? ? ? ? 48 81 c7 ? ? ? ? 48 89 f9";

/// MSVC `std::map<u32, ptr>` find, replicating the game's component-by-type
/// lookup FUN_140936870 (main tree only; the fallback tree behind the spinlock
/// is skipped — a miss is just a skipped poll tick). Node layout: left @ +0x00,
/// right @ +0x10, is_nil @ +0x19, key @ +0x20, value @ +0x28; head node at
/// map+0x10, root at head+0x08. Guarded reads, bounded depth.
pub(super) fn game_stdmap_find(map: usize, key: u32) -> Option<usize> {
    use crate::hooks::diag::{read_ptr_guarded, read_u32_guarded};

    let head = read_ptr_guarded(map, 0x10)?;
    let mut node = read_ptr_guarded(head, 0x08)?;
    let mut best = head;
    for _ in 0..64 {
        // is_nil byte at +0x19 (read via the u32 at +0x18)
        if (read_u32_guarded(node, 0x18) >> 8) & 0xFF != 0 {
            break;
        }
        if key <= read_u32_guarded(node, 0x20) {
            best = node;
            node = read_ptr_guarded(node, 0x00)?;
        } else {
            node = read_ptr_guarded(node, 0x10)?;
        }
    }
    if best != head && read_u32_guarded(best, 0x20) <= key {
        read_ptr_guarded(best, 0x28).filter(|v| *v != 0)
    } else {
        None
    }
}

/// Reads the poll preconditions shared by the diag probe and the production
/// poll: module base, validated component-type id, and the entity table.
///
/// The component-type id is assigned by a C++ static-init counter at
/// runtime. Ghidra (FUN_1406d2490 decompile, 2026-07-18): the guard dword at
/// +0x54 follows the MSVC _Init_thread protocol — 0 = never initialized,
/// -1 = initialization IN PROGRESS, and on completion _Init_thread_footer
/// stamps it with the global init epoch (which STARTS at 0x80000000, so an
/// initialized guard reads 0x8000xxxx, e.g. the live-observed 0x800016e8).
/// The old `guard == -1` test was exactly backwards and made every poll bail.
#[cfg_attr(not(feature = "hookdiag"), allow(unused_variables))]
fn poll_context(log_failures: bool) -> Option<(usize, usize, u32)> {
    use crate::hooks::diag::{read_ptr_guarded, read_u32_guarded, MODULE_BASE};

    let base = MODULE_BASE.load(std::sync::atomic::Ordering::Relaxed);
    if base == 0 {
        return None;
    }
    let type_guard = read_u32_guarded(base, SBA_COMPONENT_TYPE_RVA + 4);
    if type_guard == 0 || type_guard == 0xFFFF_FFFF {
        #[cfg(feature = "hookdiag")]
        if log_failures {
            log::info!("SBAPOLL type-id not initialized (guard={type_guard:#x})");
        }
        return None;
    }
    let type_id = read_u32_guarded(base, SBA_COMPONENT_TYPE_RVA);
    let entity_table = read_ptr_guarded(base, ENTITY_TABLE_RVA)?;
    Some((base, entity_table, type_id))
}

/// Resolves one party slot's handle to its member's SBA component (the single
/// slot walk shared by the diag probe and the production poll — these offsets
/// break on game patches and MUST stay one implementation): read the
/// slot-handle, validate it against the entity table like FUN_1406d2490 does,
/// deref the entity's specified-actor (+0x70), then find the SBA component in
/// its component map (+0xC0). Returns `(entity, id, specified, component)`.
/// Every read is SEH-guarded; any failed step resolves the slot to `None`.
#[cfg_attr(not(feature = "hookdiag"), allow(unused_variables))]
fn resolve_slot_component(
    base: usize,
    entity_table: usize,
    type_id: u32,
    slot: usize,
    log_failures: bool,
) -> Option<(usize, usize, usize, usize)> {
    use crate::hooks::diag::{read_ptr_guarded, read_u32_guarded};

    let handle = base + SBA_SLOT_HANDLES_RVA + slot * SBA_SLOT_HANDLE_STRIDE;
    let index_plus_1 = read_u32_guarded(handle, 0x00);
    if index_plus_1 == 0 {
        return None;
    }
    let entity = read_ptr_guarded(handle, 0x08)?;
    let id = read_ptr_guarded(handle, 0x10).unwrap_or(0);

    // Validate the handle against the entity table like FUN_1406d2490 does.
    let idx = (index_plus_1 - 1) as usize;
    let ids = read_ptr_guarded(entity_table, 0x48).unwrap_or(0);
    let ents = read_ptr_guarded(entity_table, 0x20).unwrap_or(0);
    let id_ok = ids != 0 && read_ptr_guarded(ids, idx * 8) == Some(id);
    let ent_ok = ents != 0 && read_ptr_guarded(ents, idx * 8) == Some(entity);
    if !id_ok || !ent_ok || entity == 0 {
        #[cfg(feature = "hookdiag")]
        if log_failures {
            log::info!(
                "SBAPOLL slot={slot} stale handle (idx={index_plus_1} id_ok={id_ok} ent_ok={ent_ok})"
            );
        }
        return None;
    }

    let specified = read_ptr_guarded(entity, 0x70).filter(|p| *p != 0)?;
    let Some(component) = game_stdmap_find(specified + 0xC0, type_id) else {
        #[cfg(feature = "hookdiag")]
        if log_failures {
            log::info!("SBAPOLL slot={slot} specified={specified:#x} component MISS (type_id={type_id:#x})");
        }
        return None;
    };
    Some((entity, id, specified, component))
}

/// hookdiag: poll all four party slots' SBA gauges via the slot-handle table
/// (see the module comment above) and log one `SBAPOLL` line per resolvable
/// slot, including the actor's embedded-record identity so gauge values are
/// attributable per player even when two players run the same character.
/// Called from the (working, local) gauge-update hook — rate-limited.
#[cfg(feature = "hookdiag")]
fn log_sba_slot_poll() {
    use crate::hooks::diag::{read_f32_guarded, read_ptr_guarded, read_u32_guarded};
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    static CALLS: AtomicU32 = AtomicU32::new(0);
    let call = CALLS.fetch_add(1, AtomicOrdering::Relaxed);
    if call >= 8 && call % 64 != 0 {
        return;
    }

    let Some((base, entity_table, type_id)) = poll_context(true) else {
        return;
    };

    for slot in 0..4usize {
        let Some((entity, id, specified, component)) =
            resolve_slot_component(base, entity_table, type_id, slot, true)
        else {
            continue;
        };
        let gauge = read_f32_guarded(component, 0x7C).unwrap_or(f32::NAN);
        let backref = read_ptr_guarded(component, 0x10).unwrap_or(0);
        let idx170 = read_u32_guarded(specified, 0x170);
        // Session mode (see SESSION_MODE_PTR_RVA): 3 = online per the game's own
        // checks; expect it to CHANGE when an AFK conversion drops the lobby offline.
        let session_mode = read_ptr_guarded(base, SESSION_MODE_PTR_RVA)
            .map(|p| read_u32_guarded(p, 4) as i64)
            .unwrap_or(-1);
        let (party, online, name) = match super::player::actor_embedded_identity(specified) {
            Some((party, online, name)) => (party as i32, online as i32, name),
            None => (-1, -1, "<unresolved>".to_string()),
        };
        log::info!(
            "SBAPOLL t={} slot={slot} entity={entity:#x} specified={specified:#x} comp={component:#x} \
             backref={backref:#x} id={id:#x} gauge={gauge:.1} idx170={idx170:#x} mode={session_mode} \
             party={party} online={online} name={name}",
            crate::hooks::diag::ms(),
        );
    }
}

/// Last emitted gauge per party slot, so the per-tick poll only emits real
/// changes (keeps event volume sane at the gauge hook's firing rate). -1.0 =
/// never seen, so the first resolvable poll emits the current value.
static LAST_SLOT_GAUGE: std::sync::Mutex<[f32; 4]> = std::sync::Mutex::new([-1.0; 4]);

/// PRODUCTION remote-SBA recovery (Ghidra-derived 2026-07-17/18, live-verified
/// via the SBAPOLL probe on an online lobby): walk the game's own four
/// party-slot entity handles to each member's SBA component and emit slot-keyed
/// gauge events. This replaces the per-entity emission of the (local-only)
/// gauge-update hook — online, that hook fires only for the local player, while
/// this poll reads every member's (synced) gauge. Slot-keyed so the parser's
/// party rows join damage, SBA and stun on the same per-player index.
///
/// Every read is SEH-guarded; a failed step just skips that slot this tick.
fn poll_slots_and_emit(tx: &event::Tx) {
    use crate::hooks::diag::read_f32_guarded;

    let Some((base, entity_table, type_id)) = poll_context(false) else {
        return;
    };
    // try_lock: the gauge hook can fire from the game thread only, but never
    // risk blocking it on a poisoned/contended lock.
    let Ok(mut last) = LAST_SLOT_GAUGE.try_lock() else {
        return;
    };

    for slot in 0..4usize {
        let Some((_entity, _id, _specified, component)) =
            resolve_slot_component(base, entity_table, type_id, slot, false)
        else {
            continue;
        };
        let Some(gauge) = read_f32_guarded(component, 0x7C).filter(|g| g.is_finite()) else {
            continue;
        };

        let previous = last[slot];
        if previous >= 0.0 && (gauge - previous).abs() < 0.05 {
            continue;
        }
        last[slot] = gauge;

        let actor_index = super::player::slot_key(slot as u8);
        if gauge == 0.0 && previous > 0.0 {
            let _ = tx.send(Message::OnPerformSBA(protocol::OnPerformSBAEvent {
                actor_index,
            }));
        }
        let _ = tx.send(Message::OnUpdateSBA(protocol::OnUpdateSBAEvent {
            actor_index,
            sba_value: gauge,
            sba_added: (gauge - previous.max(0.0)).max(0.0),
        }));
    }
}

/// Stable tags for grant sites that have no gameplay name yet — the payload of
/// `SbaGainCause::Site`. Never renumber: a stored log carries these.
///
/// NOT hooked, and why: v2.0.3 has one more `UNCONDITIONAL_CALL` into the
/// gauge wrapper, at rva 0x3940da3, that Ghidra attributes to no function.
/// FindEntry on the analyzed DB (2026-08-04) found no containing function and
/// its backward-prologue guess (0x3940660) did not hold up, so there is no
/// verified entry to detour — a detour on a guessed entry is how the game
/// crashes. Rises reaching the gauge update from that site land in
/// `SbaGainCause::Unknown`, whose amounts the SBAUNK hookdiag line logs; if
/// live data shows a persistent Unknown bucket, that call site is the first
/// place to look.
pub(crate) mod site {
    pub const VTABLE_GRANT_191BE40: u32 = 1;
    pub const VTABLE_GRANT_33BBC20: u32 = 2;
    pub const VTABLE_GRANT_33BC050: u32 = 3;
    /// The NETWORK gauge-grant message handler (see [`super::OnNetGaugeGrantHook`]).
    /// A site rather than a name because what the message means in gameplay
    /// terms is not established yet — only that the grant arrived over the
    /// wire instead of being computed locally.
    pub const NET_GAUGE_GRANT: u32 = 4;
}

thread_local! {
    /// The named grant site currently executing on this thread, if any.
    ///
    /// Same contract as [`PENDING_HIT`]: the game's gauge update runs as a
    /// synchronous callee of these sites, so whatever is parked when the gauge
    /// moves is what moved it. SAVE/RESTORE rather than set/clear because sites
    /// can nest (a guard that triggers an effect that grants again), and a
    /// clear-on-drop would erase the enclosing cause.
    static PENDING_CAUSE: std::cell::Cell<Option<protocol::SbaGainCause>> =
        const { std::cell::Cell::new(None) };
}

/// Parks a cause on [`PENDING_CAUSE`] and restores the previous value on drop.
/// All TLS access goes through `try_with`, so a call during thread teardown
/// degrades to "nothing parked" instead of panicking inside the game.
struct CauseGuard(Option<protocol::SbaGainCause>);

impl CauseGuard {
    fn park(cause: protocol::SbaGainCause) -> Self {
        CauseGuard(
            PENDING_CAUSE
                .try_with(|c| c.replace(Some(cause)))
                .unwrap_or(None),
        )
    }

    fn current() -> Option<protocol::SbaGainCause> {
        PENDING_CAUSE.try_with(|c| c.get()).ok().flatten()
    }
}

impl Drop for CauseGuard {
    fn drop(&mut self) {
        let _ = PENDING_CAUSE.try_with(|c| c.set(self.0));
    }
}

/// hookdiag: one `SBASITE <name> n=<n>` line per grant-site fire (rate-limited),
/// so a live round can say WHICH detours fired — the aggregate SBAGAIN "named"
/// bucket cannot. Live round 1 (2026-08-04) needed exactly this to tell whether
/// Perfect Guard grants were absent or merely masked by the generic percent
/// API's inner `Effect(0)` guard.
#[cfg(feature = "hookdiag")]
fn log_site_fire(name: &str, counter: &std::sync::atomic::AtomicU32) {
    use std::sync::atomic::Ordering as AtomicOrdering;
    let n = counter.fetch_add(1, AtomicOrdering::Relaxed) + 1;
    if n <= 8 || n % 16 == 0 {
        log::info!("SBASITE {name} n={n}");
    }
}

// ---------------------------------------------------------------------------
// Grant-site detours (v2.0.3 caller map of the gauge update, derived
// 2026-08-04): pass-through hooks on the named routes into the gauge update.
// Each parks its cause on PENDING_CAUSE for the duration of the original call
// and forwards every argument verbatim; the gauge-update hook reads the cause
// back (see `resolve_cause`). Every signature below is DIRECT-ENTRY, confirmed
// as a function entry with the stated arity (Ghidra InspectFunc + prologue,
// 2026-08-04) and verified to exactly 1 sigscan match — a wrong-arity detour
// leaves a register garbage and takes the game down with it.
// ---------------------------------------------------------------------------

/// Just-guard grant site (v2.0.3 entry 0x1f36f70; strings `core_pl_just_guard`,
/// `core_pl_just_guard_frend`). Reads a `{gauge%, other%}` pair from a
/// hash-keyed effect record and grants the first through the gauge wrapper, so
/// the gauge update runs as a synchronous callee of this frame. Prologue reads
/// rcx+rdx only (`mov rdi,rdx / mov rsi,rcx`) — arity 2.
type OnJustGuardGrantFunc = unsafe extern "system" fn(*const usize, *const usize) -> usize;

/// Anchored on the tail of the small preceding function (`mov al,1 / jmp`),
/// cursor at the entry. sigscan 2026-08-04: exactly 1 match, cursor=0x1f36f70.
const ON_JUST_GUARD_GRANT_SIG: &str = "b0 01 eb ea ' 55 41 57 41 56 56 57 53 48 81 ec 68 04 00 00 48 8d ac 24 80 00 00 00 c5 78 29 85 d0 03 00 00";

/// Effect-record grant (v2.0.3 entry 0x26f9640), called from 0x9b31b0 in the
/// damage-side chain; grants from a hash-keyed `{sba%, other%}` record. The
/// leading candidate for damage-taken and sigil-driven awards (Nimble
/// Onslaught). Prologue reads rcx only (`mov rsi,rcx`, then rcx fields) —
/// arity 1.
type OnEffectGrantFunc = unsafe extern "system" fn(*const usize) -> usize;

/// int3 padding + prologue through the xmm10 spill. sigscan 2026-08-04:
/// exactly 1 match, cursor=0x26f9640.
const ON_EFFECT_GRANT_SIG: &str = "cc cc cc cc ' 55 41 56 56 57 53 48 81 ec 30 02 00 00 48 8d ac 24 80 00 00 00 c5 78 29 95 a0 01 00 00 c5 78 29 8d 90 01 00 00";

/// Generic "add gauge %" API (v2.0.3 entry 0xbcaa90; `param_2 * 100 * scale`,
/// rounded), callers 0x9d5420 / 0xb94800. The highest-arity site here and the
/// easiest to get wrong: 8 args, the second in xmm1 — the declaration mirrors
/// how `OnSBAUpdateFunc` mixes pointer and float args under the MS x64 ABI.
/// Prologue reads r8d+r9d early (`mov ebx,r9d / mov edi,r8d`) and the stack
/// args in the body.
type OnGaugePercentGrantFunc =
    unsafe extern "system" fn(*const usize, f32, u8, u8, u8, u32, u8, u8) -> usize;

/// Anchored on the tail of the preceding function (`jmp` short + int3), cursor
/// at the entry. sigscan 2026-08-04: exactly 1 match, cursor=0xbcaa90.
const ON_GAUGE_PERCENT_GRANT_SIG: &str = "eb d3 cc ' 55 41 56 56 57 53 48 81 ec 90 01 00 00 48 8d ac 24 80 00 00 00 c5 f8 29 b5 00 01 00 00 48 c7 85 f8 00 00 00 fe ff ff ff 44 89 cb 44 89 c7";

/// Quest-start / per-slot initial gauge (v2.0.3 entry 0x6c50f0; strings
/// `PlayerBahamut`, `PlayerNPC`, `pl_solo_type`). Fires once per quest load.
/// Prologue reads edx+r8d+r9d (`mov r14d,r9d / mov r15d,r8d / mov r12d,edx`)
/// plus stack args — arity 8 per the decompile.
type OnQuestStartGaugeFunc = unsafe extern "system" fn(
    *const usize,
    u32,
    u32,
    u32,
    *const usize,
    *const u8,
    u8,
    u32,
) -> usize;

/// int3 padding + prologue through the register moves. sigscan 2026-08-04:
/// exactly 1 match, cursor=0x6c50f0.
const ON_QUEST_START_GAUGE_SIG: &str = "cc cc cc cc ' 55 41 57 41 56 41 55 41 54 56 57 53 48 81 ec 48 01 00 00 48 8d ac 24 80 00 00 00 48 c7 85 c0 00 00 00 fe ff ff ff 45 89 ce 45 89 c7 41 89 d4";

/// Quest-start gauge, single-player variant (v2.0.3 entry 0x6c4910; string
/// `Player_{}`). Prologue reads ecx+rdx+r8d+r9d (`mov ebx,r8d / mov rdi,rdx /
/// mov r14d,ecx / cmp r9d,-1`) — arity 4.
type OnQuestStartGaugeSoloFunc = unsafe extern "system" fn(u32, *const usize, u8, u32) -> usize;

/// int3 padding + prologue through the `cmp r9d,-1`. sigscan 2026-08-04:
/// exactly 1 match, cursor=0x6c4910.
const ON_QUEST_START_GAUGE_SOLO_SIG: &str = "cc cc cc cc ' 55 41 57 41 56 56 57 53 48 81 ec e8 00 00 00 48 8d ac 24 80 00 00 00 48 c7 45 60 fe ff ff ff 44 89 c3 48 89 d7 41 89 ce 41 83 f9 ff";

/// Unnamed vtable-registered % grant (v2.0.3 entry 0x191be40; no strings, no
/// gameplay identity yet — hence `Site(VTABLE_GRANT_191BE40)`). Prologue reads
/// rcx only (`mov edx,[rcx+0x10]`) — arity 1.
type OnVtableGrant191be40Func = unsafe extern "system" fn(*const usize) -> usize;

/// Padding + prologue, stopping BEFORE the RIP-relative `mov rbx,[rip+..]` so
/// no displacement byte is baked in. sigscan 2026-08-04: exactly 1 match,
/// cursor=0x191be40.
const ON_VTABLE_GRANT_191BE40_SIG: &str = "c3 cc cc cc cc cc cc ' 56 57 48 83 ec 68 c5 f8 29 74 24 50 8b 51 10 85 d2 0f 84 a3 01 00 00 48 8b 41 18 4c 8b 41 20";

/// Unnamed % grant (v2.0.3 entry 0x33bbc20). Prologue reads rcx+rdx
/// (`mov rbx,rdx / mov rdi,rcx`) — arity 2.
type OnVtableGrant33bbc20Func = unsafe extern "system" fn(*const usize, *const usize) -> usize;

/// Padding + prologue through the `mov r14d,[rcx+0x171d8]` field read.
/// sigscan 2026-08-04: exactly 1 match, cursor=0x33bbc20.
const ON_VTABLE_GRANT_33BBC20_SIG: &str = "c3 cc cc cc cc cc ' 41 56 56 57 53 48 83 ec 68 c5 f8 29 74 24 50 48 89 d3 48 89 cf 48 8d b1 a0 be ff ff 44 8b b1 d8 71 01 00";

/// Unnamed % grant (v2.0.3 entry 0x33bc050). Prologue reads rcx only
/// (`mov rsi,rcx`, then rcx fields) — arity 1.
type OnVtableGrant33bc050Func = unsafe extern "system" fn(*const usize) -> usize;

/// Padding + prologue through the `[rsi+0xc228]` lea. sigscan 2026-08-04:
/// exactly 1 match, cursor=0x33bc050.
const ON_VTABLE_GRANT_33BC050_SIG: &str = "cc ' 56 48 83 ec 60 c5 f8 29 74 24 50 48 89 ce 48 8b 89 20 c2 00 00 48 85 c9 0f 84 ec 00 00 00 48 8d 86 28 c2 00 00";

static_detour! {
    static OnJustGuardGrant: unsafe extern "system" fn(*const usize, *const usize) -> usize;
    static OnEffectGrant: unsafe extern "system" fn(*const usize) -> usize;
    static OnGaugePercentGrant: unsafe extern "system" fn(*const usize, f32, u8, u8, u8, u32, u8, u8) -> usize;
    static OnQuestStartGauge: unsafe extern "system" fn(*const usize, u32, u32, u32, *const usize, *const u8, u8, u32) -> usize;
    static OnQuestStartGaugeSolo: unsafe extern "system" fn(u32, *const usize, u8, u32) -> usize;
    static OnVtableGrant191be40: unsafe extern "system" fn(*const usize) -> usize;
    static OnVtableGrant33bbc20: unsafe extern "system" fn(*const usize, *const usize) -> usize;
    static OnVtableGrant33bc050: unsafe extern "system" fn(*const usize) -> usize;
}

/// PRODUCTION detour of the just-guard gauge grant: parks
/// [`protocol::SbaGainCause::PerfectGuard`] for the duration of the original
/// call. Forwards both args verbatim; no other side effects.
pub struct OnJustGuardGrantHook;

impl OnJustGuardGrantHook {
    pub fn new() -> Self {
        Self
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        if let Ok(original) = process.search_address(ON_JUST_GUARD_GRANT_SIG) {
            #[cfg(feature = "console")]
            println!("found on just guard grant");

            unsafe {
                let func: OnJustGuardGrantFunc = std::mem::transmute(original);
                OnJustGuardGrant.initialize(func, |a1, a2| Self::run(a1, a2))?;
                OnJustGuardGrant.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find just_guard_grant"));
        }

        Ok(())
    }

    fn run(a1: *const usize, a2: *const usize) -> usize {
        #[cfg(feature = "hookdiag")]
        {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            log_site_fire("just_guard", &N);
        }
        let _guard = CauseGuard::park(protocol::SbaGainCause::PerfectGuard);
        unsafe { OnJustGuardGrant.call(a1, a2) }
    }
}

/// PRODUCTION detour of the effect-record gauge grant: parks
/// [`protocol::SbaGainCause::Effect`] for the duration of the original call.
/// Forwards its arg verbatim; no other side effects.
pub struct OnEffectGrantHook;

impl OnEffectGrantHook {
    pub fn new() -> Self {
        Self
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        if let Ok(original) = process.search_address(ON_EFFECT_GRANT_SIG) {
            #[cfg(feature = "console")]
            println!("found on effect grant");

            unsafe {
                let func: OnEffectGrantFunc = std::mem::transmute(original);
                OnEffectGrant.initialize(func, |a1| Self::run(a1))?;
                OnEffectGrant.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find effect_grant"));
        }

        Ok(())
    }

    fn run(a1: *const usize) -> usize {
        #[cfg(feature = "hookdiag")]
        {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            log_site_fire("effect_record", &N);
        }
        // This function (v2.0.3 `FUN_1426f9640`) IS the just-dodge reward
        // handler — it fires the `core_pl_just_dodge` cue, and of its four
        // constant-keyed effect-map walks only the `0xD2C8E10A` one feeds the
        // gauge (`record+0x8` % -> `FUN_140bb1c00(entity+0x3230, ..)`; the
        // others are status applies, a recovery, and a cooldown decrement —
        // decompiles of 0x9ee8a0 / 0x21ec130, 2026-08-04). Live 2026-08-04
        // (log 1694): ~42.15 gauge per perfect dodge through this frame. So
        // the frame's semantic, not the record key, is the honest cause.
        // (Logs stored before this park carry `Effect(0xD2C8E10A)` — the
        // frontend maps that key to the same "Perfect Dodge" label.)
        let _guard = CauseGuard::park(protocol::SbaGainCause::PerfectDodge);
        unsafe { OnEffectGrant.call(a1) }
    }
}

/// PRODUCTION detour of the generic "add gauge %" API: parks
/// [`protocol::SbaGainCause::Effect`] for the duration of the original call.
/// If a more specific site (just-guard, effect-record) also fired for the same
/// rise, its guard nests INSIDE this one and wins — the correct precedence.
/// Forwards all eight args verbatim; no other side effects.
pub struct OnGaugePercentGrantHook;

impl OnGaugePercentGrantHook {
    pub fn new() -> Self {
        Self
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        if let Ok(original) = process.search_address(ON_GAUGE_PERCENT_GRANT_SIG) {
            #[cfg(feature = "console")]
            println!("found on gauge percent grant");

            unsafe {
                let func: OnGaugePercentGrantFunc = std::mem::transmute(original);
                OnGaugePercentGrant.initialize(func, |a1, a2, a3, a4, a5, a6, a7, a8| {
                    Self::run(a1, a2, a3, a4, a5, a6, a7, a8)
                })?;
                OnGaugePercentGrant.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find gauge_percent_grant"));
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn run(a1: *const usize, a2: f32, a3: u8, a4: u8, a5: u8, a6: u32, a7: u8, a8: u8) -> usize {
        #[cfg(feature = "hookdiag")]
        {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            log_site_fire("percent_api", &N);
        }
        // Park only when no more specific site already parked one: this is the
        // shared entry point, and live round 1 showed the just-guard site's
        // PerfectGuard being overwritten here — the generic API is the INNER
        // frame, so parking unconditionally masks every caller that named
        // itself. An enclosing cause always outranks the generic bucket.
        let _guard = match CauseGuard::current() {
            None => Some(CauseGuard::park(protocol::SbaGainCause::Effect(0))),
            Some(_) => None,
        };
        unsafe { OnGaugePercentGrant.call(a1, a2, a3, a4, a5, a6, a7, a8) }
    }
}

/// PRODUCTION detour of the quest-start / per-slot initial gauge award: parks
/// [`protocol::SbaGainCause::QuestStart`] for the duration of the original
/// call. Fires once per quest load. Forwards all args verbatim.
pub struct OnQuestStartGaugeHook;

impl OnQuestStartGaugeHook {
    pub fn new() -> Self {
        Self
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        if let Ok(original) = process.search_address(ON_QUEST_START_GAUGE_SIG) {
            #[cfg(feature = "console")]
            println!("found on quest start gauge");

            unsafe {
                let func: OnQuestStartGaugeFunc = std::mem::transmute(original);
                OnQuestStartGauge.initialize(func, |a1, a2, a3, a4, a5, a6, a7, a8| {
                    Self::run(a1, a2, a3, a4, a5, a6, a7, a8)
                })?;
                OnQuestStartGauge.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find quest_start_gauge"));
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn run(
        a1: *const usize,
        a2: u32,
        a3: u32,
        a4: u32,
        a5: *const usize,
        a6: *const u8,
        a7: u8,
        a8: u32,
    ) -> usize {
        #[cfg(feature = "hookdiag")]
        {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            log_site_fire("quest_start", &N);
        }
        let _guard = CauseGuard::park(protocol::SbaGainCause::QuestStart);
        unsafe { OnQuestStartGauge.call(a1, a2, a3, a4, a5, a6, a7, a8) }
    }
}

/// PRODUCTION detour of the single-player quest-start gauge award: parks
/// [`protocol::SbaGainCause::QuestStart`] for the duration of the original
/// call. Forwards all args verbatim.
pub struct OnQuestStartGaugeSoloHook;

impl OnQuestStartGaugeSoloHook {
    pub fn new() -> Self {
        Self
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        if let Ok(original) = process.search_address(ON_QUEST_START_GAUGE_SOLO_SIG) {
            #[cfg(feature = "console")]
            println!("found on quest start gauge solo");

            unsafe {
                let func: OnQuestStartGaugeSoloFunc = std::mem::transmute(original);
                OnQuestStartGaugeSolo
                    .initialize(func, |a1, a2, a3, a4| Self::run(a1, a2, a3, a4))?;
                OnQuestStartGaugeSolo.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find quest_start_gauge_solo"));
        }

        Ok(())
    }

    fn run(a1: u32, a2: *const usize, a3: u8, a4: u32) -> usize {
        #[cfg(feature = "hookdiag")]
        {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            log_site_fire("quest_start_solo", &N);
        }
        let _guard = CauseGuard::park(protocol::SbaGainCause::QuestStart);
        unsafe { OnQuestStartGaugeSolo.call(a1, a2, a3, a4) }
    }
}

/// PRODUCTION detour of the unnamed vtable grant at 0x191be40: parks
/// [`protocol::SbaGainCause::Site`] with its stable tag. Live data showing WHEN
/// it fires is what will earn it a real name.
pub struct OnVtableGrant191be40Hook;

impl OnVtableGrant191be40Hook {
    pub fn new() -> Self {
        Self
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        if let Ok(original) = process.search_address(ON_VTABLE_GRANT_191BE40_SIG) {
            #[cfg(feature = "console")]
            println!("found on vtable grant 191be40");

            unsafe {
                let func: OnVtableGrant191be40Func = std::mem::transmute(original);
                OnVtableGrant191be40.initialize(func, |a1| Self::run(a1))?;
                OnVtableGrant191be40.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find vtable_grant_191be40"));
        }

        Ok(())
    }

    fn run(a1: *const usize) -> usize {
        #[cfg(feature = "hookdiag")]
        {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            log_site_fire("vtable_191be40", &N);
        }
        let _guard = CauseGuard::park(protocol::SbaGainCause::Site(site::VTABLE_GRANT_191BE40));
        unsafe { OnVtableGrant191be40.call(a1) }
    }
}

/// PRODUCTION detour of the unnamed vtable grant at 0x33bbc20 (see the
/// 0x191be40 hook).
pub struct OnVtableGrant33bbc20Hook;

impl OnVtableGrant33bbc20Hook {
    pub fn new() -> Self {
        Self
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        if let Ok(original) = process.search_address(ON_VTABLE_GRANT_33BBC20_SIG) {
            #[cfg(feature = "console")]
            println!("found on vtable grant 33bbc20");

            unsafe {
                let func: OnVtableGrant33bbc20Func = std::mem::transmute(original);
                OnVtableGrant33bbc20.initialize(func, |a1, a2| Self::run(a1, a2))?;
                OnVtableGrant33bbc20.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find vtable_grant_33bbc20"));
        }

        Ok(())
    }

    fn run(a1: *const usize, a2: *const usize) -> usize {
        #[cfg(feature = "hookdiag")]
        {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            log_site_fire("vtable_33bbc20", &N);
        }
        let _guard = CauseGuard::park(protocol::SbaGainCause::Site(site::VTABLE_GRANT_33BBC20));
        unsafe { OnVtableGrant33bbc20.call(a1, a2) }
    }
}

/// PRODUCTION detour of the unnamed vtable grant at 0x33bc050 (see the
/// 0x191be40 hook).
pub struct OnVtableGrant33bc050Hook;

impl OnVtableGrant33bc050Hook {
    pub fn new() -> Self {
        Self
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        if let Ok(original) = process.search_address(ON_VTABLE_GRANT_33BC050_SIG) {
            #[cfg(feature = "console")]
            println!("found on vtable grant 33bc050");

            unsafe {
                let func: OnVtableGrant33bc050Func = std::mem::transmute(original);
                OnVtableGrant33bc050.initialize(func, |a1| Self::run(a1))?;
                OnVtableGrant33bc050.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find vtable_grant_33bc050"));
        }

        Ok(())
    }

    fn run(a1: *const usize) -> usize {
        #[cfg(feature = "hookdiag")]
        {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            log_site_fire("vtable_33bc050", &N);
        }
        let _guard = CauseGuard::park(protocol::SbaGainCause::Site(site::VTABLE_GRANT_33BC050));
        unsafe { OnVtableGrant33bc050.call(a1) }
    }
}

// ---------------------------------------------------------------------------
// NETWORK gauge grant (v2.0.4, derived 2026-08-08)
//
// The one route into the gauge that does NOT start from locally-simulated
// gameplay. It is a member of the game's network message-handler family — the
// same family as the network stun-apply handler (see `stunnet.rs`): one arg,
// the message; resolve `*(u64*)(msg+0x10)` through the net entity-handle map
// (`FUN_140269520`, rva 0x269520); validate the handle against the entity
// table (`DAT_14701f728`, +0x48 ids / +0x20 entities); take the specified
// actor at `entity+0x70`; then call the generic "add gauge %" API:
//
//   FUN_140bcba30(actor, *(f32*)(msg+0x18), *(u8*)(msg+0x1e) == 0,
//                 *(u8*)(msg+0x1c), *(u8*)(msg+0x1d), *(u32*)(msg+0x20),
//                 *(u8*)(msg+0x24), *(u8*)(msg+0x25))
//
// which is the very function `OnGaugePercentGrantHook` detours, so the gauge
// update runs as a synchronous callee of this frame and the parked cause is
// read back by `resolve_cause` exactly like every other grant site.
//
// HOW IT WAS FOUND, since none of it is reachable by the usual route: the
// handler has no code xrefs at all (its only reference is its own `.pdata`
// RUNTIME_FUNCTION), because it is a switch-case target and the fast-analysis
// DB has Decompiler Switch Analysis disabled. Enumerating callers of the net
// entity resolver instead yields the whole handler family, and this is the one
// member that also calls the gauge-percent API.
//
// WHAT IS NOT ESTABLISHED, and must come from a live online round before
// anything is named or built on top:
//   * whose gauge these messages move — the local player (a host-computed
//     party award), remote members, or both. The handler grants to whatever
//     entity the message names, and static code cannot say which are sent.
//   * what the u32 at `msg+0x20` means. It is forwarded through the percent
//     API into the gauge update's third argument, which gates that function's
//     sigil/stat scaling block — so it reads as a grant-KIND enum rather than
//     an action id. The `SBAGAIN` diagnostic now logs it (`a3`) so live data
//     can settle it.
// Until then this parks a numbered SITE, which separates these rises out of
// the generic `Effect(0)` bucket without asserting a meaning for them.
//
// NOTE for the open "Effect(0) tracks ally SBA use" lead: this is a mechanism
// that would produce exactly that. A host-side award arriving here lands on
// the percent API with nothing more specific parked, which is precisely the
// residual bucket. Suggestive, NOT confirmed — the live round decides.
// ---------------------------------------------------------------------------

/// The network gauge-grant handler (v2.0.4 entry 0xb957a0). Arity 1: the
/// prologue reads rcx only (`mov rsi,rcx`, then `mov rcx,[rcx+0x10]`), with no
/// rdx/r8/r9 touched — Ghidra InspectFunc + prologue disasm, 2026-08-08.
type OnNetGaugeGrantFunc = unsafe extern "system" fn(*const usize) -> usize;

/// Direct-entry signature: the preceding `int3` padding plus the prologue
/// through the `lea rdx,[rsp+0x40]`, stopping BEFORE the relative call so no
/// displacement is baked in. sigscan 2026-08-08: exactly 1 match,
/// cursor_rva=0xb957a0.
const ON_NET_GAUGE_GRANT_SIG: &str = "cc cc cc cc ' 56 57 48 83 ec 58 48 89 ce c7 44 24 40 00 00 00 00 c5 f8 57 c0 c5 f8 11 44 24 48 48 8b 49 10 48 8d 54 24 40";

/// Message-relative offsets (from the FUN_140b957a0 decompile). Read by the
/// `SBANET` diagnostic only — production forwards the message untouched.
#[cfg(feature = "hookdiag")]
mod net_grant_msg {
    /// u64 net entity-handle id of the player whose gauge this moves.
    pub const TARGET_ID: usize = 0x10;
    /// f32 percent, the amount handed to the gauge-percent API.
    pub const PERCENT: usize = 0x18;
    /// u32 forwarded into the gauge update's scaling-gate argument.
    pub const KIND: usize = 0x20;
}

static_detour! {
    static OnNetGaugeGrant: unsafe extern "system" fn(*const usize) -> usize;
}

/// PRODUCTION detour of the network gauge-grant handler: parks
/// [`protocol::SbaGainCause::Site`] with [`site::NET_GAUGE_GRANT`] for the
/// duration of the original call. Forwards its one arg verbatim; no other
/// side effects. See the block comment above for what this is and what about
/// it is still open.
pub struct OnNetGaugeGrantHook;

impl OnNetGaugeGrantHook {
    pub fn new() -> Self {
        Self
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        if let Ok(original) = process.search_address(ON_NET_GAUGE_GRANT_SIG) {
            #[cfg(feature = "console")]
            println!("found on net gauge grant");

            unsafe {
                let func: OnNetGaugeGrantFunc = std::mem::transmute(original);
                OnNetGaugeGrant.initialize(func, |a1| Self::run(a1))?;
                OnNetGaugeGrant.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find net_gauge_grant"));
        }

        Ok(())
    }

    fn run(msg: *const usize) -> usize {
        // The message's own fields, which the parked cause cannot carry: whose
        // gauge, how much, and the unidentified kind word. This is the line the
        // live round reads to decide whether remote members' gauge arrives here
        // — and therefore whether per-cause attribution is possible for them.
        #[cfg(feature = "hookdiag")]
        {
            use crate::hooks::diag::{read_f32_guarded, read_u32_guarded, read_u64_guarded};
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            static DETAIL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            log_site_fire("net_gauge_grant", &N);
            if crate::hooks::diag::first_n(&DETAIL, 32) {
                log::info!(
                    "SBANET target_id={:?} percent={:?} kind={:#x}",
                    read_u64_guarded(msg as usize, net_grant_msg::TARGET_ID),
                    read_f32_guarded(msg as usize, net_grant_msg::PERCENT),
                    read_u32_guarded(msg as usize, net_grant_msg::KIND),
                );
            }
        }

        let _guard = CauseGuard::park(protocol::SbaGainCause::Site(site::NET_GAUGE_GRANT));
        unsafe { OnNetGaugeGrant.call(msg) }
    }
}

/// Gets called when your SBA gauge value needs to update with a given value.
#[derive(Clone)]
pub struct OnHandleSBAUpdateHook {
    tx: event::Tx,
}

impl OnHandleSBAUpdateHook {
    pub fn new(tx: event::Tx) -> Self {
        OnHandleSBAUpdateHook { tx }
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        if let Ok(on_sba_update_original) = process.search_address(ON_HANDLE_SBA_UPDATE_SIG) {
            #[cfg(feature = "console")]
            println!("found on sba update");

            let cloned_self = self.clone();

            unsafe {
                let func: OnSBAUpdateFunc = std::mem::transmute(on_sba_update_original);
                OnSBAUpdate.initialize(
                    func,
                    move |a1, a2, a3, a4, a5, a6, a7, a8, a9, a10, a11| {
                        cloned_self.run(a1, a2, a3, a4, a5, a6, a7, a8, a9, a10, a11)
                    },
                )?;
                OnSBAUpdate.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find on_sba_update"));
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn run(
        &self,
        a1: *const usize,
        a2: f32,
        a3: u32,
        a4: u8,
        a5: u8,
        a6: f32,
        a7: u8,
        a8: u8,
        a9: u8,
        a10: u8,
        a11: u8,
    ) -> usize {
        // Online-recovery probes: whose gauge is this local update for (SBAUPD),
        // and what do all four slot gauges read right now (SBAPOLL)?
        #[cfg(feature = "hookdiag")]
        {
            use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
            static UPD: AtomicU32 = AtomicU32::new(0);
            let n = UPD.fetch_add(1, AtomicOrdering::Relaxed);
            if n < 12 || n % 256 == 0 {
                // v2.0.2: [a1+0x10] is the actor's specified-instance pointer
                // (decompiler-verified vtable object).
                let entity_ptr = unsafe { a1.byte_add(0x10).read() } as *const usize;
                let source_type_id = actor_type_id(entity_ptr);
                let (party, online, name) =
                    match super::player::actor_embedded_identity(entity_ptr as usize) {
                        Some((party, online, name)) => (party as i32, online as i32, name),
                        None => (-1, -1, "<unresolved>".to_string()),
                    };
                log::info!(
                    "SBAUPD comp={:#x} specified={:#x} type={source_type_id:#010x} party={party} online={online} name={name}",
                    a1 as usize,
                    entity_ptr as usize,
                );
            }
            log_sba_slot_poll();
        }

        // The gauge before the game's own grant. `a1` is the SBA component;
        // +0x7C is the gauge float (+0x80 its max) — the same offsets
        // `poll_slots_and_emit`/`log_sba_slot_poll` read per slot.
        use crate::hooks::diag::read_f32_guarded;
        let before = read_f32_guarded(a1 as usize, 0x7C);

        let ret = unsafe { OnSBAUpdate.call(a1, a2, a3, a4, a5, a6, a7, a8, a9, a10, a11) };

        // Attributed gain: this gauge update runs as a synchronous callee of
        // the register-hit gate (OnSBARegisterHitHook), so the damage instance
        // parked on this thread IS the hit that moved the gauge — no map, no
        // timing window.
        let after = read_f32_guarded(a1 as usize, 0x7C);
        if let (Some(before), Some(after)) = (before, after) {
            let amount = after - before;
            // A burst resetting the bar reads as a large negative; only
            // a real increase is a gain.
            if amount > 0.0 && amount.is_finite() {
                self.attribute_and_emit_gain(a1, amount, a3, a9, a11);
            }
        }

        // Unchanged: the four-slot poll stays the source of every player's
        // gauge LEVEL, and the only source at all for remote members.
        poll_slots_and_emit(&self.tx);

        ret
    }

    /// Resolves the cause of one measured rise on component `a1`, emits
    /// `SbaGain` with it, and keeps per-cause counters under `hookdiag`; an
    /// `Err` from the resolver skips the emit (the slot poll remains the
    /// record of the gauge LEVEL either way).
    ///
    /// `a3` is carried for the diagnostic only: it is the gauge update's
    /// scaling-gate word, the same value the network gauge grant forwards out
    /// of its message (`msg+0x20`), and logging it is how that field earns a
    /// meaning. Nothing branches on it.
    #[cfg_attr(not(feature = "hookdiag"), allow(unused_variables))]
    fn attribute_and_emit_gain(&self, a1: *const usize, amount: f32, a3: u32, a9: u8, a11: u8) {
        let outcome = self.resolve_cause(a1, a9, a11);

        if let Ok((actor_index, cause)) = outcome {
            let action_id = match cause {
                protocol::SbaGainCause::Skill(protocol::ActionType::Normal(id)) => id,
                _ => 0,
            };
            let _ = self.tx.send(Message::SbaGain(protocol::SbaGainEvent {
                actor_index,
                action_id,
                amount,
                cause: Some(cause),
            }));
        }

        // The next game patch is diagnosed from these: how many rises there
        // were and what cause each resolved to (or why none could be).
        #[cfg(feature = "hookdiag")]
        {
            use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
            static RISES: AtomicU32 = AtomicU32::new(0);
            static SKILL: AtomicU32 = AtomicU32::new(0);
            static TAKEN: AtomicU32 = AtomicU32::new(0);
            static PARTY: AtomicU32 = AtomicU32::new(0);
            static DIRECTOR: AtomicU32 = AtomicU32::new(0);
            static NAMED: AtomicU32 = AtomicU32::new(0);
            static UNKNOWN: AtomicU32 = AtomicU32::new(0);
            static FAILED: AtomicU32 = AtomicU32::new(0);
            match outcome {
                Ok((_, protocol::SbaGainCause::Skill(_))) => &SKILL,
                Ok((_, protocol::SbaGainCause::DamageTaken)) => &TAKEN,
                Ok((_, protocol::SbaGainCause::PartyAward)) => &PARTY,
                Ok((_, protocol::SbaGainCause::DirectorAward)) => &DIRECTOR,
                Ok((_, protocol::SbaGainCause::Unknown)) => &UNKNOWN,
                Ok(_) => &NAMED,
                Err(_) => &FAILED,
            }
            .fetch_add(1, AtomicOrdering::Relaxed);
            let rises = RISES.fetch_add(1, AtomicOrdering::Relaxed) + 1;
            if rises <= 8 || rises % 32 == 0 {
                log::info!(
                    "SBAGAIN rises={rises} skill={} taken={} party={} director={} named={} \
                     unknown={} failed={} last={outcome:?} amount={amount:.2} a3={a3:#x} \
                     a9={a9} a11={a11}",
                    SKILL.load(AtomicOrdering::Relaxed),
                    TAKEN.load(AtomicOrdering::Relaxed),
                    PARTY.load(AtomicOrdering::Relaxed),
                    DIRECTOR.load(AtomicOrdering::Relaxed),
                    NAMED.load(AtomicOrdering::Relaxed),
                    UNKNOWN.load(AtomicOrdering::Relaxed),
                    FAILED.load(AtomicOrdering::Relaxed),
                );
            }
            // Every UNKNOWN is an unlocated grant site. Log its amount so the
            // next site can be recognised by its constant (the broadcast award
            // was found exactly this way — a flat 35.00 among variable hits).
            if matches!(outcome, Ok((_, protocol::SbaGainCause::Unknown))) {
                log::info!("SBAUNK amount={amount:.2} a3-a11 flags a9={a9} a11={a11}");
                // The return-address chain names the caller no static pass
                // could (0x3940da3 has no containing function in Ghidra).
                // First few only: the walk is comparatively expensive and the
                // dodge grant repeats with identical frames.
                static WALKS: AtomicU32 = AtomicU32::new(0);
                if crate::hooks::diag::first_n(&WALKS, 6) {
                    crate::hooks::diag::log_callers("sba_unknown_rise");
                }
            }
        }
        let _ = outcome;
    }

    /// Resolves the cause of one measured rise on component `a1`, or an `Err`
    /// naming why nothing could be filed (the `hookdiag` counters bucket on it).
    ///
    /// ORDER MATTERS. The flag check comes first because a four-slot broadcast
    /// can fire while an unrelated hit is parked on this thread — live capture
    /// 2026-08-04 caught exactly that, a flat 35.00 award landing inside a
    /// link-attack frame. Checking the parked hit first would caption the
    /// party's award with whatever the player happened to be swinging.
    fn resolve_cause(
        &self,
        a1: *const usize,
        a9: u8,
        a11: u8,
    ) -> Result<(u32, protocol::SbaGainCause), &'static str> {
        use crate::hooks::diag::{read_ptr_guarded, read_u32_opt_guarded, read_u64_guarded};
        use protocol::SbaGainCause;

        // Who does `a1` (the measured component) belong to — needed by every
        // arm, so it is resolved first. Same a1+0x10 specified-instance
        // resolution the SBAUPD diag block uses, including the vfunc probe:
        // `player_slot_key_for_source`'s fallback arm CALLS the resolved
        // pointer's +0x58 vtable slot, and a future layout shift could leave
        // garbage-but-readable bytes there (an arbitrary jump on the game
        // thread). Probing first makes a drifted layout fail closed.
        let owner_key = read_ptr_guarded(a1 as usize, 0x10)
            .filter(|p| *p != 0)
            .filter(|p| super::summon::vfunc_slot_readable(*p as *const usize, 0x58))
            .and_then(|p| super::player_slot_key_for_source(p as *const usize))
            .ok_or("owner_unresolved")?;

        // Flat awards, identified by the arguments the game itself uses to skip
        // the sigil/stat scaling block inside the gauge update. FIRST, because a
        // broadcast can fire while an unrelated hit is parked on this thread.
        if a11 != 0 {
            return Ok((owner_key, SbaGainCause::DirectorAward));
        }
        if a9 != 0 {
            return Ok((owner_key, SbaGainCause::PartyAward));
        }

        // A cause parked by one of the named grant sites (Phase B). Nothing
        // parks one yet, so this is inert until those hooks land.
        if let Some(cause) = CauseGuard::current() {
            return Ok((owner_key, cause));
        }

        // The hit currently being registered on this thread, parked by the
        // register-hit gate detour. None means the rise did not come through the
        // gate at all — an unlocated site.
        let Some(hit) = HitGuard::current() else {
            return Ok((owner_key, SbaGainCause::Unknown));
        };

        // Cheapest possible sanity check that `hit` really is a DamageInstance
        // before any field of it is trusted: the gate's own first instruction
        // tests the byte at +0x158, so that byte must at least be mapped. If a
        // future patch drifts ON_SBA_REGISTER_HIT_SIG onto a different function,
        // its rdx is some foreign struct and this fails at step one instead of
        // walking it. (Reads the u32 covering the byte — same guard, no
        // allocation.)
        read_u32_opt_guarded(hit as usize, 0x158).ok_or("hit_not_instance")?;

        // The SOURCE player of the parked hit — exactly the resolution the
        // damage detour uses, so the two hooks can never attribute one hit
        // differently. Same vfunc probe, and it matters MORE here: every hit a
        // player RECEIVES falls through to the fallback's vtable walk, so that
        // is this arm's normal path rather than its rare one.
        let source_key = crate::hooks::damage::damage_source_instance_ptr(hit)
            .filter(|p| super::summon::vfunc_slot_readable(*p as *const usize, 0x58))
            .and_then(|p| super::player_slot_key_for_source(p as *const usize))
            .ok_or("source_unresolved")?;

        // Players gain gauge from TAKING hits too, and those flow through this
        // same gate. That is a real cause, not an error — it just is not the
        // victim's own action, so it must never be captioned with the enemy's
        // move. (This arm used to `return Err("actor_mismatch")`.)
        if source_key != owner_key {
            return Ok((owner_key, SbaGainCause::DamageTaken));
        }

        let flags = read_u64_guarded(hit as usize, 0xE8).ok_or("unreadable_fields")?;
        let action_id = read_u32_opt_guarded(hit as usize, 0x16C).ok_or("unreadable_fields")?;
        let action = crate::hooks::damage::classify_action_type(flags, action_id);
        Ok((owner_key, SbaGainCause::Skill(action)))
    }
}

/// PRODUCTION detour of the per-hit gauge GRANT virtual (v2.0.3 entry
/// 0x9b41b0, dispatched as vtable slot +0x80 by the target-side hit notifier
/// 0x27d4590): parks [`protocol::SbaGainCause::DamageTaken`] for the duration
/// of the original call, deferring to any enclosing named cause.
///
/// History, both halves live-proven:
/// - The local player's OWN hits do NOT route here (the H3 test, c0d06e1:
///   zero fires while the local player's gauge rose on their hits — that path
///   is the register-hit gate 0x9adaa0, see `OnSBARegisterHitHook`).
/// - Hits players RECEIVE do (2026-08-04, log 1694): NPC-inflicted hits on
///   party slots fired this virtual with the attacker's `DamageInstance` and
///   granted the victim a flat award (8.435/hit that round) via the walk
///   chain 0x1fb5d60 -> 0x1fb7330 -> 0x27d4590 -> here -> 0xbb1c00. Those
///   rises previously resolved `Unknown`: the register gate parks nothing on
///   this path, so nothing was on the thread when the update measured them.
///
/// Forwards all three args unchanged and returns the original's return value.
/// Zero-damage fires and full-gauge fires produce no rise; parking on them is
/// harmless because a cause is only read when a rise is measured.
pub struct OnSBAGrantHook;

impl OnSBAGrantHook {
    pub fn new() -> Self {
        Self
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        if let Ok(on_sba_grant_original) = process.search_address(ON_SBA_GRANT_SIG) {
            #[cfg(feature = "console")]
            println!("found on sba grant");

            unsafe {
                let func: OnSBAGrantFunc = std::mem::transmute(on_sba_grant_original);
                OnSBAGrant.initialize(func, |a1, a2, a3| Self::run(a1, a2, a3))?;
                OnSBAGrant.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find sba_grant"));
        }

        Ok(())
    }

    fn run(actor: *const usize, damage_instance: *const usize, mode: u32) -> usize {
        #[cfg(feature = "hookdiag")]
        let before = crate::hooks::diag::read_f32_guarded(actor as usize + 0x23B0, 0x7C);

        // Defer to an enclosing named cause, the same precedence rule as the
        // percent API: this is the shared taken-side grant, so an outer frame
        // that already named the rise must win over the generic caption.
        let _guard = match CauseGuard::current() {
            None => Some(CauseGuard::park(protocol::SbaGainCause::DamageTaken)),
            Some(_) => None,
        };
        let ret = unsafe { OnSBAGrant.call(actor, damage_instance, mode) };

        #[cfg(feature = "hookdiag")]
        {
            use crate::hooks::diag::{read_f32_guarded, read_u32_guarded};
            use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

            let after = read_f32_guarded(actor as usize + 0x23B0, 0x7C);
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, AtomicOrdering::Relaxed) + 1;
            if n <= 32 || n % 16 == 0 {
                let action_id = read_u32_guarded(damage_instance as usize, 0x16C);
                let damage = read_u32_guarded(damage_instance as usize, 0xD4) as i32;
                log::info!(
                    "SBAGRANT n={n} tid={:?} mode={mode} action={action_id} dmg={damage} \
                     gauge {before:?}->{after:?}",
                    std::thread::current().id(),
                );
            }
        }

        ret
    }
}

/// Called when your first try to attempt your SBA, and sets you into "casting SBA" state.
#[derive(Clone)]
pub struct OnAttemptSBAHook {
    tx: event::Tx,
}

impl OnAttemptSBAHook {
    pub fn new(tx: event::Tx) -> Self {
        OnAttemptSBAHook { tx }
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        if let Ok(on_sba_attempt_original) = process.search_address(ON_ATTEMPT_SBA_SIG) {
            #[cfg(feature = "console")]
            println!("found on sba attempt");

            let cloned_self = self.clone();

            unsafe {
                let func: OnSBAAttemptFunc = std::mem::transmute(on_sba_attempt_original);
                OnSBAAttempt.initialize(func, move |a1, a2| cloned_self.run(a1, a2))?;
                OnSBAAttempt.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find on_sba_attempt"));
        }

        Ok(())
    }

    fn run(&self, a1: *const usize, a2: f32) -> usize {
        // hookdiag: sba_attempt still resolves on v2.0.2; timestamp + callers let us
        // correlate the in-game SBA button press to the SBA manager code that also drives
        // the (broken) sba_update/collision/continue handlers. The caller RVA is stable, so
        // rate-limit the (relatively expensive) stack walk to the first few presses — same
        // policy as process_damage — while still timestamping every attempt.
        crate::hooks::diag::ev!("sba_attempt", "a2={a2}");
        #[cfg(feature = "hookdiag")]
        {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if crate::hooks::diag::first_n(&N, 3) {
                crate::hooks::diag::log_callers("sba_attempt");
            }
        }

        let ret = unsafe { OnSBAAttempt.call(a1, a2) };

        let entity_ptr = unsafe { a1.byte_add(0x10).read() } as *const usize;

        let source_idx = actor_idx(entity_ptr);
        let source_type_id = actor_type_id(entity_ptr);
        let (_, source_parent_idx) =
            super::player_keyed_parent(source_type_id, source_idx, entity_ptr);

        #[cfg(feature = "console")]
        println!("on sba attempt: player_index={}", source_parent_idx);

        let payload = Message::OnAttemptSBA(protocol::OnAttemptSBAEvent {
            actor_index: source_parent_idx,
        });

        let _ = self.tx.send(payload);

        ret
    }
}

/// Gets called when you're in "casting SBA state" once per game update interval until your SBA lands on
/// the target (or you miss)
/// ONLY WORKS FOR LOCAL.
#[derive(Clone)]
pub struct OnCheckSBACollisionHook {
    tx: event::Tx,
}

impl OnCheckSBACollisionHook {
    pub fn new(tx: event::Tx) -> Self {
        OnCheckSBACollisionHook { tx }
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        if let Ok(on_check_sba_collision_original) =
            process.search_address(ON_CHECK_SBA_COLLISION_SIG)
        {
            #[cfg(feature = "console")]
            println!("found on check sba collision");

            let cloned_self = self.clone();

            unsafe {
                let func: OnCheckSBACollisionFunc =
                    std::mem::transmute(on_check_sba_collision_original);
                OnCheckSBACollision.initialize(func, move |a1, a2| cloned_self.run(a1, a2))?;
                OnCheckSBACollision.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find on_check_sba_collision"));
        }

        Ok(())
    }

    fn run(&self, a1: *const usize, a2: f32) -> usize {
        let ret = unsafe { OnCheckSBACollision.call(a1, a2) };

        if ret != 0 {
            let entity_ptr = unsafe { a1.byte_add(0x10).read() } as *const usize;

            let source_idx = actor_idx(entity_ptr);
            let source_type_id = actor_type_id(entity_ptr);
            let (_, source_parent_idx) =
                super::player_keyed_parent(source_type_id, source_idx, entity_ptr);

            #[cfg(feature = "console")]
            println!("on perform sba: player_index={}", source_parent_idx);

            let payload = Message::OnPerformSBA(protocol::OnPerformSBAEvent {
                actor_index: source_parent_idx,
            });

            let _ = self.tx.send(payload);
        }

        ret
    }
}

/// Gets called when you connect your SBA with an active SBA chain (2/3/4)
#[derive(Clone)]
pub struct OnContinueSBAChainHook {
    tx: event::Tx,
}

impl OnContinueSBAChainHook {
    pub fn new(tx: event::Tx) -> Self {
        OnContinueSBAChainHook { tx }
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        if let Ok(on_continue_sba_chain_original) =
            process.search_address(ON_CONTINUE_SBA_CHAIN_SIG)
        {
            #[cfg(feature = "console")]
            println!("found on continue sba chain");

            let cloned_self = self.clone();

            unsafe {
                let func: OnContinueSBAChainFunc =
                    std::mem::transmute(on_continue_sba_chain_original);
                OnContinueSBAChain.initialize(func, move |a1, a2| cloned_self.run(a1, a2))?;
                OnContinueSBAChain.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find on_continue_sba_chain"));
        }

        Ok(())
    }

    fn run(&self, player_entity: *const usize, a2: *const usize) -> usize {
        #[cfg(feature = "console")]
        println!(
            "on continue sba chain: player_entity={:p}, a2={:p}",
            player_entity, a2
        );

        let ret = unsafe { OnContinueSBAChain.call(player_entity, a2) };

        let source_idx = actor_idx(player_entity);
        let source_type_id = actor_type_id(player_entity);
        let (_, source_parent_idx) =
            super::player_keyed_parent(source_type_id, source_idx, player_entity);

        let payload = Message::OnContinueSBAChain(protocol::OnContinueSBAChainEvent {
            actor_index: source_parent_idx,
        });

        let _ = self.tx.send(payload);

        ret
    }
}

#[derive(Clone)]
pub struct OnRemoteSBAUpdateHook {
    tx: event::Tx,
}

impl OnRemoteSBAUpdateHook {
    pub fn new(tx: event::Tx) -> Self {
        OnRemoteSBAUpdateHook { tx }
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        if let Ok(on_remote_sba_update_original) =
            process.search_address(ON_HANDLE_REMOTE_SBA_UPDATE_SIG)
        {
            #[cfg(feature = "console")]
            println!("found on remote sba update");

            let cloned_self = self.clone();

            unsafe {
                let func: OnRemoteSBAUpdateFunc =
                    std::mem::transmute(on_remote_sba_update_original);
                OnRemoteSBAUpdate
                    .initialize(func, move |a1, a2, a3, a4| cloned_self.run(a1, a2, a3, a4))?;
                OnRemoteSBAUpdate.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find on_remote_sba_update"));
        }

        Ok(())
    }

    fn run(&self, player_entity: *const usize, a2: *const usize, a3: f32, a4: f32) -> usize {
        let sba_offset = SBA_OFFSET.load(Ordering::Relaxed);

        // If the sba_offset signature failed to resolve (setup_globals now logs-and-continues
        // rather than aborting, leaving SBA_OFFSET at 0), reading the gauge at
        // player_entity+0+0x7C yields a garbage f32 and would emit bogus OnUpdateSBA/
        // OnPerformSBA events (e.g. a spurious "performed SBA" when the read happens to be 0.0).
        // Still call the original so game behaviour is unaffected, but skip our observation.
        if sba_offset == 0 {
            return unsafe { OnRemoteSBAUpdate.call(player_entity, a2, a3, a4) };
        }

        let sba_value_ptr =
            unsafe { player_entity.byte_add(sba_offset as usize).byte_add(0x7C) } as *const f32;
        let old_sba_value = unsafe { sba_value_ptr.read() };

        let ret = unsafe { OnRemoteSBAUpdate.call(player_entity, a2, a3, a4) };

        let source_idx = actor_idx(player_entity);
        let source_type_id = actor_type_id(player_entity);
        let (_, source_parent_idx) =
            super::player_keyed_parent(source_type_id, source_idx, player_entity);

        let new_sba_value = unsafe { sba_value_ptr.read() };
        let sba_added = f32::max(new_sba_value - old_sba_value, 0.0);

        // If the SBA value is 0, then the player has performed an SBA and this is resetting their SBA.
        if new_sba_value == 0.0 {
            #[cfg(feature = "console")]
            println!("on perform sba: player_index={}", source_parent_idx);

            let payload = Message::OnPerformSBA(protocol::OnPerformSBAEvent {
                actor_index: source_parent_idx,
            });

            let _ = self.tx.send(payload);
        } else {
            let payload = Message::OnUpdateSBA(protocol::OnUpdateSBAEvent {
                actor_index: source_parent_idx,
                sba_value: new_sba_value,
                sba_added,
            });

            let _ = self.tx.send(payload);
        }

        ret
    }
}

#[cfg(test)]
mod hit_guard_tests {
    use super::HitGuard;

    /// The nesting invariant the whole attribution rests on: a nested
    /// register-hit call must RESTORE its caller's hit on drop, not clear the
    /// slot. A clear-on-drop would leave every gauge rise after an inner hit
    /// unattributed (or, worse, attributed to nothing while an outer hit is
    /// still being processed). Pure Rust — no game process involved; the
    /// pointers are opaque markers, never dereferenced.
    #[test]
    fn nested_guards_restore_the_enclosing_hit() {
        let a = 0x1000usize as *const usize;
        let b = 0x2000usize as *const usize;

        assert_eq!(HitGuard::current(), None);
        let outer = HitGuard::park(a);
        assert_eq!(HitGuard::current(), Some(a));
        {
            let _inner = HitGuard::park(b);
            assert_eq!(HitGuard::current(), Some(b));
        }
        assert_eq!(HitGuard::current(), Some(a));
        drop(outer);
        assert_eq!(HitGuard::current(), None);
    }
}
