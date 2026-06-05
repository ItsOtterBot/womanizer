# Womanizer — Windows build prerequisites installer.
#
# Idempotent: detects whether libclang.dll is already on the system and exits
# 0 if so. Otherwise installs LLVM via winget (preferred — no admin needed
# for the per-user package) or chocolatey (requires admin) so subsequent
# `cargo build` / `cargo test` succeed on a clean Windows checkout.
#
# Background: signalsmith-stretch 0.1.3 pulls bindgen ^0.70 as a build-dep.
# bindgen needs libclang.dll at build time; MSVC and Visual Studio Build
# Tools do not ship it. See README.md "Build Prerequisites (Windows)".
#
# Usage (from the repo root):
#   powershell -ExecutionPolicy Bypass -File scripts\setup-windows.ps1
#
# Re-runs are safe — the script no-ops when libclang is already present.

$ErrorActionPreference = "Stop"

function Test-Libclang {
    $candidates = @(
        "$env:ProgramFiles\LLVM\bin\libclang.dll",
        "${env:ProgramFiles(x86)}\LLVM\bin\libclang.dll",
        "$env:LocalAppData\Programs\LLVM\bin\libclang.dll"
    )
    if ($env:LIBCLANG_PATH) {
        $candidates += (Join-Path $env:LIBCLANG_PATH "libclang.dll")
    }
    foreach ($p in $candidates) {
        if (Test-Path -LiteralPath $p) {
            Write-Host "[ok] Found libclang at $p"
            return $true
        }
    }
    if (Get-Command clang.exe -ErrorAction SilentlyContinue) {
        Write-Host "[ok] clang.exe is on PATH"
        return $true
    }
    return $false
}

if (Test-Libclang) {
    Write-Host "[ok] LLVM/libclang is already installed — no action needed."
    Write-Host "[ok] Next step: cargo build --release"
    exit 0
}

Write-Host "[setup] libclang.dll not found. Installing LLVM..."
Write-Host ""

$installed = $false

if (-not $installed -and (Get-Command winget.exe -ErrorAction SilentlyContinue)) {
    Write-Host "[setup] Using winget (preferred — no admin required)..."
    try {
        winget install --id LLVM.LLVM --silent `
            --accept-package-agreements --accept-source-agreements
        if ($LASTEXITCODE -eq 0) {
            $installed = $true
        } else {
            Write-Warning "winget exited with code $LASTEXITCODE — falling through to chocolatey."
        }
    } catch {
        Write-Warning "winget threw: $($_.Exception.Message) — falling through to chocolatey."
    }
}

if (-not $installed -and (Get-Command choco.exe -ErrorAction SilentlyContinue)) {
    Write-Host "[setup] Using chocolatey (requires admin)..."
    choco install llvm --yes --no-progress
    if ($LASTEXITCODE -eq 0) {
        $installed = $true
    } else {
        Write-Warning "choco exited with code $LASTEXITCODE."
    }
}

if (-not $installed) {
    Write-Error @"
Neither winget nor chocolatey could install LLVM.

Install one of the following package managers, then re-run this script:
  - winget : ships with Windows 10/11 (App Installer in the Microsoft Store)
  - choco  : https://chocolatey.org/install (requires admin PowerShell)

Or install LLVM manually from
  https://github.com/llvm/llvm-project/releases
(pick the LLVM-<version>-win64.exe artifact and tick "Add LLVM to the system PATH").
"@
    exit 1
}

Write-Host ""
Write-Host "[ok] LLVM installed."
Write-Host "[!] IMPORTANT: close and reopen your terminal so the new PATH entries"
Write-Host "    (and libclang.dll) become visible. Then run: cargo build --release"
