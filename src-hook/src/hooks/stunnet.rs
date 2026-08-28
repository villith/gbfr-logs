//! PRODUCTION hook on the game's NETWORK stun-apply handler + the STUNNET
//! diagnostic probe (the probe compiles only with `--features hookdiag`).
//!
//! Why (Ghidra `gbfr202fast`, 2026-07-18, live-confirmed same day): in an online
//! lobby the target's stun accumulator (`actor+0xB90`) is HOST-AUTHORITATIVE —
//! it never moves synchronously inside `ProcessDamageEvent`, for ANY player,
//! local included, so the damage hook's delta-across-the-call method reads 0
//! online. The accrual instead arrives asynchronously via a network
//! message handler, `FUN_140b43b40` (v2.0.2 entry rva 0xb43b40; v2.0.4
//! 0xb3e260). It has NO code or vtable xrefs — it is a switch-case target, and
//! the fast-analysis DB runs with Decompiler Switch Analysis disabled, so its
//! only reference is its own `.pdata` RUNTIME_FUNCTION entry. (This comment
//! used to read that entry as "its slot in the handler table at 0x7cc2c20";
//! there is no such table — that is just where `.pdata` describes it.) To find
//! its SIBLINGS, enumerate callers of the net entity-handle resolver
//! `FUN_140269520`: every handler needing a source or target entity calls it.
//! That is how the network SBA gauge grant was found — see the network-grant
//! block in `sba.rs`.
//!
//!   fn(rcx = target actor, rdx = message) {
//!       resolve_entity(*(msg+0x20));                 // u64 source entity-handle id
//!       amount = (float)*(i32*)(msg+0x18) * 0.001;   // fixed-point per-hit stun
//!       if stun_enabled(actor+0xAA0) {
//!           amount *= 1.0 + ramp(actor+0xBAC);       // bonus ramp past threshold
//!           actor+0xB90 = min(actor+0xB90 + amount, actor+0xB94 /* stun max */);
//!       }
//!   }
//!
//! This hook measures the accumulator across the original call (so the ramp
//! bonus and the stun-cap clamp are included), resolves the SOURCE through the
//! game's network-id → entity-handle hashmap (`FUN_140270250`, replicated below
//! with guarded reads), keys it by the player's embedded-record party slot, and
//! emits `Message::OnPlayerStun`. The parser prefers these messages over the
//! (online-dead) delta path per player, so solo keeps working either way.

use anyhow::{anyhow, Result};
use protocol::Message;
use retour::static_detour;

use crate::{event, process::Process};

#[cfg(feature = "hookdiag")]
use std::sync::atomic::AtomicU32;

/// Direct-entry signature for the network stun-apply handler `FUN_140b43b40`.
/// Verified unique via `sigscan ... addr` → target_rva=0xb43b40 (2026-07-18).
/// Anchored on the preceding `ret; int3*4` padding so the cursor lands exactly
/// on the entry; the prologue reads BOTH rcx and rdx (2-arg, matches decompile).
const NETWORK_STUN_SIG: &str =
    "c3 cc cc cc cc ' 56 57 53 48 83 ec 50 c5 f8 29 74 24 40 48 89 d3 48 89 ce 48 8b 4a 20";

/// Message-relative offsets (from the FUN_140b43b40 decompile).
const MSG_AMOUNT_OFFSET: usize = 0x18; // i32, fixed-point ×0.001
const MSG_SOURCE_ID_OFFSET: usize = 0x20; // u64 entity-handle id of the source

/// Target-actor offsets (same fields the solo stun path uses).
const STUN_ACCUMULATOR_OFFSET: usize = 0xB90;
#[allow(dead_code)] // read by the hookdiag STUNNET log line only
const STUN_MAX_OFFSET: usize = 0xB94;

