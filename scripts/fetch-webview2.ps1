param()

$ErrorActionPreference = 'Stop'

function Get-ProjectRoot {
  return (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
}

$projectRoot = Get-ProjectRoot
$redistRoot = Join-Path $projectRoot 'redist'
$targetPath = Join-Path $redistRoot 'MicrosoftEdgeWebview2Setup.exe'
$url = 'https://go.microsoft.com/fwlink/p/?LinkId=2124703'

New-Item -ItemType Directory -Force -Path $redistRoot | Out-Null
Invoke-WebRequest -Uri $url -OutFile $targetPath
Write-Host "Downloaded WebView2 installer to $targetPath"
