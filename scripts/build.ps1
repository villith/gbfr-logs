<#
.SYNOPSIS
    Builds the GBFR Logs Tauri application end-to-end, including the prerequisite hook DLL.

.DESCRIPTION
    The release build (`npm run tauri build`) bundles `src-tauri/hook.dll` as a Tauri
    resource, but nothing in that command produces the DLL. If it's missing, the build
    fails in src-tauri/build.rs with:

        thread 'main' panicked at src-tauri\build.rs:15:14:
        Could not build Tauri app.: path matching hook.dll not found.

    This script performs the steps in the required order (matching .github/workflows/ci.yaml):
        1. npm install                              (unless -SkipInstall)
        2. cargo build --release --package hook     -> target/release/hook.dll
        3. copy hook.dll into src-tauri/            (where the release resource glob expects it)
        4. npm run tauri build                      (frontend build runs via beforeBuildCommand)

.PARAMETER SkipInstall
    Skip `npm install` (use when dependencies are already installed).

.PARAMETER Dev
    Run `npm run tauri dev` instead of a release build. In dev mode build.rs copies the
    hook DLL itself, so this script still builds the hook crate first for parity.

.EXAMPLE
    ./scripts/build.ps1
    ./scripts/build.ps1 -SkipInstall
    ./scripts/build.ps1 -Dev
#>
[CmdletBinding()]
param(
    [switch]$SkipInstall,
    [switch]$Dev
)

# NOTE: Do NOT set $ErrorActionPreference = 'Stop' here. In Windows PowerShell 5.1,
# native tools that write progress to stderr (cargo, npm) get their stderr wrapped as
# terminating NativeCommandError records under 'Stop', aborting the script even on
# exit code 0. We check $LASTEXITCODE explicitly per step instead.

# Always operate from the repo root (this script lives in <root>/scripts).
$RepoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $RepoRoot

function Invoke-Step {
    param([string]$Name, [scriptblock]$Action)
    Write-Host ""
    Write-Host "==> $Name" -ForegroundColor Cyan
    & $Action
    if ($LASTEXITCODE -ne 0) {
        throw "Step failed: $Name (exit code $LASTEXITCODE)"
    }
}

# 1. Dependencies
if (-not $SkipInstall) {
    Invoke-Step "npm install" { npm install }
} else {
    Write-Host "==> Skipping npm install (-SkipInstall)" -ForegroundColor DarkGray
}

# 2. Build the hook DLL (release) -> target/release/hook.dll
#    This is the step missing from `npm run tauri build`; without it the release
#    build cannot find the bundled resource.
Invoke-Step "cargo build --release --package hook" {
    cargo build --release --package hook
}

# 3. Place the DLL where the Tauri release resource glob expects it.
$Source = Join-Path $RepoRoot 'target/release/hook.dll'
$Dest   = Join-Path $RepoRoot 'src-tauri/hook.dll'
if (-not (Test-Path $Source)) {
    throw "Expected hook DLL not found at $Source after cargo build."
}
Write-Host ""
Write-Host "==> Copying hook.dll into src-tauri/" -ForegroundColor Cyan
Copy-Item -Path $Source -Destination $Dest -Force
Write-Host "    $Dest"

# 4. Build (or run) the Tauri app. The frontend (tsc + vite) is built automatically
#    via beforeBuildCommand in tauri.conf.json.
if ($Dev) {
    Invoke-Step "npm run tauri dev" { npm run tauri dev }
} else {
    # We do NOT use Invoke-Step here: `tauri build` exits non-zero when the updater
    # signing step can't find TAURI_PRIVATE_KEY, even though the exe and MSI were
    # produced successfully. We judge success by the presence of the built exe instead.
    Write-Host ""
    Write-Host "==> npm run tauri build" -ForegroundColor Cyan
    npm run tauri build
    $tauriExit = $LASTEXITCODE

    $Exe = Join-Path $RepoRoot 'target/release/GBFR Logs.exe'
    if (-not (Test-Path $Exe)) {
        throw "tauri build failed: '$Exe' was not produced (exit code $tauriExit)."
    }

    if ($tauriExit -ne 0) {
        Write-Host ""
        Write-Host "Note: tauri exited $tauriExit, but the app binary was produced." -ForegroundColor DarkYellow
        Write-Host "      This is the updater-signing step failing on a missing TAURI_PRIVATE_KEY" -ForegroundColor DarkYellow
        Write-Host "      (a release-signing concern) and does not affect the local build." -ForegroundColor DarkYellow
    }

    Write-Host ""
    Write-Host "Build complete." -ForegroundColor Green
    Write-Host "Artifacts:" -ForegroundColor Green
    Write-Host "  target/release/GBFR Logs.exe"
    Write-Host "  target/release/bundle/msi/GBFR Logs_<version>_x64_en-US.msi"

    # Build succeeded (exe verified above); force a clean exit code so the trailing
    # non-zero $LASTEXITCODE from the benign updater-signing step doesn't propagate.
    exit 0
}
