$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$TauriDir = Resolve-Path (Join-Path $PSScriptRoot '..')
$IconsDir = Join-Path $TauriDir 'icons'
$MasterSvg = Join-Path $TauriDir 'icon-master.svg'
$OutputIco = Join-Path $IconsDir 'icon.ico'
$OutputIcns = Join-Path $IconsDir 'icon.icns'
$RequiredSizes = @(16, 24, 32, 48, 64, 128, 256)
$TauriPngOutputs = @(
  '16x16.png',
  '24x24.png',
  '32x32.png',
  '48x48.png',
  '64x64.png',
  '128x128.png',
  '128x128@2x.png'
)
$LegacyOutputs = @(
  'icon.png',
  '256x256.png',
  'preview.png'
)
$MasterSize = 1024

if (-not (Get-Command magick -ErrorAction SilentlyContinue)) {
  throw 'ImageMagick `magick` was not found on PATH. Install ImageMagick or add it to PATH before building.'
}

New-Item -ItemType Directory -Path $IconsDir -Force | Out-Null

$outputsToDelete = @('icon.ico', 'icon.icns') + $TauriPngOutputs + $LegacyOutputs
foreach ($fileName in $outputsToDelete) {
  $path = Join-Path $IconsDir $fileName
  if (Test-Path -LiteralPath $path) {
    Remove-Item -LiteralPath $path -Force
  }
}

$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) 'aria2-icon-build'
if (Test-Path -LiteralPath $tempDir) {
  Remove-Item -LiteralPath $tempDir -Recurse -Force
}
New-Item -ItemType Directory -Path $tempDir | Out-Null

function New-Aria2IconSvg {
  param(
    [int] $Size,
    [bool] $IncludeTwo
  )

  $isTiny = $Size -le 24
  $isSmall = $Size -le 32
  $topArc = ''
  $swooshShadow = ''
  $swoosh = ''
  $bottomCurve = ''
  $smallCrossbar = ''

  if ($isTiny) {
    $smallCrossbar = '<path d="M382 592 L646 592" fill="none" stroke="#20e8f2" stroke-width="74" stroke-linecap="round"/>'
  } elseif ($isSmall) {
    $swooshShadow = '<path d="M248 692 C408 590 590 526 802 504" fill="none" stroke="#020814" stroke-width="88" stroke-linecap="round" opacity="0.62"/>'
    $swoosh = '<path d="M246 676 C408 574 592 512 804 492" fill="none" stroke="url(#swooshFill)" stroke-width="68" stroke-linecap="round"/>'
  } else {
    $topArc = '<path d="M250 332 C330 182 642 132 802 250 C904 325 947 458 910 576" fill="none" stroke="#18e7f1" stroke-width="38" stroke-linecap="round" opacity="0.95"/>'
    $swooshShadow = '<path d="M238 700 C408 590 590 526 836 500 C632 552 464 650 330 805" fill="none" stroke="#020814" stroke-width="96" stroke-linecap="round" opacity="0.62"/>'
    $swoosh = '<path d="M236 684 C407 576 594 510 832 488 C623 538 452 640 316 786" fill="none" stroke="url(#swooshFill)" stroke-width="74" stroke-linecap="round"/>'
    $bottomCurve = '<path d="M420 830 C514 872 640 876 748 842" fill="none" stroke="#187dff" stroke-width="34" stroke-linecap="round"/>'
  }

  $two = ''
  $numberShadow = ''
  if ($IncludeTwo) {
    $numberShadow = '<text x="704" y="810" font-family="Arial Black, Arial, Segoe UI, sans-serif" font-size="332" font-weight="900" fill="#020916" opacity="0.55">2</text>'
    $two = '<text x="690" y="792" font-family="Arial Black, Arial, Segoe UI, sans-serif" font-size="332" font-weight="900" fill="url(#numberFill)">2</text>'
  }

  return @"
<svg xmlns="http://www.w3.org/2000/svg" width="$Size" height="$Size" viewBox="0 0 1024 1024">
  <defs>
    <linearGradient id="bg" x1="124" y1="96" x2="900" y2="930" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="#021735"/>
      <stop offset="0.52" stop-color="#031028"/>
      <stop offset="1" stop-color="#020713"/>
    </linearGradient>
    <linearGradient id="markFill" x1="258" y1="812" x2="686" y2="168" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="#2266ff"/>
      <stop offset="0.44" stop-color="#00aaff"/>
      <stop offset="1" stop-color="#23f7ee"/>
    </linearGradient>
    <linearGradient id="swooshFill" x1="222" y1="718" x2="848" y2="494" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="#7657ff"/>
      <stop offset="0.34" stop-color="#1179ff"/>
      <stop offset="1" stop-color="#20f4ed"/>
    </linearGradient>
    <linearGradient id="numberFill" x1="720" y1="506" x2="890" y2="812" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="#2af8ee"/>
      <stop offset="1" stop-color="#1388ff"/>
    </linearGradient>
  </defs>

  <rect x="64" y="64" width="896" height="896" rx="214" fill="url(#bg)"/>

  $topArc
  <path d="M404 770 L516 210 C524 170 584 169 604 207 L772 600"
        fill="none" stroke="#021026" stroke-width="166" stroke-linecap="round" stroke-linejoin="round" opacity="0.42"/>
  <path d="M360 780 L514 236 C528 188 588 188 610 235 L790 640"
        fill="none" stroke="url(#markFill)" stroke-width="132" stroke-linecap="round" stroke-linejoin="round"/>
  $smallCrossbar
  $swooshShadow
  $swoosh
  $bottomCurve
  $numberShadow
  $two
</svg>
"@
}