/// Network-id → entity-handle hashmap globals (v2.0.2 RVAs, from the
/// FUN_140270250 decompile): chained buckets at `*(base+BUCKETS)` with
/// `{end_node, head_node}` pairs per bucket (0x10 stride), FNV-1a-64 over the
/// id's 8 bytes masked by `*(base+MASK)`, nil sentinel node at `*(base+SENTINEL)`.
/// Node layout: next @ +0x08, key @ +0x10, then the 0x18-byte handle
/// `{u32 index+1, pad, entity*, u64 id}` at +0x18/+0x20/+0x28.
///
/// The game guards this map with a critical section; we read lock-free but
/// every read is SEH-guarded and the found node is validated by key, so a
/// racing mutation can only cost us one attribution, never a fault.
///
/// v2.0.4 (re-derived 2026-08-07): the resolver stayed at rva 0x269520 and is
/// byte-identical to its 2.0.3 self apart from twelve displacement bytes. That
/// diff pins the displacement positions exactly — mask at fn+0xDD (`and
/// rcx,[rip]`), buckets at fn+0xE4 (`mov r14,[rip]`), sentinel at fn+0xF7 (`mov
/// r11,[rip]`) — and the same arithmetic reproduces the known-good 2.0.3 values
/// from the 2.0.3 function, which is what makes these trustworthy. Node layout
/// unchanged. Cross-checked a second way: the xref-site relocation of each
/// global (several independent call sites per global) lands on the same three
/// values.
///
/// These going stale is exactly what breaks ONLINE stun/SPS while solo keeps
/// working: the mask reads out of the wrong global, `resolve_source_entity`
/// bails, `source_slot_key` is None and no `OnPlayerStun` is ever emitted. The
/// solo path measures the accumulator delta in the damage hook and is unaffected,
/// so the symptom is "stun works solo, is dead online".
///
/// Derive these by DIFFING the function, not by counting bytes in a hex dump: a
/// first pass mis-sited all three displacements by 8, which silently walked the
/// wrong bucket array and made every online stun unattributable. Reading a
/// plausible-looking pointer out of the wrong global fails silently, and a
/// "does it land in BSS" sanity check cannot catch it — the whole region is BSS.
const NET_ENTITY_MAP_BUCKETS_RVA: usize = 0x7bc5298; // 2.0.4: 0x7bc5018
const NET_ENTITY_MAP_SENTINEL_RVA: usize = 0x7bc5288; // 2.0.4: 0x7bc5008
const NET_ENTITY_MAP_MASK_RVA: usize = 0x7bc52b0; // 2.0.4: 0x7bc5030

type NetworkStunFunc = unsafe extern "system" fn(*const usize, *const usize) -> usize;

static_detour! {
    static NetworkStun: unsafe extern "system" fn(*const usize, *const usize) -> usize;
}

#[cfg(any(feature = "eject", test))]
pub(super) fn disable() {
    super::disable_quiet("NetworkStun", &NetworkStun);
}

/// FNV-1a-64 over the 8 little-endian bytes of `id` — exactly the hash
/// FUN_140270250 computes for the bucket index.
fn fnv1a64_of_id(id: u64) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte_index in 0..8 {
        let byte = (id >> (byte_index * 8)) & 0xFF;
        hash = (hash ^ byte).wrapping_mul(0x100000001b3);
    }
    hash
}

/// Resolve a network entity-handle id to the entity pointer, replicating
/// FUN_140270250 with guarded reads. `None` on any miss/unreadable step.
fn resolve_source_entity(base: usize, id: u64) -> Option<usize> {
    use crate::hooks::diag::read_ptr_guarded;

    if id == u64::MAX || id == 0 {
        return None;
    }

    let mask = read_ptr_guarded(base, NET_ENTITY_MAP_MASK_RVA)? as u64;
    // Bucket masks are (power-of-two - 1) and small; a huge value means the map
    // isn't initialized or the RVA shifted on a patch — bail rather than walk.
    if mask == 0 || mask > 0x00FF_FFFF {
        return None;
    }
    let buckets = read_ptr_guarded(base, NET_ENTITY_MAP_BUCKETS_RVA).filter(|p| *p != 0)?;
    let sentinel = read_ptr_guarded(base, NET_ENTITY_MAP_SENTINEL_RVA)?;

    let bucket = (fnv1a64_of_id(id) & mask) as usize;
    let bucket_end = read_ptr_guarded(buckets, bucket * 0x10)?;
    let mut node = read_ptr_guarded(buckets, bucket * 0x10 + 8)?;
    if node == sentinel {
        return None;
    }

    // Bounded chain walk (chains are short; 64 is far beyond any real bucket).
    for _ in 0..64 {
        let key = read_ptr_guarded(node, 0x10)? as u64;
        if key == id {
            if node == sentinel {
                return None;
            }
            return read_ptr_guarded(node, 0x20).filter(|p| *p != 0);
        }
        if node == bucket_end {
            return None;
        }
        node = read_ptr_guarded(node, 0x08)?;
    }
    None
}

