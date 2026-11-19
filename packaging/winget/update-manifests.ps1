# Fills the winget manifest templates for a release and writes them to
# packaging\winget\manifests\e\Exel\Hush\<version>\ (the winget-pkgs layout).
# Nothing is downloaded or submitted.
#
#   powershell -ExecutionPolicy Bypass -File .\packaging\winget\update-manifests.ps1 -Version 1.0.0
#
# The hash is read from dist\Hush-Setup-<version>.sha256 (written by
# package.ps1 and the release workflow), or computed from the exe.
param(
    [Parameter(Mandatory)][string]$Version,
    [string]$Dist = ''
)
$ErrorActionPreference = 'Stop'
if (-not $Dist) { $Dist = Join-Path $PSScriptRoot '..\..\dist' }
$Version = $Version.TrimStart('v')
$exe = Join-Path $Dist "Hush-Setup-$Version.exe"
$sha = Join-Path $Dist "Hush-Setup-$Version.sha256"

if (Test-Path $sha) {
    $hash = ((Get-Content $sha -TotalCount 1) -split '\s+')[0]
} elseif (Test-Path $exe) {
    $hash = (Get-FileHash $exe -Algorithm SHA256).Hash
} else {
    throw "Neither $sha nor $exe exists."
}
if ($hash -notmatch '^[0-9a-fA-F]{64}$') { throw "Not a SHA-256 hash: $hash" }
$hash = $hash.ToUpper()   # winget-pkgs convention

$date = if (Test-Path $exe) { (Get-Item $exe).LastWriteTimeUtc } else { [DateTime]::UtcNow }
$date = $date.ToString('yyyy-MM-dd')

$out = Join-Path $PSScriptRoot "manifests\e\Exel\Hush\$Version"
New-Item -ItemType Directory -Force $out | Out-Null
$utf8 = New-Object System.Text.UTF8Encoding($false)
foreach ($template in Get-ChildItem (Join-Path $PSScriptRoot 'templates') -Filter *.yaml) {
    $text = [System.IO.File]::ReadAllText($template.FullName)
    $text = $text.Replace('{{VERSION}}', $Version).Replace('{{SHA256}}', $hash).Replace('{{DATE}}', $date)
    [System.IO.File]::WriteAllText((Join-Path $out $template.Name), $text.Replace("`r`n", "`n"), $utf8)
}
Write-Host "Wrote manifests for Hush $Version to $out"
