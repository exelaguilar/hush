# Compare the tagged and candidate DSP on identical synthetic input.
param([string] $Baseline = 'v1.2.0', [switch] $Offline)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("HushPcm-" + [guid]::NewGuid().ToString('N'))
$previousTarget = $env:CARGO_TARGET_DIR
$previousLocation = Get-Location
$null = New-Item -Path $work -ItemType Directory
try {
    Set-Location $root
    $zip = Join-Path $work 'baseline.zip'
    git archive --format=zip --output=$zip $Baseline
    if ($LASTEXITCODE -ne 0) { throw "Could not read baseline tag $Baseline" }
    $baselineDir = Join-Path $work 'baseline'
    Expand-Archive -LiteralPath $zip -DestinationPath $baselineDir
    $exampleDir = Join-Path $baselineDir 'apo/examples'
    $null = New-Item -Path $exampleDir -ItemType Directory -Force
    Copy-Item -LiteralPath (Join-Path $root 'apo/examples/pcm_regression.rs') -Destination $exampleDir
    $targetDir = if ($previousTarget) { Join-Path $previousTarget 'pcm-regression' } else { Join-Path $root 'target' }
    $env:CARGO_TARGET_DIR = $targetDir
    $flags = @('--locked', '--release', '-p', 'hush-apo', '--example', 'pcm_regression')
    if ($Offline) { $flags += '--offline' }

    $current = Join-Path $work 'current.pcm'
    cargo run @flags -- $current
    if ($LASTEXITCODE -ne 0) { throw 'Candidate render failed' }
    $tagged = Join-Path $work 'baseline.pcm'
    Set-Location $baselineDir
    cargo run @flags -- $tagged
    if ($LASTEXITCODE -ne 0) { throw 'Baseline render failed' }
    if ((Get-FileHash -LiteralPath $current).Hash -ne (Get-FileHash -LiteralPath $tagged).Hash) {
        throw "PCM differs from $Baseline for unchanged settings"
    }
    Write-Host "PCM matches $Baseline on every fixture."
} finally {
    Set-Location $previousLocation
    $env:CARGO_TARGET_DIR = $previousTarget
    $resolved = (Resolve-Path -LiteralPath $work).Path
    $temp = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath())
    if ($resolved.StartsWith($temp, [StringComparison]::OrdinalIgnoreCase) -and
        (Split-Path -Leaf $resolved) -match '^HushPcm-[a-f0-9]{32}$') {
        Remove-Item -LiteralPath $resolved -Recurse
    }
}
