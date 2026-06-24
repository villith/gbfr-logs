# Damage Cap Detection (Phase 1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Mark attacks/skills (and players) that hit their per-hit damage cap, using the cap value the game already reports.

**Architecture:** Detect capped hits in the parser from the existing `DamageEvent.damage_cap` field (no hook/CSV/DB changes). Add additive `capped_hits` counts to `SkillState` and `PlayerState`, serialize them (camelCase) to the frontend, and render a `capped/total` ratio column in the skill breakdown plus a per-player cap badge.

**Tech Stack:** Rust (nightly, `gbfr-logs` crate, `cargo test`), React + TypeScript (Vite, `npm run build`/`lint`), Phosphor icons.

**Spec:** `docs/superpowers/specs/2026-06-24-damage-cap-detection-design.md`

**Deviation from spec:** The skill-breakdown table headers in `SkillBreakdown.tsx` are hardcoded English ("Skill", "Hits", ...), not `t()` calls. For consistency the new "Cap" header is also hardcoded; the spec's i18n note does not apply to this table. No `ui.json` change.

---

## File Structure

**Backend (`src-tauri`, crate `gbfr-logs`):**
- `src/parser/v1/mod.rs` — `AdjustedDamageInstance` gains `is_capped: bool`.
- `src/parser/v1/skill_state.rs` — `SkillState` gains `capped_hits: u32`; tests.
- `src/parser/v1/player_state.rs` — `PlayerState` gains `capped_hits: u32`; tests; fixtures updated.

**Frontend (`src/`):**
- `types.ts` — `cappedHits` on `SkillState`, `PlayerState`, `ComputedSkillGroup`.
- `components/useSkillBreakdown.ts` — sum `cappedHits` into groups (2 sites).
- `components/SkillBreakdown.tsx` — new "Cap" header `<th>`.
- `components/SkillRow.tsx` — new "Cap" `<td>` (`capped/total`).
- `components/SkillGroupRow.tsx` — new "Cap" `<td>` (`capped/total`).
- `components/PlayerRow.tsx` — per-player cap badge.

---

## Task 1: `SkillState.capped_hits` (backend, TDD)

**Files:**
- Modify: `src-tauri/src/parser/v1/skill_state.rs`

- [ ] **Step 1: Add the failing test**

Append inside the existing `mod tests { ... }` block in `src-tauri/src/parser/v1/skill_state.rs` (after the existing `updating_from_damage_event` test). This uses the same `AdjustedDamageInstance::from_damage_event` path the real parser uses, so it exercises the detection rule end to end.

```rust
    fn make_event(damage: i32, damage_cap: Option<i32>) -> DamageEvent {
        DamageEvent {
            source: Actor { index: 0, actor_type: 0, parent_actor_type: 0, parent_index: 0 },
            target: Actor { index: 0, actor_type: 0, parent_actor_type: 0, parent_index: 0 },
            action_id: ActionType::Normal(1),
            damage,
            flags: 0,
            attack_rate: None,
            stun_value: None,
            damage_cap,
        }
    }

    #[test]
    fn counts_capped_hits() {
        use crate::parser::v1::AdjustedDamageInstance;

        let mut skill_state = SkillState::new(ActionType::Normal(1), CharacterType::Pl0000);

        // damage == cap -> capped
        let e1 = make_event(22_999, Some(22_999));
        skill_state.update_from_damage_event(&AdjustedDamageInstance::from_damage_event(&e1, None));
        // damage > cap -> capped
        let e2 = make_event(30_000, Some(22_999));
        skill_state.update_from_damage_event(&AdjustedDamageInstance::from_damage_event(&e2, None));
        // damage < cap -> not capped
        let e3 = make_event(10_000, Some(22_999));
        skill_state.update_from_damage_event(&AdjustedDamageInstance::from_damage_event(&e3, None));
        // no cap info -> not capped
        let e4 = make_event(99_999, None);
        skill_state.update_from_damage_event(&AdjustedDamageInstance::from_damage_event(&e4, None));
        // cap == 0 -> not capped (guard against bogus zero cap)
        let e5 = make_event(5_000, Some(0));
        skill_state.update_from_damage_event(&AdjustedDamageInstance::from_damage_event(&e5, None));

        assert_eq!(skill_state.hits, 5);
        assert_eq!(skill_state.capped_hits, 2);
    }
```

- [ ] **Step 2: Run the test to verify it fails to compile**

