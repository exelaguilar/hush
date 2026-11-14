# Builds Hush-Setup.exe: the release app, setup tool and audio plugin,
# embedded into a single installer in dist\.
#
#   powershell -ExecutionPolicy Bypass -File .\package.ps1

$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

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
$size = [math]::Round((Get-Item $target).Length / 1MB, 1)
Write-Host "Created $target ($size MB)" -ForegroundColor Green