/// The STUNNET probe's log budget (hookdiag only), reset per quest so town play
/// can't starve an online capture.
#[cfg(feature = "hookdiag")]
const MAX_LOGS: u32 = 600;
#[cfg(feature = "hookdiag")]
static LOGS: AtomicU32 = AtomicU32::new(0);

/// Fresh probe budget per quest (called from the quest-load hook).
#[cfg(feature = "hookdiag")]
pub(crate) fn reset_budget() {
    LOGS.store(0, std::sync::atomic::Ordering::Relaxed);
}

#[derive(Clone)]
pub struct OnNetworkStunHook {
    tx: event::Tx,
}

impl OnNetworkStunHook {
    pub fn new(tx: event::Tx) -> Self {
        Self { tx }
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        let addr = process
            .search_address(NETWORK_STUN_SIG)
            .map_err(|e| anyhow!("stun_net: handler sig failed: {e:?}"))?;

        let cloned_self = self.clone();
        unsafe {
            let func: NetworkStunFunc = std::mem::transmute(addr);
            NetworkStun.initialize(func, move |a1, a2| cloned_self.run(a1, a2))?;
            NetworkStun.enable()?;
        }

        Ok(())
    }

    fn run(&self, a1: *const usize, a2: *const usize) -> usize {
        use crate::hooks::diag::{read_f32_guarded, read_ptr_guarded, read_u32_guarded};
        use std::sync::atomic::Ordering as AtomicOrdering;

        let target = a1 as usize;
        let msg = a2 as usize;

        // All reads guarded — this path can never fault the game thread. A zero
        // amount carries no signal (every hit sends a stun message, most with 0).
        let amount_raw = read_u32_guarded(msg, MSG_AMOUNT_OFFSET) as i32;
        if amount_raw <= 0 {
            return unsafe { NetworkStun.call(a1, a2) };
        }

        let source_id = ((read_u32_guarded(msg, MSG_SOURCE_ID_OFFSET + 4) as u64) << 32)
            | read_u32_guarded(msg, MSG_SOURCE_ID_OFFSET) as u64;
        let pre = read_f32_guarded(target, STUN_ACCUMULATOR_OFFSET);

        let ret = unsafe { NetworkStun.call(a1, a2) };

        let post = read_f32_guarded(target, STUN_ACCUMULATOR_OFFSET);

        // Prefer the measured accumulator delta: it includes the ramp bonus and
        // the stun-cap clamp, i.e. what the boss actually received. Fall back to
        // the raw fixed-point amount when the accumulator isn't readable.
        let applied = match (pre, post) {
            (Some(before), Some(after)) if after > before => after - before,
            (Some(_), Some(_)) => 0.0, // gated off (stun disabled) or clamped at max
            _ => amount_raw as f32 * 0.001,
        };

        // Attribute to a player: message source id → entity handle → specified
        // instance → embedded-record party slot (pets resolve to their owner).
        let source_slot_key = {
            let base = crate::hooks::diag::MODULE_BASE.load(AtomicOrdering::Relaxed);
            (base != 0)
                .then(|| resolve_source_entity(base, source_id))
                .flatten()
                .and_then(|entity| read_ptr_guarded(entity, 0x70))
                .filter(|specified| *specified != 0)
                .and_then(|specified| super::player_slot_key_for_source(specified as *const usize))
        };

        #[cfg(feature = "hookdiag")]
        if crate::hooks::diag::first_n(&LOGS, MAX_LOGS) {
            let max = read_f32_guarded(target, STUN_MAX_OFFSET);
            let target_idx = read_u32_guarded(target, 0x170);
            log::info!(
                "STUNNET t={} target={target:#x} idx170={target_idx:#x} amount_raw={amount_raw} \
                 (={:.3}) applied={applied:.3} src_id={source_id:#x} slot_key={source_slot_key:?} \
                 b90 {pre:?} -> {post:?} max={max:?}",
                crate::hooks::diag::ms(),
                amount_raw as f32 * 0.001,
            );
        }

        if applied > 0.0 {
            if let Some(actor_index) = source_slot_key {
                let _ = self
                    .tx
                    .send(Message::OnPlayerStun(protocol::OnPlayerStunEvent {
                        actor_index,
                        stun_amount: applied,
                    }));
            }
        }

        ret
    }
}
