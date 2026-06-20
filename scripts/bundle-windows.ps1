<#
.SYNOPSIS
  Build the Noema Atlas Windows installer (and a portable zip).

.DESCRIPTION
  Compiles the release binaries, then packages them into a one-click NSIS
  installer (Noema-Atlas-Setup.exe) and a portable zip. Requires NSIS
  (`makensis` on PATH) for the installer; the zip is always produced.

  Optional Authenticode signing: set $env:WIN_CERT_PFX (path to a .pfx) and
  $env:WIN_CERT_PASSWORD to sign the binaries and the installer with signtool.

.PARAMETER OutDir
  Output directory (default: dist).
#>
param(
  [string]$OutDir = "dist"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Set-Location $root

$version = (Select-String -Path "Cargo.toml" -Pattern '^version\s*=\s*"(.*)"' |
  Select-Object -First 1).Matches.Groups[1].Value
if (-not $version) { $version = "0.0.0" }

Write-Host "==> building release binaries (v$version)"
cargo build --release --workspace
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$rel = Join-Path $root "target\release"
$exes = @("noema-desktop.exe", "noema.exe", "noema-registry.exe")
foreach ($e in $exes) {
  if (-not (Test-Path (Join-Path $rel $e))) { throw "missing build output: $e" }
}

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDirFull = (Resolve-Path $OutDir).Path

# Optional Authenticode signing of the binaries before packaging.
function Sign-File($path) {
  if ($env:WIN_CERT_PFX -and (Test-Path $env:WIN_CERT_PFX)) {
    Write-Host "==> signing $path"
    signtool sign /f $env:WIN_CERT_PFX /p $env:WIN_CERT_PASSWORD `
      /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 $path
    if ($LASTEXITCODE -ne 0) { throw "signtool failed for $path" }
  }
}
foreach ($e in $exes) { Sign-File (Join-Path $rel $e) }

# ---- Portable zip (always) -------------------------------------------------
$zip = Join-Path $OutDirFull "Noema-Atlas-windows-x86_64.zip"
if (Test-Path $zip) { Remove-Item $zip }
Compress-Archive -Path ($exes | ForEach-Object { Join-Path $rel $_ }) -DestinationPath $zip
Write-Host "==> portable zip ready: $zip"

# ---- NSIS installer --------------------------------------------------------
$makensis = Get-Command makensis -ErrorAction SilentlyContinue
if (-not $makensis) {
  Write-Warning "makensis not found on PATH — skipping installer (portable zip still produced)."
  Write-Warning "Install NSIS (https://nsis.sourceforge.io) to build Noema-Atlas-Setup.exe."
  exit 0
}

$nsi = Join-Path $root "scripts\windows\installer.nsi"
$setup = Join-Path $OutDirFull "Noema-Atlas-Setup.exe"
$license = Join-Path $root "LICENSE"
$logo = Join-Path $root "assets\logo.png"

$args = @(
  "/DVERSION=$version",
  "/DSRCDIR=$rel",
  "/DOUTFILE=$setup",
  "/DLICENSE_FILE=$license"
)
if (Test-Path $logo) { $args += "/DLOGO=$logo" }
$args += $nsi

Write-Host "==> building installer with makensis"
& $makensis.Source @args
if ($LASTEXITCODE -ne 0) { throw "makensis failed" }

Sign-File $setup
Write-Host "==> installer ready: $setup"
