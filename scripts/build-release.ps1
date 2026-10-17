<#
.SYNOPSIS
  Builds signed installers and the auto-update manifest into release\vX.Y.Z\.

.DESCRIPTION
  1. Builds the NSIS and MSI installers with `cargo tauri build`.
  2. Signs them for the auto-updater with the private key (never commit it).
  3. Writes latest.json (read by installed copies of Sagasu) and SHA256SUMS.txt.
  Upload everything in release\vX.Y.Z\ to the GitHub release; latest.json must be attached
  to the release marked "Latest" for auto-update to find it.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1
#>
param(
  # Updater private key made by `cargo tauri signer generate`. Keep a backup: without it, installed copies can no longer be updated.
  [string]$KeyPath = "$env:USERPROFILE\.tauri\sagasu-updater.key",
  # Build output outside OneDrive so gigabytes of build files are not synced.
  [string]$TargetDir = "$env:USERPROFILE\.cargo-target\sagasu",
  # Short text shown in the in-app update dialog.
  [string]$Notes = ""
)
$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
$conf = Get-Content "$root\src-tauri\tauri.conf.json" -Raw -Encoding UTF8 | ConvertFrom-Json
$version = $conf.version
$tag = "v$version"
if (-not (Test-Path $KeyPath)) { throw "Updater key not found: $KeyPath" }

$env:CARGO_TARGET_DIR = $TargetDir
$env:TAURI_SIGNING_PRIVATE_KEY = (Get-Content $KeyPath -Raw).Trim()
$env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ""

Write-Host "Building Sagasu $version ..."
Push-Location "$root\src-tauri"
try {
  cargo tauri build
  if ($LASTEXITCODE -ne 0) { throw "cargo tauri build failed" }
} finally {
  Pop-Location
  Remove-Item Env:TAURI_SIGNING_PRIVATE_KEY, Env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD -ErrorAction SilentlyContinue
}

$bundle = "$TargetDir\release\bundle"
$setup = "Sagasu_${version}_x64-setup.exe"
$msi = "Sagasu_${version}_x64_ja-JP.msi"
$out = "$root\release\$tag"
New-Item -ItemType Directory -Force $out | Out-Null
Copy-Item "$bundle\nsis\$setup", "$bundle\nsis\$setup.sig", "$bundle\msi\$msi" $out -Force

if (-not $Notes) { $Notes = "Sagasu $tag" }
$manifest = [ordered]@{
  version   = $version
  notes     = $Notes
  pub_date  = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
  platforms = [ordered]@{
    "windows-x86_64" = [ordered]@{
      signature = (Get-Content "$out\$setup.sig" -Raw).Trim()
      url       = "https://github.com/Nishi-Yura/sagasu/releases/download/$tag/$setup"
    }
  }
}
[IO.File]::WriteAllText("$out\latest.json", ($manifest | ConvertTo-Json -Depth 5), [Text.UTF8Encoding]::new($false))

Get-ChildItem $out -Filter "Sagasu_*" | Where-Object { $_.Extension -ne ".sig" } | ForEach-Object {
  "{0}  {1}" -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower(), $_.Name
} | Set-Content -Encoding ascii "$out\SHA256SUMS.txt"

Write-Host ""
Write-Host "Done: $out"
Get-ChildItem $out | Format-Table Name, Length -AutoSize
Write-Host "Publish with:"
Write-Host "  gh release create $tag release/$tag/$setup release/$tag/$msi release/$tag/latest.json release/$tag/SHA256SUMS.txt --title `"Sagasu $tag`" --notes-file release/$tag/release-notes.md"
