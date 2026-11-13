#Requires -Version 7
# Assembles the canopy renderer as a self-contained "mechanism B" sidecar payload: a pinned Node
# runtime + the @iyulab/canopy release pinned in canopy-sidecar/, installed with its production
# dependencies. The payload is bundled by Tauri as a resource (see tauri.conf.json) and invoked as
# `node node_modules/@iyulab/canopy/dist/cli.js` at runtime (see canopy_from_resource_dir).
# Idempotent: safe to re-run. The Node runtime is the one for this machine's platform (win-x64 ·
# darwin-arm64 · linux-x64). CI and the release build run exactly this.
[CmdletBinding()]
param(
  # A canopy source checkout to build and install instead of the pinned release — for trying a
  # renderer change before it is released. The result is reported as not the shipped renderer.
  [string]$CanopyPath = '',
  [string]$NodeVersion = '22.12.0'
)
$ErrorActionPreference = 'Stop'

$repoRoot  = Join-Path $PSScriptRoot '..'                       # textree/
$manifest  = Join-Path $repoRoot 'canopy-sidecar'
$stage     = Join-Path $repoRoot 'src-tauri' 'resources' 'canopy'
$cacheDir  = Join-Path $repoRoot '.cache'

# 1. The pins must be exact, and the editor and the renderer must draw math with the same KaTeX.
& node (Join-Path $PSScriptRoot 'canopy-stage.mjs')
if ($LASTEXITCODE -ne 0) { throw "canopy pins are not usable ($LASTEXITCODE)" }

# 2. Reset the stage dir and put the manifest in it.
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force -Path $stage | Out-Null
Copy-Item (Join-Path $manifest 'package.json') $stage -Force
Copy-Item (Join-Path $manifest 'package-lock.json') $stage -Force

# 3. Production-only install into the stage: the pinned release as the lock file records it, or a
#    package built from the given checkout in its place.
$checkout = $null
if ($CanopyPath) {
  $checkout = (Resolve-Path $CanopyPath).Path
  Write-Host "Assembling canopy sidecar from the checkout $checkout (not the shipped release)"
  New-Item -ItemType Directory -Force -Path $cacheDir | Out-Null
  Push-Location $checkout
  try {
    npm ci
    if ($LASTEXITCODE -ne 0) { throw "npm ci failed in $checkout" }
    npm run build   # `npm pack` does not run prepublishOnly
    if ($LASTEXITCODE -ne 0) { throw "npm run build failed in $checkout" }
    $packed = (npm pack --pack-destination $cacheDir --silent) | Select-Object -Last 1
    if ($LASTEXITCODE -ne 0) { throw "npm pack failed in $checkout" }
  } finally { Pop-Location }
  Push-Location $stage
  try {
    npm install --omit=dev --no-audit --no-fund (Join-Path $cacheDir $packed)
    if ($LASTEXITCODE -ne 0) { throw "installing the packed canopy failed" }
  } finally { Pop-Location }
} else {
  Write-Host "Assembling canopy sidecar: node v$NodeVersion + the pinned canopy release -> $stage"
  Push-Location $stage
  try {
    npm ci --omit=dev --no-audit --no-fund
    if ($LASTEXITCODE -ne 0) { throw "npm ci failed in the stage" }
  } finally { Pop-Location }
}

# 4. Fetch + cache the pinned Node runtime for this platform, put its executable into the stage
#    (`node.exe` on Windows, `node` elsewhere — the name the app looks for).
if ($IsWindows) { $nodePlatform = 'win-x64'; $archiveExt = 'zip'; $inArchive = 'node.exe' }
elseif ($IsMacOS) { $nodePlatform = 'darwin-arm64'; $archiveExt = 'tar.gz'; $inArchive = 'bin/node' }
else { $nodePlatform = 'linux-x64'; $archiveExt = 'tar.gz'; $inArchive = 'bin/node' }
$nodeExe = Join-Path $stage (Split-Path -Leaf $inArchive)
if (-not (Test-Path $nodeExe)) {
  New-Item -ItemType Directory -Force -Path $cacheDir | Out-Null
  $archiveName = "node-v$NodeVersion-$nodePlatform"
  $archive     = Join-Path $cacheDir "$archiveName.$archiveExt"
  if (-not (Test-Path $archive)) {
    Invoke-WebRequest "https://nodejs.org/dist/v$NodeVersion/$archiveName.$archiveExt" -OutFile $archive
  }
  $extract = Join-Path $cacheDir $archiveName
  if (-not (Test-Path $extract)) {
    if ($archiveExt -eq 'zip') { Expand-Archive $archive -DestinationPath $cacheDir -Force }
    else {
      tar -xzf $archive -C $cacheDir
      if ($LASTEXITCODE -ne 0) { throw "extracting $archive failed ($LASTEXITCODE)" }
    }
  }
  Copy-Item (Join-Path $extract $inArchive) $nodeExe -Force
}

# 5. Record which renderer this payload is (sidecar-provenance.mjs; the E2E run reports it).
$stampArgs = @('stamp', 'canopy')
if ($checkout) { $stampArgs += $checkout }
& node (Join-Path $PSScriptRoot 'sidecar-provenance.mjs') @stampArgs
if ($LASTEXITCODE -ne 0) { throw "recording the renderer's source failed ($LASTEXITCODE)" }

Write-Host "Done. Payload at $stage"