Run: `cargo test -p gbfr-logs counts_capped_hits 2>&1 | tail -20`
Expected: compile error — `no field 'capped_hits' on type 'SkillState'` and `no field 'is_capped' on AdjustedDamageInstance`. (This proves the test exercises the new API.)

- [ ] **Step 3: Add `is_capped` to `AdjustedDamageInstance`**

In `src-tauri/src/parser/v1/mod.rs`, change the struct (currently lines ~23-39):

```rust
pub struct AdjustedDamageInstance<'a> {
    pub event: &'a DamageEvent,
    pub player_data: Option<&'a PlayerData>,
    pub stun_damage: f64,
    pub is_capped: bool,
}

impl<'a> AdjustedDamageInstance<'a> {
    pub fn from_damage_event(event: &'a DamageEvent, player_data: Option<&'a PlayerData>) -> Self {
        let stun_damage = event.stun_value.unwrap_or(0.0) as f64;
        let is_capped = matches!(event.damage_cap, Some(cap) if cap > 0 && event.damage >= cap);

        Self {
            event,
            player_data,
            stun_damage,
            is_capped,
        }
    }
}
```

- [ ] **Step 4: Add `capped_hits` to `SkillState`**

In `src-tauri/src/parser/v1/skill_state.rs`, add the field to the struct (after `total_stun_value`, ~line 27):

```rust
    /// Total stun value done by this skill
    pub total_stun_value: f64,
    /// Number of hits that reached the game's damage cap for this skill
    pub capped_hits: u32,
}
```

Initialize it in `SkillState::new` (in the `Self { ... }` literal, after `total_stun_value: 0.0,`):

```rust
            total_stun_value: 0.0,
            capped_hits: 0,
        }
```

Increment it in `update_from_damage_event` (add as the first line of the method body, before `self.hits += 1;`):

```rust
    pub fn update_from_damage_event(&mut self, damage_instance: &AdjustedDamageInstance) {
        if damage_instance.is_capped {
            self.capped_hits += 1;
        }
        self.hits += 1;
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p gbfr-logs counts_capped_hits 2>&1 | tail -20`
Expected: `test ... counts_capped_hits ... ok`.

- [ ] **Step 6: Run the full crate test suite (no regressions)**

Run: `cargo test -p gbfr-logs 2>&1 | tail -20`
Expected: all tests pass (the existing `updating_from_damage_event` still passes; `SkillState::new` change is covered).

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/parser/v1/mod.rs src-tauri/src/parser/v1/skill_state.rs
git commit -m "feat(parser): detect capped hits per skill"
```

---

## Task 2: `PlayerState.capped_hits` (backend, TDD)

**Files:**
- Modify: `src-tauri/src/parser/v1/player_state.rs`

Note: `update_from_damage_event` has early `return`s in the skill-update loop (lines ~102, ~108). Therefore `capped_hits` MUST be incremented at the **top** of the method, before the loop, or capped hits on already-tracked skills are missed.

- [ ] **Step 1: Add the failing test**

Append inside the existing `mod tests { ... }` block in `src-tauri/src/parser/v1/player_state.rs`:

```rust
    fn capped_event() -> DamageEvent {
        DamageEvent {
            source: protocol::Actor { index: 0, actor_type: 0, parent_actor_type: 0, parent_index: 0 },
            target: protocol::Actor { index: 0, actor_type: 0, parent_actor_type: 0, parent_index: 0 },
            action_id: ActionType::Normal(1),
            damage: 22_999,
            flags: 0,
            attack_rate: None,
            stun_value: None,
            damage_cap: Some(22_999),
        }
    }

    fn uncapped_event() -> DamageEvent {
        DamageEvent {
            source: protocol::Actor { index: 0, actor_type: 0, parent_actor_type: 0, parent_index: 0 },
            target: protocol::Actor { index: 0, actor_type: 0, parent_actor_type: 0, parent_index: 0 },
            action_id: ActionType::Normal(2),
            damage: 100,
            flags: 0,
            attack_rate: None,
            stun_value: None,
            damage_cap: Some(22_999),
        }
    }

    #[test]
    fn counts_player_capped_hits_across_skills() {
        let mut player_state = PlayerState {
            index: 0,
            character_type: CharacterType::Pl0000,
            total_damage: 0,
            last_known_pet_skill: None,
            dps: 0.0,
            skill_breakdown: vec![],
            sba: 0.0,
            total_stun_value: 0.0,
            stun_per_second: 0.0,
            capped_hits: 0,
        };

        // Two capped hits on the same skill (exercises the early-return path),
        // one uncapped hit on a different skill.
        let capped = capped_event();
        let uncapped = uncapped_event();
        player_state.update_from_damage_event(&AdjustedDamageInstance::from_damage_event(&capped, None));
        player_state.update_from_damage_event(&AdjustedDamageInstance::from_damage_event(&capped, None));
        player_state.update_from_damage_event(&AdjustedDamageInstance::from_damage_event(&uncapped, None));

        assert_eq!(player_state.capped_hits, 2);
        // Skill-level counts are still correct through the early-return path.
        let normal_1 = player_state
            .skill_breakdown
            .iter()
            .find(|s| s.action_type == ActionType::Normal(1))
            .unwrap();
        assert_eq!(normal_1.capped_hits, 2);
    }
