# Hop installer for Windows. Served by Hop on a Mac; run it in PowerShell with:
#   iwr http://<mac-ip>:47123/install.ps1 | iex
# Installs for the current user, adds a Start menu shortcut, starts Hop at sign-in and
# allows it through Windows Firewall (one admin prompt).

$ErrorActionPreference = 'Stop'
$base = '{{BASE}}'
$dir = Join-Path $env:LOCALAPPDATA 'Programs\Hop'
$exe = Join-Path $dir 'Hop.exe'

Write-Host ''
Write-Host 'Installing Hop...' -ForegroundColor Cyan
New-Item -ItemType Directory -Force -Path $dir | Out-Null
Get-Process -Name 'Hop' -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 300

Write-Host "  downloading from $base"
$ProgressPreference = 'SilentlyContinue'
Invoke-WebRequest -Uri "$base/Hop.exe" -OutFile $exe -UseBasicParsing
Unblock-File -Path $exe

$shell = New-Object -ComObject WScript.Shell
$lnk = $shell.CreateShortcut((Join-Path ([Environment]::GetFolderPath('Programs')) 'Hop.lnk'))
$lnk.TargetPath = $exe
$lnk.WorkingDirectory = $dir
$lnk.Description = 'Hop remote desktop'
$lnk.Save()
Write-Host '  added Start menu shortcut'

Set-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'Hop' -Value "`"$exe`" --minimized"
Write-Host '  Hop will start when you sign in'

# Inbound rule so the Mac can reach Hop (TCP 47121, UDP 47120/47122). Needs admin once.
$rule = "netsh advfirewall firewall delete rule name=Hop >nul 2>&1 & " +
        "netsh advfirewall firewall add rule name=Hop dir=in action=allow program=`"$exe`" enable=yes profile=any"
try {
    Start-Process -FilePath cmd.exe -ArgumentList "/c $rule" -Verb RunAs -Wait -WindowStyle Hidden
    Write-Host '  allowed Hop through Windows Firewall'
} catch {
    Write-Host '  skipped the firewall rule (Windows will ask when Hop first runs; click Allow)' -ForegroundColor Yellow
}

Start-Process -FilePath $exe
Write-Host ''
Write-Host 'Done. Hop is running.' -ForegroundColor Green
Write-Host 'To check this PC can capture and encode the screen, run:'
Write-Host "  & `"$exe`" selftest"
