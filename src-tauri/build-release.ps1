param(
  [switch] $SkipIconCacheClear
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$ProjectRoot = Resolve-Path (Join-Path $PSScriptRoot '..')

function Remove-BuildArtifact {
  param([string] $Path)

  if (-not (Test-Path -LiteralPath $Path)) {
    return
  }

  $resolved = Resolve-Path -LiteralPath $Path
  $projectRootPath = $ProjectRoot.Path.TrimEnd('\')
  $tauriRootPath = (Resolve-Path $PSScriptRoot).Path.TrimEnd('\')

  foreach ($item in $resolved) {
    $fullPath = $item.Path.TrimEnd('\')
    $isInsideProject = $fullPath.StartsWith($projectRootPath, [System.StringComparison]::OrdinalIgnoreCase)
    $isInsideTauri = $fullPath.StartsWith($tauriRootPath, [System.StringComparison]::OrdinalIgnoreCase)

    if (-not ($isInsideProject -or $isInsideTauri)) {
      throw "Refusing to delete build artifact outside the project: $fullPath"
    }

    Write-Host "Removing stale build artifact: $fullPath"
    Remove-Item -LiteralPath $fullPath -Recurse -Force
  }
}

Set-Location $PSScriptRoot
Remove-Item Env:\CARGO_TARGET_DIR -ErrorAction SilentlyContinue

& (Join-Path $PSScriptRoot 'scripts\generate-icons.ps1')

$WindowsSidecarDir = Join-Path $PSScriptRoot 'bin'
$WindowsSidecar = Join-Path $WindowsSidecarDir 'aria2c-x86_64-pc-windows-msvc.exe'
if (-not (Test-Path -LiteralPath $WindowsSidecar)) {
  throw "Missing Windows aria2 sidecar: $WindowsSidecar"
}

Remove-BuildArtifact (Join-Path $PSScriptRoot 'target')
Remove-BuildArtifact (Join-Path $ProjectRoot 'dist')
Remove-BuildArtifact (Join-Path $ProjectRoot 'release')
Remove-BuildArtifact (Join-Path $PSScriptRoot 'dist')
Remove-BuildArtifact (Join-Path $PSScriptRoot 'release')
Remove-BuildArtifact (Join-Path $PSScriptRoot '.tauri')

cargo tauri build @args

& (Join-Path $PSScriptRoot 'scripts\verify-windows-icon.ps1')

if (-not $SkipIconCacheClear) {
  & (Join-Path $PSScriptRoot 'scripts\clear-windows-icon-cache.ps1')
}
