# Gets Hop from the Mac that's running it and opens it. In the Hop window that appears, click
# "Install Hop" (it installs, or updates an existing copy).
#   iwr http://<mac-ip>:47123/install.ps1 | iex

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$exe = Join-Path ([Environment]::GetFolderPath('UserProfile')) 'Downloads\Hop.exe'
Write-Host 'Downloading Hop from {{BASE}} ...'
Invoke-WebRequest -Uri '{{BASE}}/Hop.exe' -OutFile $exe -UseBasicParsing
Start-Process -FilePath $exe
Write-Host 'Hop is open. Click "Install Hop" in its window.'
