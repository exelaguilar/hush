# Builds Hush-Setup.exe: the release app, setup tool and audio plugin,
# embedded into a single installer in dist\.
#
#   powershell -ExecutionPolicy Bypass -File .\package.ps1

$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

# The installer's build script brotli-compresses the payload (quality 11);
# optimize build scripts so that takes seconds, not minutes.
$env:CARGO_PROFILE_RELEASE_BUILD_OVERRIDE_OPT_LEVEL = '3'

Write-Host 'Building Hush (release)...' -ForegroundColor Cyan
cargo build --release --workspace --exclude hush-installer
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host 'Building installer...' -ForegroundColor Cyan
$env:SM_PAYLOAD_DIR = Join-Path $PSScriptRoot 'target\release'
cargo build --release -p hush-installer
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches.Groups[1].Value
New-Item -ItemType Directory -Force dist | Out-Null
$target = Join-Path 'dist' "Hush-Setup-$version.exe"
Copy-Item 'target\release\hush-installer.exe' $target -Force
$hash = (Get-FileHash $target -Algorithm SHA256).Hash.ToLower()
[System.IO.File]::WriteAllText((Join-Path (Get-Location) "dist\Hush-Setup-$version.sha256"), "$hash  Hush-Setup-$version.exe`n")
$size = [math]::Round((Get-Item $target).Length / 1MB, 1)
Write-Host "Created $target ($size MB)" -ForegroundColor Green
