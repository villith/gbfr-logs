---
name: building-gbfr-logs
description: Use when building, compiling, packaging, or producing an installer/MSI for the GBFR Logs Tauri app, or when a build fails with "path matching hook.dll not found" or build.rs panics. Covers the hook-DLL prerequisite that `npm run tauri build` does not handle on its own.
---

# Building GBFR Logs

## Overview

GBFR Logs is a Tauri app with a separate injectable hook DLL crate. **The release build bundles `src-tauri/hook.dll` as a Tauri resource, but no single command produces it.** You must build the hook crate and place the DLL *before* running `tauri build`, or the build fails.

Core rule: **build `hook.dll` first, copy it into `src-tauri/`, then build the app.**

## Just build it

Run the script — it does every step in the right order:

```powershell
./scripts/build.ps1              # full release build (+ MSI bundle)
./scripts/build.ps1 -SkipInstall # skip npm install if deps are present
./scripts/build.ps1 -Dev         # npm run tauri dev instead of a release build
```

## The failure this prevents

Running `npm run tauri build` directly (without the hook DLL) fails with:

```
thread 'main' panicked at src-tauri\build.rs:15:14:
Could not build Tauri app.: path matching hook.dll not found.
```

Cause: `tauri.conf.json` lists `hook.dll` under `resources`, but the hook crate
(`src-hook/`, a `cdylib`) is never built by the `tauri` command. In **debug** builds,
`build.rs` copies `target/release/hook.dll` itself; in **release** builds it does not,
so the DLL must already exist at `src-tauri/hook.dll`.

## Manual steps (what the script does)

This order matches `.github/workflows/ci.yaml`:

1. `npm install`
2. `cargo build --release --package hook` → produces `target/release/hook.dll`
3. Copy `target/release/hook.dll` → `src-tauri/hook.dll`
4. `npm run tauri build` (the frontend `tsc && vite build` runs automatically via `beforeBuildCommand`)

## Quick reference

| Goal | Command |
|------|---------|
| Full release build + installer | `./scripts/build.ps1` |
| Frontend only (typecheck + bundle) | `npm run build` |
| Frontend dev server only | `npm run dev` |
| Hook DLL only | `cargo build --release --package hook` |
| Run the app in dev | `npm run tauri dev` (builds hook DLL via `npm run dev`) |

## Prerequisites

- **Nightly Rust** (the toolchain is pinned; `rustup` will sync it automatically).
- **Node.js + npm.**
- Building the app to *run* it against the game requires Windows + admin + a live game process — but **compiling** does not.

## Common mistakes

- **Running `npm run tauri build` first.** Fails on missing `hook.dll`. Build the hook crate first (use the script).
- **Trusting a piped exit code.** `npm run tauri build | tail` reports the *pipe's* exit code (0), masking the real failure. Run the command unpiped, or redirect to a file and check `$LASTEXITCODE`.
- **Treating the `TAURI_PRIVATE_KEY` error as a build failure.** The build ends with `A public key has been found, but no private key... TAURI_PRIVATE_KEY`. This is **only** the auto-updater *signing* step (needs the maintainer's private key) and does **not** affect the produced exe or MSI. Because of this, `tauri build` itself **exits non-zero even on success** — judge success by whether `target/release/GBFR Logs.exe` was produced (which is what `scripts/build.ps1` does), not by the exit code.
- **Editing `scripts/build.ps1` and setting `$ErrorActionPreference = 'Stop'`.** In Windows PowerShell 5.1 that turns cargo/npm *stderr progress lines* (e.g. cargo's `Finished release` banner) into terminating `NativeCommandError`s, aborting the script mid-build even on exit code 0. Check `$LASTEXITCODE` per step instead.

## Artifacts

- `target/release/GBFR Logs.exe` — the app binary (named from `productName`, not the crate).
- `target/release/hook.dll` — the injectable hook.
- `target/release/bundle/msi/GBFR Logs_<version>_x64_en-US.msi` — the installer.

## Note on `src-tauri/hook.dll`

It is a generated artifact (the build script regenerates it each run). Don't commit it
unless the repo already tracks it; treat it like other build output.
