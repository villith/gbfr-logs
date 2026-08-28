---
name: reverse-engineering-signatures
description: Use when a Granblue Fantasy Relink game patch breaks the GBFR Logs hook — signatures no longer match, "Could not find match for pattern" / "Could not find <offset>" warnings, hooks logging FAIL at startup, an offset reads a wrong/garbage value, or a hook crashes the game. Also use when tracing where a value in the game binary comes from — an unexplained id or constant surfaced by the hook, "which ability/system set this field", or before concluding a value is data-driven, runtime-only, or absent from the code.
---

# Reverse-Engineering GBFR Signatures

## Overview

The hook (`src-hook/`) reads game memory via **reverse-engineered byte-signatures (AOB scans)** and **struct field offsets**. A game patch that recompiles `granblue_fantasy_relink.exe` shifts both, silently. This skill re-derives them.

Two tools, two jobs:
- **`sigscan`** (Rust, in-repo) — scans the *on-disk exe*. Fast. Confirms a pattern's match count, reads offset operands, follows calls, dumps bytes. **Your primary tool.**
- **Ghidra lean analysis** — when you need a function's true **entry point** (for a hook target). `sigscan` can follow a `call`, but the call target may be a *callee mid-logic*, not a clean entry — hooking that crashes the game. Ghidra gives the real function boundary.

Core rule: **a signature is only trustworthy when `sigscan` reports exactly 1 match. A hook target is only safe when Ghidra confirms it's a function ENTRY with the right argument count.**

## When to use

- Hook log (`%APPDATA%\gbfr-logs\gbfr-logs.txt`) shows `Could not find match for pattern` or `[hook FAIL] <name>`.
- An offset resolves but reads a wrong value (e.g. skill names show as `Skill <bignum>`, damage cap reads 0) — an inner struct field shifted.
- A hook crashes the game (access violation) — likely hooking a non-entry or wrong-arity function.
- After any GBFR game update.

Two failure classes, two approaches:

| Symptom | It's a... | Tool |
|---------|-----------|------|
| pattern 0 matches / offset warning | broken **AOB signature** | sigscan (re-anchor) |
| wrong value read, no crash | shifted **struct field offset** | sigscan `dumprva` + live diag |
| game crashes on hook | wrong **function target/arity** | Ghidra (find true entry) |
| every sig still matches, but a feature silently reads nothing | shifted **hardcoded RVA** | see "Relocating hardcoded RVAs" |

**A patch can move every hardcoded RVA while leaving every AOB signature matching.** That was the whole of 2.0.3: `.data` moved uniformly, the hook log looked healthy (`[hook ok]` across the board), and the only symptom was features quietly reading nothing. Whenever a patch breaks behaviour but not signatures, audit the `const *_RVA` blocks first — `grep -rn "RVA: usize" src-hook/ src-tauri/`.

## The sigscan harness

```sh
# from repo root; GBFR_EXE overrides the default Steam path
cargo run -p hook --bin sigscan --release -- "<pelite pattern>" [mode] [--all]
```

Modes: `slice_u32` (default, read a u32 operand at cursor), `slice_u8`, `addr` (follow a call, report target RVA), `dumprva <hexrva> [len]` (raw bytes at an RVA).

`--all` widens the scan from the code section to the **whole image**. Hook signatures are all code, so the default is right for them — but data lives outside it, and without `--all` a pattern in `.rdata`/`.data` silently reports **0 matches**. Needed for the RTTI vtable walk below.

Every match prints `pre:` (24 bytes before) + `at:` (48 bytes from the match) — essential for re-anchoring. **Want exactly 1 match.** See the file header in `src-hook/src/bin/sigscan.rs` for details.

**Pattern syntax (pelite):** `?` = wildcard byte, `'` = store-cursor marker (its RVA → `addrs[1]`, what `search_slice`/`search_address` return), `e8 $ { ' }` = follow the `call` and capture its target. `search_address` returns `base + cursor`.

### Re-deriving an offset signature

