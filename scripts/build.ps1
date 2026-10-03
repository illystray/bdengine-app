param()

$ErrorActionPreference = 'Stop'

function New-IcoFromPng {
  param([string]$PngPath, [string]$IcoPath)

  $pngBytes = [System.IO.File]::ReadAllBytes($PngPath)
  $fs = [System.IO.File]::Open($IcoPath, [System.IO.FileMode]::Create, [System.IO.FileAccess]::Write)
  try {
    $writer = New-Object System.IO.BinaryWriter($fs)
    $writer.Write([UInt16]0)
    $writer.Write([UInt16]1)
    $writer.Write([UInt16]1)
    $writer.Write([byte]0)
    $writer.Write([byte]0)
    $writer.Write([byte]0)
    $writer.Write([byte]0)
    $writer.Write([UInt16]1)
    $writer.Write([UInt16]32)
    $writer.Write([UInt32]$pngBytes.Length)
    $writer.Write([UInt32]22)
    $writer.Write($pngBytes)
    $writer.Flush()
  }
  finally {
    $fs.Dispose()
  }
}

$projectRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$binaryName = 'bdengine_app.exe'
$binaryPath = Join-Path $projectRoot "src-tauri\target\release\$binaryName"
$fileIconPng = Join-Path $projectRoot 'src-tauri\icons\FileAssociationLogo.png'
$licensePath = Join-Path $projectRoot 'LICENSE'
$redistRoot = Join-Path $projectRoot 'redist'
$buildParent = Join-Path $projectRoot 'build'
$buildRoot = [System.IO.Path]::GetFullPath((Join-Path $buildParent 'app'))

if (-not (Test-Path -LiteralPath $fileIconPng -PathType Leaf)) {
  throw "File association icon source not found: $fileIconPng"
}
if (-not (Test-Path -LiteralPath $licensePath -PathType Leaf)) {
  throw "Application license not found: $licensePath"
}

& npm.cmd --prefix $projectRoot run build:bin
if ($LASTEXITCODE -ne 0) {
  throw 'Failed to build the Tauri binary.'
}
if (-not (Test-Path -LiteralPath $binaryPath -PathType Leaf)) {
  throw "Built executable not found: $binaryPath"
}

# Validate the exact output directory before clearing a previous build.
$workspacePrefix = $projectRoot.TrimEnd('\') + '\'
if (-not $buildRoot.StartsWith($workspacePrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
  throw "Refusing to operate outside workspace: $buildRoot"
}
foreach ($path in @($projectRoot, $buildParent, $buildRoot)) {
  if (Test-Path -LiteralPath $path) {
    $item = Get-Item -LiteralPath $path -Force
    if (-not $item.PSIsContainer -or ($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint)) {
      throw "Build directory must be a regular directory: $path"
    }
  }
}
if (Test-Path -LiteralPath $buildRoot) {
  $links = @(Get-ChildItem -LiteralPath $buildRoot -Recurse -Force -Attributes ReparsePoint)
  if ($links.Count -ne 0) {
    throw "Refusing to clear a build directory containing links: $buildRoot"
  }
  Remove-Item -LiteralPath $buildRoot -Recurse -Force
}
New-Item -ItemType Directory -Path $buildRoot | Out-Null
Copy-Item -LiteralPath $binaryPath -Destination (Join-Path $buildRoot $binaryName)
Copy-Item -LiteralPath $licensePath -Destination (Join-Path $buildRoot 'LICENSE')

$fileIconsRoot = Join-Path $buildRoot 'fileicons'
$fileIconIco = Join-Path $fileIconsRoot 'bdengine-file.ico'
New-Item -ItemType Directory -Path $fileIconsRoot | Out-Null
New-IcoFromPng -PngPath $fileIconPng -IcoPath $fileIconIco

$webViewInstaller = $null
foreach ($name in @('MicrosoftEdgeWebView2RuntimeInstallerX64.exe', 'MicrosoftEdgeWebview2Setup.exe')) {
  $candidate = Join-Path $redistRoot $name
  if (Test-Path -LiteralPath $candidate -PathType Leaf) {
    $webViewInstaller = $candidate
    break
  }
}
if ($webViewInstaller) {
  $buildRedistRoot = Join-Path $buildRoot 'redist'
  New-Item -ItemType Directory -Path $buildRedistRoot | Out-Null
  Copy-Item -LiteralPath $webViewInstaller -Destination $buildRedistRoot
}

Write-Host "Application build prepared: $buildRoot"
Write-Host "Included .bdengine file icon: $fileIconIco"
if ($webViewInstaller) {
  Write-Host "Included WebView2 installer: $([System.IO.Path]::GetFileName($webViewInstaller))"
} else {
  Write-Host 'WebView2 installer not found; build output does not include a redist installer.'
}