function Invoke-Magick {
  param([string[]] $Arguments)

  & magick @Arguments
  if ($LASTEXITCODE -ne 0) {
    throw "ImageMagick failed: magick $($Arguments -join ' ')"
  }
}

function Write-BigEndianUInt32 {
  param(
    [System.IO.BinaryWriter] $Writer,
    [uint32] $Value
  )

  $Writer.Write([byte](($Value -shr 24) -band 0xff))
  $Writer.Write([byte](($Value -shr 16) -band 0xff))
  $Writer.Write([byte](($Value -shr 8) -band 0xff))
  $Writer.Write([byte]($Value -band 0xff))
}

function Write-IcnsFile {
  param(
    [string] $Path,
    [hashtable] $Entries
  )

  $entryBytes = @()
  foreach ($entry in $Entries.GetEnumerator()) {
    $data = [System.IO.File]::ReadAllBytes($entry.Value)
    $entryBytes += [pscustomobject]@{
      Type = $entry.Key
      Data = $data
      Size = [uint32](8 + $data.Length)
    }
  }

  $totalSize = [uint32](8 + (($entryBytes | Measure-Object -Property Size -Sum).Sum))
  $fs = [System.IO.File]::Open($Path, [System.IO.FileMode]::Create)
  $writer = $null
  try {
    $writer = [System.IO.BinaryWriter]::new($fs)
    $writer.Write([System.Text.Encoding]::ASCII.GetBytes('icns'))
    Write-BigEndianUInt32 -Writer $writer -Value $totalSize

    foreach ($entry in $entryBytes) {
      $writer.Write([System.Text.Encoding]::ASCII.GetBytes($entry.Type))
      Write-BigEndianUInt32 -Writer $writer -Value $entry.Size
      $writer.Write($entry.Data)
    }
  }
  finally {
    if ($null -ne $writer) {
      $writer.Dispose()
    } else {
      $fs.Dispose()
    }
  }
}

function Get-ImageInfo {
  param([string] $Path)

  $info = & magick identify -format '%w %h %[channels]' $Path
  if ($LASTEXITCODE -ne 0) {
    throw "Could not inspect image: $Path"
  }

  $parts = $info -split '\s+'
  return [pscustomobject]@{
    Width = [int]$parts[0]
    Height = [int]$parts[1]
    Channels = $parts[2]
  }
}

function Get-EdgeScore {
  param([string] $Path)

  $score = & magick $Path -background white -alpha remove -colorspace Gray -format '%[fx:standard_deviation]' info:
  if ($LASTEXITCODE -ne 0) {
    throw "Could not calculate sharpness score for: $Path"
  }

  return [double]::Parse($score, [System.Globalization.CultureInfo]::InvariantCulture)
}