Old patterns embed **magic type-hash constants** (e.g. `0x887ae0b0` = LE `b0 e0 7a 88`) that **survive recompiles**. Anchor on the surviving constant:

1. `sigscan "<old pattern>"` → confirm it now gives 0 matches (the break).
2. Find the surviving constant nearby; dump bytes around it (`dumprva`).
3. Rebuild the pattern around the constant, place `'` at the operand you need.
4. `sigscan "<new pattern>"` until it gives **1 match** with the right value.

Compiler drift to expect: `mov eax,K`→`mov r8d,K` (`b8`→`41 b8`); base register `rsi`→`r14` (`48 8d 8e`→`49 8d 8e`); AVX sequences re-ordered.

### Relocating hardcoded RVAs (data globals)

A global is reached by some instruction's RIP-relative displacement, so recover it from the code that uses it:

1. `XrefsTo.java` on the **old analyzed DB** → the code sites referencing the stale RVA.
2. `DumpBytes.java` on the old DB → the bytes at that site.
3. Wildcard the disp32, scan the **new** exe, and compute `target = cursor_rva + 4 + disp` (plus any bytes trailing the disp32, e.g. 4 more for a trailing imm32 as in `cmp dword [rip+x], imm32`).

**Take the disp32's address from sigscan's reported `cursor_rva` — never by counting bytes in a hex dump by eye.** Miscounting is silent: the value still lands in the same BSS region, still looks like a plausible global, and simply reads nothing at runtime. A 2.0.3 pass mis-sited three displacements by 8 this way, shipped, and cost a full live-test round.

**The strongest check: byte-diff the whole enclosing function, old vs new.** These functions are typically identical apart from their displacement bytes, so the diff pins every displacement position exactly — and running the same arithmetic on the *old* function must reproduce the *old, known-good* value. That reproduction is the proof.

Cross-check two ways: derive the same global from two independent call sites, and compare against the section's common delta (2.0.3: every `.data` global moved `-0x3040`). **Uniformity is a check, not a derivation** — but any value that breaks it is far more likely to be your arithmetic than a real anomaly.

### Relocating vtable RVAs (RTTI walk)

Vtables have no RIP-relative xrefs to chase, so walk **MSVC RTTI** forward from the class name, which survives recompiles. Get the old names with `SymbolAt.java` on the analyzed DB, then in the new exe (all scans need `--all`, since none of this is in the code section):

```
".?AV<Class>@@" string  ->  TypeDescriptor       (the string sits at TD+0x10)
TypeDescriptor          ->  Complete Object Locator  (COL+0x0C holds the TD's RVA)
COL                     ->  vtable               (the qword at vtable-8 points at the COL)
```

COL layout: `sig(0)=1, offset(4), cdOffset(8), pTypeDescriptor(0xC), pClassDescriptor(0x10), pSelf(0x14)`.

**These classes carry ~20 COLs each** (one per base subobject of a deep multiple-inheritance hierarchy), so `sig==1` alone is not discriminating. The vtable that lands at `*(object)` — the one the hook compares — is the **subobject at offset 0**: filter `sig==1 && offset==0 && cdOffset==0 && pSelf==colRva`.

Always **round-trip**: feed each recovered vtable RVA back through the walk and confirm it yields the class you started from. That turns 18 guesses into 18 verified facts.

### Shifted struct field (wrong value, no crash)

The AOB sig is fine but a field *inside* the struct moved. Confirm with a wide live dump: build with the `dmgdiag` feature (`cargo build -p hook --features dmgdiag`) — `damage.rs` then logs every nonzero u32 in a window per real hit. Match the known value (a skill ID, a cap magnitude) to its new offset, update `ffi.rs`.

## Ghidra: finding a function's true entry

Needed only for **function-hook targets** (the thing you detour). One-time setup already done (Ghidra 12.1.2 at `C:\ghidra\ghidra_12.1.2_PUBLIC`, JDK 21 wired via `support\launch.properties`).

