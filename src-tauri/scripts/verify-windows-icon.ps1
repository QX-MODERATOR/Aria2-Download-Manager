param(
  [string] $ExePath = (Join-Path (Resolve-Path (Join-Path $PSScriptRoot '..')) 'target\release\aria2-manager.exe'),
  [string] $IconPath = (Join-Path (Resolve-Path (Join-Path $PSScriptRoot '..')) 'icons\icon.ico')
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$RequiredSizes = @(16, 24, 32, 48, 64, 128, 256)

function Assert-IcoSizes {
  param([string] $Path)

  if (-not (Test-Path -LiteralPath $Path)) {
    throw "Icon file not found: $Path"
  }

  $sizes = & magick identify -format "%w`n" $Path
  if ($LASTEXITCODE -ne 0) {
    throw "ImageMagick could not inspect icon: $Path"
  }

  $actualSizes = $sizes -split "`n" | Where-Object { $_ } | ForEach-Object { [int]$_ }
  $missing = @($RequiredSizes | Where-Object { $_ -notin $actualSizes })
  if ($missing.Count -gt 0) {
    throw "$Path is missing required ICO sizes: $($missing -join ', ')"
  }

  Write-Host "$Path contains ICO sizes: $($actualSizes -join ', ')"
}

function Get-EmbeddedIconSizes {
  param([string] $Path)

  if (-not (Test-Path -LiteralPath $Path)) {
    throw "EXE not found: $Path"
  }

  $bytes = [System.IO.File]::ReadAllBytes($Path)

  function U16([int] $offset) {
    return [System.BitConverter]::ToUInt16($bytes, $offset)
  }

  function U32([int] $offset) {
    return [System.BitConverter]::ToUInt32($bytes, $offset)
  }

  function Ascii([int] $offset, [int] $length) {
    return [System.Text.Encoding]::ASCII.GetString($bytes, $offset, $length)
  }

  $peOffset = U32 0x3c
  if ((Ascii $peOffset 4) -ne "PE`0`0") {
    throw "$Path is not a valid PE executable."
  }

  $coffOffset = $peOffset + 4
  $sectionCount = U16 ($coffOffset + 2)
  $optionalHeaderSize = U16 ($coffOffset + 16)
  $optionalHeaderOffset = $coffOffset + 20
  $magic = U16 $optionalHeaderOffset
  $dataDirectoryOffset = if ($magic -eq 0x20b) { $optionalHeaderOffset + 112 } elseif ($magic -eq 0x10b) { $optionalHeaderOffset + 96 } else { throw 'Unsupported PE optional header.' }
  $resourceRva = U32 ($dataDirectoryOffset + (2 * 8))
  if ($resourceRva -eq 0) {
    throw "$Path does not contain a resource table."
  }

  $sections = @()
  $sectionOffset = $optionalHeaderOffset + $optionalHeaderSize
  for ($i = 0; $i -lt $sectionCount; $i++) {
    $offset = $sectionOffset + ($i * 40)
    $sections += [pscustomobject]@{
      Name = (Ascii $offset 8).Trim([char]0)
      VirtualSize = U32 ($offset + 8)
      VirtualAddress = U32 ($offset + 12)
      RawSize = U32 ($offset + 16)
      RawPointer = U32 ($offset + 20)
    }
  }

  function Rva-ToOffset([uint32] $rva) {
    foreach ($section in $sections) {
      $size = [Math]::Max($section.VirtualSize, $section.RawSize)
      if ($rva -ge $section.VirtualAddress -and $rva -lt ($section.VirtualAddress + $size)) {
        return [int]($section.RawPointer + ($rva - $section.VirtualAddress))
      }
    }

    throw "Could not map RVA $rva to a file offset."
  }

  $resourceOffset = Rva-ToOffset $resourceRva

  function Get-ResourceDirectoryEntries([int] $relativeOffset) {
    $directoryOffset = $resourceOffset + $relativeOffset
    $namedCount = U16 ($directoryOffset + 12)
    $idCount = U16 ($directoryOffset + 14)
    $entryCount = $namedCount + $idCount
    $entries = @()

    for ($i = 0; $i -lt $entryCount; $i++) {
      $entryOffset = $directoryOffset + 16 + ($i * 8)
      $nameOrId = U32 $entryOffset
      $dataOrDirectory = U32 ($entryOffset + 4)
      $entries += [pscustomobject]@{
        Id = $nameOrId -band 0xffff
        IsDirectory = (($dataOrDirectory -band 0x80000000) -ne 0)
        Offset = [int]($dataOrDirectory -band 0x7fffffff)
      }
    }

    return $entries
  }

  function Get-FirstResourceData([int] $directoryOffset) {
    $nameEntry = (Get-ResourceDirectoryEntries $directoryOffset | Select-Object -First 1)
    if ($null -eq $nameEntry -or -not $nameEntry.IsDirectory) {
      throw 'Malformed icon group resource: missing name directory.'
    }

    $langEntry = (Get-ResourceDirectoryEntries $nameEntry.Offset | Select-Object -First 1)
    if ($null -eq $langEntry -or $langEntry.IsDirectory) {
      throw 'Malformed icon group resource: missing language data.'
    }

    $dataEntryOffset = $resourceOffset + $langEntry.Offset
    $dataRva = U32 $dataEntryOffset
    $dataSize = U32 ($dataEntryOffset + 4)
    $dataOffset = Rva-ToOffset $dataRva
    return ,@($dataOffset, $dataSize)
  }

  $typeEntries = Get-ResourceDirectoryEntries 0
  $groupIconEntry = $typeEntries | Where-Object { $_.Id -eq 14 } | Select-Object -First 1
  if ($null -eq $groupIconEntry -or -not $groupIconEntry.IsDirectory) {
    throw "$Path does not contain a RT_GROUP_ICON resource."
  }

  $data = Get-FirstResourceData $groupIconEntry.Offset
  $groupOffset = $data[0]
  $count = U16 ($groupOffset + 4)
  $sizes = @()

  for ($i = 0; $i -lt $count; $i++) {
    $entryOffset = $groupOffset + 6 + ($i * 14)
    $width = $bytes[$entryOffset]
    if ($width -eq 0) {
      $width = 256
    }

    $sizes += [int]$width
  }

  return $sizes | Sort-Object -Unique
}

Assert-IcoSizes -Path $IconPath

$embeddedSizes = Get-EmbeddedIconSizes -Path $ExePath
$missingEmbedded = @($RequiredSizes | Where-Object { $_ -notin $embeddedSizes })
if ($missingEmbedded.Count -gt 0) {
  throw "$ExePath is missing embedded Windows icon sizes: $($missingEmbedded -join ', ')"
}

Write-Host "$ExePath contains embedded Windows icon sizes: $($embeddedSizes -join ', ')"
Write-Host 'Windows icon verification passed.'