try {
  if ($MasterSize -lt 512) {
    Write-Warning "The generated icon master is only $MasterSize x $MasterSize. Use at least 512 x 512 for Windows icons."
  }

  if (-not (Test-Path -LiteralPath $MasterSvg)) {
    throw "Missing SVG master icon: $MasterSvg"
  }

  $tempMasterSvg = Join-Path $tempDir 'master-1024.svg'
  Copy-Item -LiteralPath $MasterSvg -Destination $tempMasterSvg -Force
  $masterPng = Join-Path $tempDir 'master-1024.png'
  Invoke-Magick -Arguments @('-background', 'none', $tempMasterSvg, '-depth', '8', '-define', 'png:color-type=6', $masterPng)

  $masterInfo = Get-ImageInfo -Path $masterPng
  if ($masterInfo.Width -ne $MasterSize -or $masterInfo.Height -ne $MasterSize) {
    throw "Master icon render must be $MasterSize x $MasterSize. Actual: $($masterInfo.Width) x $($masterInfo.Height)"
  }

  $icoInputs = @()

  foreach ($size in $RequiredSizes) {
    if ($size -gt $MasterSize) {
      throw "Refusing to upscale icon size $size from master size $MasterSize."
    }

    $includeTwo = $size -ge 48
    $svgPath = Join-Path $tempDir "icon-$size.svg"
    $tmpPng = Join-Path $tempDir "icon-$size.png"

    Set-Content -LiteralPath $svgPath -Value (New-Aria2IconSvg -Size $size -IncludeTwo $includeTwo) -Encoding UTF8
    Invoke-Magick -Arguments @('-background', 'none', $svgPath, '-depth', '8', '-define', 'png:color-type=6', $tmpPng)

    $info = Get-ImageInfo -Path $tmpPng
    if ($info.Width -ne $size -or $info.Height -ne $size) {
      throw "Generated $size px icon has wrong dimensions: $($info.Width) x $($info.Height)"
    }

    if ($info.Channels -notmatch 'a') {
      throw "Generated $size px icon does not contain an alpha channel."
    }

    $edgeScore = Get-EdgeScore -Path $tmpPng
    if ($edgeScore -lt 0.05) {
      throw "Generated $size px icon failed the contrast/sharpness sanity check. Score: $edgeScore"
    }

    if ($size -lt 256) {
      $pngName = "$($size)x$($size).png"
      Copy-Item -LiteralPath $tmpPng -Destination (Join-Path $IconsDir $pngName) -Force
    } else {
      Copy-Item -LiteralPath $tmpPng -Destination (Join-Path $IconsDir '128x128@2x.png') -Force
    }

    $icoInputs += $tmpPng
  }

  Invoke-Magick -Arguments (@($icoInputs) + @($OutputIco))

  $icns512 = Join-Path $tempDir 'icon-512.png'
  $icns1024 = Join-Path $tempDir 'icon-1024.png'
  Invoke-Magick -Arguments @('-background', 'none', $MasterSvg, '-resize', '512x512', '-depth', '8', '-define', 'png:color-type=6', $icns512)
  Invoke-Magick -Arguments @('-background', 'none', $MasterSvg, '-resize', '1024x1024', '-depth', '8', '-define', 'png:color-type=6', $icns1024)
  Write-IcnsFile -Path $OutputIcns -Entries @{
    'icp4' = Join-Path $tempDir 'icon-16.png'
    'icp5' = Join-Path $tempDir 'icon-32.png'
    'icp6' = Join-Path $tempDir 'icon-64.png'
    'ic07' = Join-Path $tempDir 'icon-128.png'
    'ic08' = Join-Path $tempDir 'icon-256.png'
    'ic09' = $icns512
    'ic10' = $icns1024
  }

  $actual = & magick identify -format "%w`n" $OutputIco
  if ($LASTEXITCODE -ne 0) {
    throw "Could not inspect generated ICO: $OutputIco"
  }

  $actualSizes = $actual -split "`n" | Where-Object { $_ } | ForEach-Object { [int]$_ }
  $missing = @($RequiredSizes | Where-Object { $_ -notin $actualSizes })
  if ($missing.Count -gt 0) {
    throw "Generated ICO is missing required sizes: $($missing -join ', ')"
  }

  Write-Host "Generated crisp Windows ICO with embedded sizes: $($actualSizes -join ', ')"
  Write-Host "Generated macOS ICNS: $OutputIcns"
  Write-Host 'Small sizes 16/24/32 use the symbol-only mark. Larger sizes use the A2 mark.'
}
finally {
  Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
}