```

- [ ] **Step 2: Run the test to verify it fails to compile**

Run: `cargo test -p gbfr-logs counts_player_capped_hits 2>&1 | tail -20`
Expected: compile error — `missing field 'capped_hits' in initializer of 'PlayerState'` / `no field 'capped_hits'`.

- [ ] **Step 3: Add the `capped_hits` field to `PlayerState`**

In `src-tauri/src/parser/v1/player_state.rs`, add to the struct (after `stun_per_second: f64,`, ~line 20):

```rust
    pub stun_per_second: f64,
    /// Number of hits by this player that reached the game's damage cap
    pub capped_hits: u32,
}
```

- [ ] **Step 4: Increment at the top of `update_from_damage_event`**

In `src-tauri/src/parser/v1/player_state.rs`, add as the first lines of `update_from_damage_event` (before `self.total_damage += ...`, ~line 73):

```rust
    pub fn update_from_damage_event(&mut self, damage_instance: &AdjustedDamageInstance) {
        if damage_instance.is_capped {
            self.capped_hits += 1;
        }
        self.total_damage += damage_instance.event.damage as u64;
```

- [ ] **Step 5: Fix existing test fixtures that build `PlayerState` literally**

Each existing test in this file constructs `PlayerState { ... }` with all fields listed, so each needs `capped_hits: 0,` added. There are fixtures in: `calculates_dps`, `updates_from_damage_event`, `same_skill_updates_from_multiple_damage_events`, `new_skills_are_tracked_separately`, and any others below them. Add `capped_hits: 0,` to every `PlayerState { ... }` literal in the `mod tests` block (place it after the `stun_per_second` / `total_stun_value` line in each).

Then find any other `PlayerState { ... }` literal in the codebase:

Run: `grep -rn "PlayerState {" src-tauri/src/ | grep -v target`
For each match outside `player_state.rs` (e.g. in `mod.rs` if the parser constructs players there), confirm whether it is a struct literal that needs `capped_hits: 0,`. If `PlayerState` is constructed via a constructor/`Default`, no change is needed there.

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo test -p gbfr-logs counts_player_capped_hits 2>&1 | tail -20`
Expected: `test ... counts_player_capped_hits_across_skills ... ok`.

- [ ] **Step 7: Run the full crate test suite**

Run: `cargo test -p gbfr-logs 2>&1 | tail -20`
Expected: all tests pass.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/parser/v1/player_state.rs
git commit -m "feat(parser): detect capped hits per player"
```

---

## Task 3: End-to-end parser test + clippy (backend)

**Files:**
- Modify: `src-tauri/src/parser/v1/mod.rs` (test only)

- [ ] **Step 1: Locate an existing parser-level test that calls `process_damage_event` or `reparse`**

Run: `grep -n "fn .*test\|#\[test\]\|process_damage_event\|reparse\|mod tests" src-tauri/src/parser/v1/mod.rs | head -40`
Read the surrounding test (and how `Parser` / `DerivedEncounterState` is built in tests) so the new test matches the file's established setup pattern. If `mod.rs` has no test module, SKIP this task — Tasks 1 and 2 already cover detection through the real `AdjustedDamageInstance` path; note the skip in the commit log of Task 4 instead.

- [ ] **Step 2: Add an end-to-end capped-hit test (only if a test harness exists in mod.rs)**

Following the existing pattern found in Step 1, add a test that feeds one capped `DamageEvent` (`damage: 99_999, damage_cap: Some(99_999)`) through the parser's damage entry point and asserts the resulting `DerivedEncounterState`'s player has `capped_hits == 1` and that player's matching skill has `capped_hits == 1`. Use the exact constructor calls the surrounding tests use (do not invent new constructors).

- [ ] **Step 3: Run the new test**

Run: `cargo test -p gbfr-logs 2>&1 | tail -20`
Expected: all tests pass.

- [ ] **Step 4: Clippy clean**

Run: `cargo clippy -p gbfr-logs 2>&1 | tail -30`
Expected: no new warnings introduced by these changes.

- [ ] **Step 5: Commit (if a test was added)**

```bash
git add src-tauri/src/parser/v1/mod.rs
git commit -m "test(parser): end-to-end capped hit aggregation"
```

---

## Task 4: Frontend types (`types.ts`)

**Files:**
- Modify: `src/types.ts`

- [ ] **Step 1: Add `cappedHits` to `SkillState`**

In `src/types.ts`, in `export type SkillState`, after `maxStunValue: number;` (~line 54):

```ts
  /** Maximum recorded stun value of the skill */
  maxStunValue: number;
  /** Number of hits that reached the game's damage cap */
  cappedHits: number;
};
```

- [ ] **Step 2: Add `cappedHits` to `ComputedSkillGroup`**

In `export type ComputedSkillGroup`, after its `maxStunValue: number;` (~line 82):

```ts
  /** Maximum recorded stun value of the skill */
  maxStunValue: number;
  /** Number of hits that reached the game's damage cap (summed over grouped skills) */
  cappedHits: number;
};
```

- [ ] **Step 3: Add `cappedHits` to `PlayerState`**

In `export type PlayerState`, after `skillBreakdown: SkillState[];` (~line 103):

```ts
  /** Stats for individual skills logged */
  skillBreakdown: SkillState[];
  /** Number of hits by this player that reached the game's damage cap */
  cappedHits: number;
};
```

- [ ] **Step 4: Typecheck**

Run: `npm run tsc 2>&1 | tail -30`
Expected: this surfaces errors in `useSkillBreakdown.ts` where `ComputedSkillGroup` objects are built without `cappedHits` (fixed in Task 5). It is acceptable for this step to FAIL with exactly those "missing property cappedHits" errors — that confirms the group-builder needs updating. If other unrelated errors appear, stop and investigate.

- [ ] **Step 5: Commit**

```bash
git add src/types.ts
git commit -m "feat(types): add cappedHits to skill/player/group types"
```

---

## Task 5: Group aggregation (`useSkillBreakdown.ts`)

**Files:**
- Modify: `src/components/useSkillBreakdown.ts`

The condense logic builds `ComputedSkillGroup` objects in two places: the merge branch (`skills[skillGroupIndex] = { ...skillGroup, hits: ... }`) and the new-group branch (`skills.push({ actionType: groupActionType, ... })`). Both must carry `cappedHits`.

- [ ] **Step 1: Sum `cappedHits` in the merge branch**

In `src/components/useSkillBreakdown.ts`, in the `skills[skillGroupIndex] = { ...skillGroup, ... }` object, add (alongside `hits: skillGroup.hits + skill.hits,`):

```ts
                hits: skillGroup.hits + skill.hits,
                cappedHits: skillGroup.cappedHits + skill.cappedHits,
