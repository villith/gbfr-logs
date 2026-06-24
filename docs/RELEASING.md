# Releasing GBFR Logs

This fork (`villith/gbfr-logs`) publishes its own signed releases and auto-updates.
Releases are driven by the `Release` workflow (`.github/workflows/release.yaml`),
which builds + signs the Windows MSI and creates a GitHub Release when a version
tag is pushed.

## One-time setup (do this before the first release)

### 1. Updater signing keypair (already generated)

The auto-updater signs every update bundle with a **private** key and verifies it
against a **public** key embedded in the app.

A keypair has already been generated for this fork (no password):

- **Public key** — set in `src-tauri/tauri.conf.json` → `tauri.updater.pubkey`.
- **Private key** — `.tauri/gbfr-logs.key` (and `.tauri/gbfr-logs.key.pub`). This
  directory is **gitignored** (`.tauri/`, `*.key`) and must never be committed.

To regenerate (e.g. if the key is lost — note: existing installs can then no longer
auto-update and must reinstall manually):

```sh
npx tauri signer generate --ci -p "" -w .tauri/gbfr-logs.key
```

…then copy the printed/`.pub` public key into `tauri.updater.pubkey`.

### 2. Add the private key as GitHub Actions secrets

The release workflow needs the private key. In the repo:
**Settings → Secrets and variables → Actions → New repository secret**

- `TAURI_PRIVATE_KEY` — the full contents of `.tauri/gbfr-logs.key`.

You do **not** need a `TAURI_KEY_PASSWORD` secret: this key was generated without a
password. The workflow references it, but a missing secret resolves to an empty
string, which is exactly "no password". Only add it if you regenerate the key
**with** a password.

> The private key file lives only on the machine that generated it (it's gitignored).
> Back it up somewhere safe — losing it breaks auto-updates for all installed clients.

The release workflow reads these to sign the bundle. A GitHub secret does nothing
until a workflow consumes it — that's what `release.yaml` is for.

### 3. Point the updater endpoint at this repo

Already done: `tauri.updater.endpoints` points at
`https://raw.githubusercontent.com/villith/gbfr-logs/main/update.json`.

## Cutting a release

1. **Bump the version** in all three files (they must agree):
   `package.json`, `src-tauri/Cargo.toml`, `src-tauri/tauri.conf.json`
   (and let `cargo` update `Cargo.lock`). Commit as `build: <version>` and merge to
   `main`.

2. **Tag and push** the version (the workflow triggers on tags like `1.9.0`):

   ```sh
   git tag 1.9.0
   git push origin 1.9.0   # use your fork remote name (e.g. `fork`)
   ```

   The `Release` workflow builds the hook DLL, builds + signs the app, and creates a
   **draft** GitHub Release with the `.msi`, the updater `.msi.zip`, and a
   `latest.json` containing the signature.

3. **Update `update.json`** (repo root) so installed apps see the new version. From
   the workflow's output / release assets, set:
   - `version` → the new version (e.g. `1.9.0`)
   - `platforms.windows-x86_64.url` → the uploaded `GBFR Logs_<ver>_x64_en-US.msi.zip`
     release-asset URL
   - `platforms.windows-x86_64.signature` → the contents of the generated `.sig`
     (also surfaced in the workflow-generated `latest.json`)
   - `notes`, `pub_date` → as appropriate

   Commit `update.json` to `main` (the endpoint serves it from `main`).

4. **Publish** the draft GitHub Release.

### Pausing auto-updates between releases

`update.json` can be pointed at an *older* version on purpose to stop clients from
auto-updating while a release is being staged, then flipped forward once the new
release is fully live. (This is why the upstream history has commits that move
`update.json` back and forth.)

## What the CI does NOT do

`ci.yaml` only runs lint/format/typecheck/tests — it does not build or release.
`release.yaml` (this setup) is what produces signed artifacts, and only on a tag push.
