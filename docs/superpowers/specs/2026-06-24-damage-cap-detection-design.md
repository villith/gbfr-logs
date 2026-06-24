# Damage Cap Detection (Phase 1) — Design

**Date:** 2026-06-24
**Status:** Approved (design) — pending spec review
**Scope:** Parser/DB + frontend. No hook changes, no CSV parsing, no DB migration.

## Problem

Granblue Fantasy: Relink clamps each attack/skill hit to a per-skill **damage cap**.
Players want to know when their attacks are hitting that cap (wasted potential
damage). Today the meter shows damage per skill but gives no indication that a hit
was capped.

## Key insight

The game already reports the per-hit cap. The injected hook reads
`DamageInstance.damage_cap` (offset `0x264`, `src-hook/src/hooks/ffi.rs:15`) and
puts it on every regular damage event as `DamageEvent.damage_cap: Option<i32>`
(`src-hook/src/hooks/damage.rs:146`, `protocol/src/lib.rs:81`). This value flows
into the parser's `raw_event_log` and is persisted in every saved encounter — but
nothing downstream reads it.

Therefore Phase 1 needs **no hook work and no CSVs**: we detect a capped hit purely
from data we already have.

### What we can and cannot do

- **Detect capped hits (this phase):** the game gives us the final (post-cap)
  `damage` and the `damage_cap`. A hit is capped when the final damage reached the
  cap. Fully accurate, all characters, retroactive on old logs.
- **"Damage lost" (NOT this phase):** the game does NOT report pre-cap raw damage,
  so the amount lost cannot be derived from the hook. Estimating it requires the
  CSV skill ratios plus the player's attack stat and buff state — deferred to a
  later phase. Explicitly out of scope here.

## Cap detection rule

A single damage hit is **capped** when:

```
damage_cap == Some(cap) && cap > 0 && damage >= cap
```

`damage_cap == None` (DoT events, legacy/hand-built events) means "cap unknown" and
is **never** counted as capped. `>=` is used (not `==`) to be robust to any
off-by-one between the reported final damage and cap.

This rule lives in exactly one place: `AdjustedDamageInstance` (see below).

## Back-compat

`SkillState` and `PlayerState` are **derived** state. The on-disk blob stores only
the raw `Encounter` (`player_data`, quest fields, `raw_event_log`) —
`src-tauri/src/parser/v1/mod.rs:225`. Derived state is recomputed from
`raw_event_log` on load. Consequences:

- No DB migration and no `#[serde(default)]` needed for disk back-compat.
- Existing saved logs gain `cappedHits` automatically on reparse, because their
  stored `raw_event_log` already contains `damage_cap`.
- The only serialization of `SkillState`/`PlayerState` is the live Tauri→frontend
  event channel, where backend and frontend are always the same build, so the three
  additive fields are safe.

## Backend changes (`src-tauri`, `crate gbfr-logs`)

All additive. No `protocol` change (the field already exists there).

1. **`AdjustedDamageInstance`** (`parser/v1/mod.rs:23`) — add `pub is_capped: bool`,
   computed in `from_damage_event`:
   ```rust
   let is_capped = matches!(event.damage_cap, Some(cap) if cap > 0 && event.damage >= cap);
   ```

2. **`SkillState`** (`parser/v1/skill_state.rs:11`) — add `pub capped_hits: u32`
   (default 0). In `update_from_damage_event`:
   `if damage_instance.is_capped { self.capped_hits += 1; }`.

3. **`PlayerState`** (`parser/v1/player_state.rs:8`) — add `pub capped_hits: u32`,
   incremented the same way in its `update_from_damage_event`, giving a player-level
   total without re-summing skills on the frontend.

Both structs are `#[serde(rename_all = "camelCase")]`, so the wire field is
`cappedHits`.

## Frontend changes (`src/`)

1. **`types.ts`** — add `cappedHits: number` to `SkillState`, `PlayerState`, and
   `ComputedSkillGroup` (so grouped rows carry it).

2. **`components/useSkillBreakdown.ts`** — when condensing skills into a group, sum
   `cappedHits` in both the merge branch (`skills[i] = {...}`) and the new-group
   branch (`skills.push({...})`), exactly as `hits` is summed today.

3. **Per-skill display** — add a **"Cap" column** to the skill breakdown table
   showing the ratio `cappedHits/hits` (e.g. `12/40`; `0/88` when none).
   - Header `<th>` in `components/SkillBreakdown.tsx`.
   - `<td>` in `components/SkillRow.tsx` and `components/SkillGroupRow.tsx`.
   - When `cappedHits > 0`, the cell is visually flagged (warning color +
     Phosphor `Warning` icon, already a dependency).

4. **Per-player indicator** — in `components/PlayerRow.tsx`, show a small cap badge
   next to the player name when `player.cappedHits > 0`, so capped players are
   visible without expanding.

5. **i18n** — the "Cap" header goes through the existing `t(...)` pattern with a new
   key in `src-tauri/lang/en/ui.json` (the only hand-edited lang file; others
   fall back to `en`).

## Out of scope (Phase 1)

- "Damage lost" / second-color bar segment (#5) — CSV-dependent, deferred.
- CSV parsing, character-name mapping, skill-row mapping — not needed for detection.
- DoT cap detection — the hook does not populate `damage_cap` for DoT events.

## Testing

Pure parser logic — fully verifiable with `cargo test -p gbfr-logs`, no game needed.
Extend the existing `#[cfg(test)]` modules:

- **`skill_state.rs`**: capped when `damage == cap`; capped when `damage > cap`; not
  capped when `damage < cap`; not capped when `damage_cap: None`; `capped_hits`
  accumulates correctly across a mix of capped and uncapped hits.
- **`player_state.rs`**: player-level `capped_hits` accumulates across skills.
- **`mod.rs`**: an end-to-end `process_damage_event` / reparse test asserting a
  known capped event produces `capped_hits == 1` at skill and player level.

Frontend: `npm run build` (tsc) + `npm run lint`; the grouping sum is covered by
existing Vitest patterns if a unit test is added for `useSkillBreakdown`.

## Verification

`cargo test -p gbfr-logs`, `cargo clippy`, `npm run build`, `npm run lint`. Live
visual verification (marker rendering against a real encounter) requires the game
and is the user's manual step.