```

- [ ] **Step 2: Initialize `cappedHits` in the new-group branch**

In the `skills.push({ ... })` object, add (alongside `hits: skill.hits,`):

```ts
                hits: skill.hits,
                cappedHits: skill.cappedHits,
```

- [ ] **Step 3: Check for any other grouping branch**

Run: `grep -n "hits:" src/components/useSkillBreakdown.ts`
For every object literal that sets `hits:` and represents a group/skill row, ensure a sibling `cappedHits:` is set. (There may be additional branches for pet/child skills further down the file — apply the same rule: where `hits` is summed, sum `cappedHits`; where `hits` is copied, copy `cappedHits`.)

- [ ] **Step 4: Typecheck**

Run: `npm run tsc 2>&1 | tail -30`
Expected: PASS (no missing-property errors). `ComputedSkillState` already inherits `cappedHits` from `SkillState`, and groups now set it.

- [ ] **Step 5: Commit**

```bash
git add src/components/useSkillBreakdown.ts
git commit -m "feat(meter): aggregate cappedHits into skill groups"
```

---

## Task 6: Skill-breakdown "Cap" column (`SkillBreakdown.tsx`, `SkillRow.tsx`, `SkillGroupRow.tsx`)

**Files:**
- Modify: `src/components/SkillBreakdown.tsx`
- Modify: `src/components/SkillRow.tsx`
- Modify: `src/components/SkillGroupRow.tsx`

- [ ] **Step 1: Add the "Cap" header**

In `src/components/SkillBreakdown.tsx`, in the `<thead>` row, after the `%` header (`<th className="header-column text-center">%</th>`):

```tsx
              <th className="header-column text-center">%</th>
              <th className="header-column text-center">Cap</th>
