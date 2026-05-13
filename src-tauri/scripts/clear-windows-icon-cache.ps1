$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$cacheRoots = @(
  $env:LOCALAPPDATA,
  (Join-Path $env:LOCALAPPDATA 'Microsoft\Windows\Explorer')
)

$patterns = @(
  'IconCache.db',
  'iconcache*.db',
  'thumbcache*.db'
)

Write-Host 'Stopping Explorer so Windows releases icon and thumbnail cache handles...'
Get-Process explorer -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2

foreach ($root in $cacheRoots) {
  if (-not (Test-Path -LiteralPath $root)) {
    continue
  }

  foreach ($pattern in $patterns) {
    Get-ChildItem -LiteralPath $root -Filter $pattern -Force -ErrorAction SilentlyContinue |
      Remove-Item -Force -ErrorAction SilentlyContinue
  }
}

$ie4uinit = Join-Path $env:WINDIR 'System32\ie4uinit.exe'
if (Test-Path -LiteralPath $ie4uinit) {
  & $ie4uinit -ClearIconCache
  & $ie4uinit -show
}

Start-Process explorer.exe
Write-Host 'Windows icon and thumbnail caches were cleared, and Explorer was restarted.'