**For most fixes, use lean import (`-noanalysis`)** + a targeted Java post-script that disassembles only a local window — fast, no decompiler needed.

**When lean isn't enough** (the surviving byte-anchor keeps landing in generic FNV-1a hashmap-lookup helpers, so you can't tell the real accessor apart — this happens for player_load, the sigil/weapon/overmastery offsets, and the SBA family), you need the **decompiler + xrefs** from a full analysis pass.

**The analyzed DB already exists: `gbfr202fast` (project `C:\Users\Scott\ghidra-projects\gbfr`, ~1.7 GB `.rep`, persists).** For v2.0.2 you do NOT need to rebuild it — just query it (see "Decompile a function" below). Only re-run the analysis after a NEW game patch.

To (re)build it after a patch — do NOT run the *default* full analysis (multi-hour: the "Decompiler Parameter ID" analyzer alone ~10x's the time). Run a **fast analysis** with the slow analyzers disabled (still gives function boundaries, xrefs, decompiler, and C++ RTTI class names). **Measured: ~2h49m** on the 118 MB exe (i5-13600K, NVMe) — run it in the background and wait for the completion notification:

```sh
cd C:/ghidra/ghidra_12.1.2_PUBLIC/support
SKILL=C:/Users/Scott/Projects/gbfr-logs/.claude/skills/reverse-engineering-signatures/ghidra
GHIDRA_HEADLESS_MAXMEM=28G ./analyzeHeadless.bat \
  C:/Users/Scott/ghidra-projects/gbfr gbfr<version>fast \
  -import "<path to granblue_fantasy_relink.exe>" -processor x86:LE:64:default \
  -scriptPath "$SKILL" -preScript FastAnalysisOptions.java   # run in background
```

`FastAnalysisOptions.java` disables `Decompiler Parameter ID`, `Decompiler Switch Analysis`, `Call Convention ID`, `Aggressive Instruction Finder`, `Stack`, and logs `getOptionNames()` to prove it applied. **VERIFY** the run log shows `FastAnalysisOptions: DISABLED '<name>'` lines. Success markers at the end: `Analysis succeeded` + `Save succeeded` + `Import succeeded`. **Gotchas:** `-preScript` needs `-scriptPath <dir> -preScript <bare-name.java>` (an absolute path arg → ClassNotFound). Use ≤28G heap (heap is hard-capped by `-Xmx`; won't exceed it). Fresh project name so the lean DB stays usable. The late RTTI phase (`CreateRtti4BackgroundCmd`, "Unprocessed TypeDescriptor" lines) is CPU-light, single-threaded, and logs sparsely — a quiet log ≠ hung; it also emits non-fatal `ERROR No vfTable found` / `VarnodeContext: out of address spaces` and continues. **When checking if it's alive, find the RIGHT java PID** — `Get-CimInstance Win32_Process -Filter "Name='java.exe'" | select ProcessId,CommandLine` and pick the one whose command line contains `ghidra.GhidraClassLoader` (NOT the Cursor/VSCode `redhat.java` LSP), then sample that PID's `TotalProcessorTime` over 10s (>0 = working).

### Decompile a function (the analyzed DB's superpower)

`Decompile.java` prints a function's C. Use it to read what a function actually DOES — arg meaning, which enum values branch where, what struct fields it touches — instead of guessing from asm or chasing byte idioms into generic helpers.

```sh
cd C:/ghidra/ghidra_12.1.2_PUBLIC/support
SKILL=C:/Users/Scott/Projects/gbfr-logs/.claude/skills/reverse-engineering-signatures/ghidra
GHIDRA_HEADLESS_MAXMEM=12G ./analyzeHeadless.bat \
  C:/Users/Scott/ghidra-projects/gbfr gbfr202fast -process granblue_fantasy_relink.exe \
  -noanalysis -scriptPath "$SKILL" -postScript Decompile.java 0x3f1330 0x63ecb0   # 1+ RVAs
```

Filter with `grep 'Decompile.java>'`. Runs against the analyzed `gbfr202fast` DB (the decompiler needs analysis). `-noanalysis` here just means "don't re-analyze on open." Takes the RVA of any byte inside the target function (it resolves the containing function). Example payoff: decompiling the result-screen router `FUN_1403f1330` showed `if (0x13 < param_2) return; if (*(char*)(param_1 + 0xed4 + param_2*0x40) == 0) return;` — i.e. result_type is a per-screen table index, NOT a quest-clear flag, proving that hardcoding `result_type == N` is fragile. That's the kind of ground truth lean analysis can't give you.

### One-time: import the exe (≈2 min)

```sh
cd C:/ghidra/ghidra_12.1.2_PUBLIC/support
GHIDRA_HEADLESS_MAXMEM=24G ./analyzeHeadless.bat \
  C:/Users/Scott/ghidra-projects/gbfr gbfr202lean \
  -import "<path to granblue_fantasy_relink.exe>" \
  -processor x86:LE:64:default -noanalysis
```

The saved program DB lives at `C:\Users\Scott\ghidra-projects\gbfr\gbfr202lean.rep` (~2.3 GB) and **persists**. Re-import only after a new game patch (use a fresh project name, e.g. `gbfr<version>lean`). The project's **parent dir must exist** and Ghidra 12 needs `-processor` explicit.

### Per-query: find entries / inspect a function (≈90 s each)

Scripts live in `ghidra/` next to this skill. Run against the saved DB with `-process` (no re-import). **Ghidra 12 has no Python by default — these are Java `GhidraScript`s.**

```sh
cd C:/ghidra/ghidra_12.1.2_PUBLIC/support
SKILL=C:/Users/Scott/Projects/gbfr-logs/.claude/skills/reverse-engineering-signatures/ghidra

# Find the true ENTRY of the function CONTAINING each anchor RVA (a byte inside it):
GHIDRA_HEADLESS_MAXMEM=24G ./analyzeHeadless.bat \
  C:/Users/Scott/ghidra-projects/gbfr gbfr202lean -process granblue_fantasy_relink.exe \
  -noanalysis -scriptPath "$SKILL/ghidra" -postScript FindEntry.java 0x3f13b5 0x63ecb0

# Inspect a function ENTRY (callers, callees, string refs, arg usage from prologue):
GHIDRA_HEADLESS_MAXMEM=24G ./analyzeHeadless.bat \
  C:/Users/Scott/ghidra-projects/gbfr gbfr202lean -process granblue_fantasy_relink.exe \
  -noanalysis -scriptPath "$SKILL/ghidra" -postScript InspectFunc.java 0x3f1330
```

Output is verbose; filter with `grep 'FindEntry.java>\|InspectFunc.java>'`. Run in background (`run_in_background`) — JVM startup dominates.

### Reading the prologue for argument count (critical)

The prologue tells you the arity — **get this wrong and the detour crashes the game**. In the x64 MS ABI, args arrive in `rcx, rdx, r8, r9`. If the prologue reads `rdx`/`edx`/`r8`/`r9` early, the function takes that many args:

```
cmp edx, 0x13      ; uses arg2 → at least a 2-arg fn
mov rbx, rcx       ; arg1
```

Match your `retour` detour signature and `.call(...)` to the real arity. A 1-arg detour on a 2-arg function leaves a register garbage → access violation.

## Tracing a VALUE through the binary (not a signature)

Sometimes the question is not "where is this function" but "where does this
number come from" — a status cause id, a damage-type constant, a quest flag.
Different discipline, and the failure mode is the opposite of a broken sig: you
get a **confident wrong negative** instead of a loud error.

### Never conclude "it isn't in the binary"

**A null result from a scan only means your scan was too narrow.** It is never
evidence the value is absent, and reporting it as such sends everyone chasing
runtime captures for something sitting in the code.

A real 2026-08-02 failure: a sweep of one API's call sites found no `1100`, and
that was written up as "this cause is data-driven, static evidence exhausted."
`1100` was in fact `MOV R8D,0x44c` in the very function that applies it — it
just reached the field through a *different* applier. One extra sweep of the
missed family recovered **18 cause values** in a single run.

Before writing "not in the binary", you must have done all three:

1. **Derived** the function family (below), not guessed it.
2. **Position-aware** argument parsing for every member of that family.
3. **Dataflow** for the register-passed cases — not a text scan.

If you have not, the honest phrasing is "not found by <method>; <method> cannot
see X" — say what you searched, not what exists.

### Derive the call-site family, never guess it

Wrappers around one implementation take **different argument positions**, so a
sweep written for one of them silently misreads the others.

```sh
# 1. Who calls the implementation? These are the wrappers.
-postScript XrefsTo.java <impl_rva>
# 2. Repeat on each wrapper until the set stops growing (transitive closure).
# 3. Only now sweep every member.
```

Stop when a round adds nothing. Values can also reach a call through a **field
store** (`MOV [R14+0x8b4],0x44c` … later read and passed), which no call-site
scan can see — those need the decompiler.

### The three queries, in order

| Question | Tool |
|---|---|
| Which constants does each caller pass? | `CallSiteArgs.java <target>` — all 8 arg slots per site |
| Where is this constant used at all? | `ImmSites.java <value> …` — whole-listing scan, with containing function |
| Where did this register's value come from? | `Decompile.java` — read the dataflow; a scan cannot |

`CallSiteArgs` reports `?` (nothing in window) and `reg:NAME` (came from a
register). **Both mean UNKNOWN, not absent** — resolve them by decompiling, or
your summary will encode the same wrong negative.

Interpreting `ImmSites` output: histogram the *instruction kinds* before
concluding anything. 118 `AND` + 86 `TEST` of a value means it is a **mask**;
`CMP` means a comparison; only a `MOV` into an argument slot is a passed value.
But a value absent from this scan may still arrive via a field or a table.

### Cross-check with a second stream

Static call-site attribution and a live `hookdiag` capture are independent. When
they disagree, the static sweep is usually incomplete (see above). When they
agree, the fact is solid. The hook log is at `%APPDATA%\gbfr-logs\gbfr-logs.txt`
and reaches **gigabytes** — always `tail -n` it, never read it whole.

## End-to-end: fixing one function hook

1. Find a byte fingerprint that survived, *inside* the target (use `sigscan` to confirm it's unique).
2. `FindEntry.java <anchor_rva>` → true entry RVA (self-check: a known-good anchor must resolve to its own entry).
3. `InspectFunc.java <entry_rva>` → confirm it's the right function (callers, behavior) and read its arg count from the prologue.
4. Build a signature that resolves to the entry. Direct-entry form: match the preceding `ret`+`int3` padding then the prologue, cursor at entry — `c3 cc cc cc cc ' <prologue bytes>`. Verify with `sigscan "<pattern>" addr` → 1 match, `target_rva` = the entry.
5. Update the hook's `const *_SIG`, the `type` alias, `static_detour!`, and `run()`/`.call()` to the correct arity.
6. `cargo build -p hook --release`, then copy to `src-tauri/hook.dll` (see below), restart game, test.

## Injection gotcha

The app injects `src-tauri/hook.dll` from CWD (dev). `build.rs` refreshes it *only* on a backend recompile, so after rebuilding just the hook:

```sh
cp -f target/release/hook.dll src-tauri/hook.dll
```

The injected DLL is **locked while the game runs** — close the game to swap it. Re-injecting a same-named already-loaded DLL is a silent no-op.

## Common mistakes

- **Hand-counting a disp32's position in a hex dump.** Use sigscan's `cursor_rva`, or byte-diff the enclosing function. An off-by-N lands in the same BSS region and reads as a plausible global, so nothing errors — the feature just silently returns nothing.
- **Assuming the OLD exe is still around to compare against.** Steam patches it **in place**, so the moment you launch after an update the old binary is gone. Its Ghidra DB is the only surviving record — keep the previous version's `.rep` until the new one is fully derived and live-verified.
- **Trusting a unique `call`-follow match as a hook target.** Unique ≠ clean entry. The followed target can be a callee mid-function. Always confirm the entry with Ghidra before detouring. (This crashed the quest hook on v2.0.2: the old sig followed a call to a 2-arg helper hooked as 1-arg.)
- **Running the DEFAULT full Ghidra auto-analysis.** Multi-hour (Decompiler Parameter ID ~10x's it). For quick lookups use lean `-noanalysis` + targeted scripts; when you genuinely need the decompiler, use the *fast* analysis (analyzers disabled via `FastAnalysisOptions.java`) — and note the analyzed `gbfr202fast` DB already exists for v2.0.2, so query it, don't rebuild.
- **Misidentifying the Ghidra java PID as hung.** Sampling the wrong `java.exe` (e.g. the editor's `redhat.java` LSP) shows 0 CPU and looks dead. Match on `ghidra.GhidraClassLoader` in the command line first.
- **Writing Ghidra scripts in Python.** Ghidra 12 has no Python without PyGhidra. Use Java `GhidraScript`s.
- **Assuming a surviving AOB sig means correct data.** Inner struct offsets shift independently and fail *silently* as wrong numbers — verify the value, not just the match.
- **Hand-computing RVA→file-offset with a fixed delta.** Wrong for high sections. Use `sigscan dumprva` (correct pelite addressing).
- **Reporting a scan's null result as "not in the binary".** The single most expensive mistake in value tracing: it looks like a finding and sends everyone to runtime capture for something that is in the code. Derive the call-site family, parse every wrapper's own argument positions, and use the decompiler for register-passed values before claiming absence. See "Tracing a VALUE through the binary".
- **Guessing which functions form an applier/wrapper family.** Wrappers take different argument positions, so a sweep written for one misreads the rest. Walk `XrefsTo` on the implementation transitively until the set stops growing.
- **Deleting diagnostic logging.** The `dmgdiag` feature block and `console`-gated prints are kept on purpose for the next patch.

## Files

- `src-hook/src/bin/sigscan.rs` — the harness (in-repo, committed).
- `ghidra/DumpBytes.java` — RVA(s) → raw hex bytes from the program (lean DB). THE query for "what did the OLD exe look like here" once Steam has patched the exe in place and only its Ghidra DB survives. Pair with `XrefsTo`: xref site → `DumpBytes` → wildcard the disp32 → scan the NEW exe → read the new displacement.
- `ghidra/SymbolAt.java` — RVA(s) → the symbols defined there (needs the **analyzed** DB). The inverse of `ListSymbols`. THE query for "what class is this vtable RVA", i.e. the first step in relocating a hardcoded vtable list after a patch.
- `ghidra/FindEntry.java` — anchor RVA(s) → containing-function entry RVA + prologue bytes (lean DB).
- `ghidra/InspectFunc.java` — entry RVA(s) → callers, callees, string refs, prologue disasm (lean DB).
- `ghidra/FindByBytes.java` — byte pattern → containing-function entry for each hit (lean DB).
- `ghidra/FindStringRefs.java` — ASCII substring → enclosing C-strings → code xrefs + containing-function entries (needs the **analyzed** DB for xrefs).
- `ghidra/XrefsTo.java` — RVA(s) → every referencing site, deduped by containing function with per-function counts. THE query for "who touches this global/vtable/function" (needs the **analyzed** DB).
- `ghidra/ListSymbols.java` — case-insensitive substring search over the symbol table (RTTI class/vtable names) (needs the **analyzed** DB).
- `ghidra/FindVCallSlot.java` — slot displacement (e.g. `0x48`) → every indirect `CALL qword ptr [reg + disp]` site with its containing function, plus the surrounding instructions. THE query for "who calls virtual slot N", which `XrefsTo` cannot answer: a virtual call references only the vtable, never the callee. Scans the listing, so data bytes that happen to match are never reported. Expect many hits — filter by the caller's code region and by how the out-param is used.
- `ghidra/DisasmCalls.java` — target function RVA → every call site with the ~8 instructions preceding it (optionally filtered to given containing functions). Raw disassembly per site; use `CallSiteArgs.java` instead when you want the arguments already parsed.
- `ghidra/CallSiteArgs.java` — target RVA → per call site, the last immediate written to EACH x64 argument slot (RCX/RDX/R8/R9 + `[RSP+0x20..0x38]`), as CSV. THE query for "which constants does each caller pass". Emits `?` (nothing in window) and `reg:NAME` (register-sourced) — **both mean UNKNOWN, resolve with the decompiler, never read as "absent"**.
- `ghidra/CallSiteArgsMulti.java` — same report for MANY targets in one headless run (JVM startup dominates), and tracks 12 slots through `[RSP+0x58]` — the status-apply family passes its cause as deep as positional arg 10, which the 8-slot scan silently missed (that blind spot hid cause 0xFFFFF).
- `ghidra/FnStrings.java` — function-entry RVA(s) → the defined strings referenced inside each function body. THE attribution query for "which character/entity owns this code": per-character code carries tag strings (`pl0400`, `em1200`, `ab_scourge_vanish`). Expect sparse results — most functions reference no strings; fall back to class-binder symbols and address-band inference (character code is contiguous and ordered by character id).
- `ghidra/FindOperandText.java` — substring → every instruction whose rendered text contains it, with containing function. THE query for "who touches struct field +0xNNNN": a displacement inside `[RSI + 0xNNNN]` is not a scalar operand, so `ImmSites.java` cannot see it. Histogram the hits (reads/writes/CMP bases) before concluding; a field with only zeroing writers is written through computed pointers elsewhere.
- `ghidra/ImmSites.java` — value(s) → every instruction in the listing whose scalar operand matches, with containing function. THE query for "where is this constant used at all". Histogram the instruction kinds before concluding: many `AND`/`TEST` = a mask, `CMP` = a comparison, only a `MOV` into an argument slot is a passed value.
- `ghidra/DumpVtableSlot.java` — class-name substring + slot displacement → the function each matching `<Class>::vftable` holds at that slot, with a tally of distinct targets. Answers "which subclasses OVERRIDE this virtual and which inherit the base", i.e. exactly how many detours a virtual needs. Classes with multiple vftables (multiple inheritance) or short vtables produce misaligned reads — sanity-check a candidate by decompiling it before trusting it.
- `ghidra/Decompile.java` — RVA(s) → decompiled C of the containing function (needs the **analyzed** `gbfr202fast` DB).
- `ghidra/FastAnalysisOptions.java` — pre-script that disables slow analyzers for the fast full-analysis build.

**Ghidra DBs** (all under `C:\Users\Scott\ghidra-projects\gbfr`, all persist). Two kinds per game version: `gbfr<ver>lean` (import-only, for fast FindEntry/InspectFunc/FindByBytes lookups) and `gbfr<ver>fast` (fully analyzed, for `Decompile.java` + xrefs + `SymbolAt`/C++ RTTI names). Re-create both only after a new game patch.

Present: `gbfr202*` (v2.0.2), `gbfr203*` (v2.0.3), `gbfr204*` (v2.0.4) and **`gbfr205lean` / `gbfr205fast`** (v2.0.5, current). The exe lives at `G:\SteamLibrary\steamapps\common\Granblue Fantasy Relink\` (sigscan's default), not under `C:\Program Files (x86)\Steam`. **Keep the previous version's DBs** — the 2.0.3 fix was derived almost entirely by querying `gbfr202fast` for xrefs and old bytes, which is impossible once Steam has overwritten the old exe.

Detailed, evolving findings for the current patch live in the memory file `gbfr-endless-ragnarok-break` (verified entries, offsets, and per-hook status).