```

- [ ] **Step 2: Add the "Cap" cell to `SkillRow`**

In `src/components/SkillRow.tsx`, after the `%` cell (the `<td>` containing `skill.percentage.toFixed(0)`), before the `<div className="damage-bar" .../>`:

```tsx
      <td className="text-center row-data">
        {skill.cappedHits > 0 ? (
          <span className="capped">
            {skill.cappedHits}/{skill.hits}
          </span>
        ) : (
          `0/${skill.hits}`
        )}
      </td>
```

- [ ] **Step 3: Add the "Cap" cell to `SkillGroupRow`**

In `src/components/SkillGroupRow.tsx`, after the `%` cell (the `<td>` containing `group.percentage.toFixed(0)`), before the `<div className="damage-bar" .../>`:

```tsx
        <td className="text-center row-data">
          {group.cappedHits > 0 ? (
            <span className="capped">
              {group.cappedHits}/{group.hits}
            </span>
          ) : (
            `0/${group.hits}`
          )}
        </td>
```

- [ ] **Step 4: Add the `.capped` style**

In `src/App.css`, add a rule giving capped cells a warning color (match existing warning/accent usage in the file; if none, use a clear orange):

```css
.capped {
  color: #ffb454;
  font-weight: 600;
}
```

- [ ] **Step 5: Typecheck + lint + build**

Run: `npm run tsc 2>&1 | tail -20 && npm run lint 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/components/SkillBreakdown.tsx src/components/SkillRow.tsx src/components/SkillGroupRow.tsx src/App.css
git commit -m "feat(meter): show capped/total ratio in skill breakdown"
```

---

## Task 7: Per-player cap badge (`PlayerRow.tsx`)

**Files:**
- Modify: `src/components/PlayerRow.tsx`

- [ ] **Step 1: Import the Warning icon**

In `src/components/PlayerRow.tsx`, extend the existing Phosphor import (line 1):

```tsx
import { CaretDown, CaretUp, Warning } from "@phosphor-icons/react";
```

- [ ] **Step 2: Render the badge next to the player name**

In the player-name `<td>` (currently rendering `translatedPlayerName(...)`), append a conditional badge:

```tsx
        <td className="text-left row-data">
          {translatedPlayerName(partySlotIndex, partyData[partySlotIndex], player, showDisplayNames)}
          {player.cappedHits > 0 && (
            <span className="capped" title={`${player.cappedHits} capped hits`}>
              {" "}
              <Warning size={12} weight="fill" />
            </span>
          )}
        </td>
```

- [ ] **Step 3: Typecheck + lint + build**

Run: `npm run tsc 2>&1 | tail -20 && npm run lint 2>&1 | tail -20 && npm run build 2>&1 | tail -15`
Expected: PASS; build produces `dist/`.

- [ ] **Step 4: Commit**

```bash
git add src/components/PlayerRow.tsx
git commit -m "feat(meter): flag players with capped hits"
```

---

## Task 8: Full verification

- [ ] **Step 1: Backend tests + clippy**

Run: `cargo test -p gbfr-logs 2>&1 | tail -20 && cargo clippy -p gbfr-logs 2>&1 | tail -20`
Expected: all tests pass; no new clippy warnings.

- [ ] **Step 2: Frontend test + build**

Run: `npm run test 2>&1 | tail -20 && npm run build 2>&1 | tail -15`
Expected: Vitest passes; tsc + Vite build succeed.

- [ ] **Step 3: Note manual verification**

Live visual verification (the ratio column and player badge rendering against a real encounter, and confirming capped hits look right in-game) requires Windows + admin + the running game and is the user's manual step. State this in the handoff; do not claim live UI verification was done.
