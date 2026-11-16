# Full-access mode for the Remote Desktop extension: TightVNC Server as a
# Windows service, so the desktop tab can also show UAC prompts, the lock
# screen and the sign-in screen.
#
# - Accepts connections ONLY from this machine (the extension relays traffic
#   over the VS Code connection; nothing is reachable from the network).
# - Uses a random password that is handed to the extension, never typed.
# - Picks port 5900, or a free port from 5917 if 5900 belongs to other software.
# - Records what it did in %ProgramData%\vscode-remote-desktop\service.json so
#   the extension can find the port, and so removal only uninstalls TightVNC
#   if this script installed it.
#
# Usage (normally launched by the extension):
#   setup-windows.ps1 -HandoffDir <dir> -ForUser <DOMAIN\user> [-Uninstall]

param(
    [string]$HandoffDir,
    [string]$ForUser,
    [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'
$RegPath    = 'HKLM:\SOFTWARE\TightVNC\Server'
$MarkerDir  = Join-Path $env:ProgramData 'vscode-remote-desktop'
$MarkerFile = Join-Path $MarkerDir 'service.json'

function Test-Admin {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    ([Security.Principal.WindowsPrincipal]$id).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Finish($code) {
    Write-Host ''
    Read-Host 'Press Enter to close'
    exit $code
}

# ---- elevation ---------------------------------------------------------------
# An SSH session as an administrator is already elevated, so no prompt appears.
if (-not (Test-Admin)) {
    Write-Host 'Administrator rights are needed. Requesting elevation (a UAC prompt appears on this machine''s screen)...'
    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"")
    if ($HandoffDir) { $argList += @('-HandoffDir', "`"$HandoffDir`"") }
    if ($ForUser)    { $argList += @('-ForUser', "`"$ForUser`"") }
    if ($Uninstall)  { $argList += '-Uninstall' }
    try {
        $p = Start-Process powershell.exe -Verb RunAs -Wait -PassThru -ArgumentList $argList
        Write-Host "Elevated step finished with exit code $($p.ExitCode)."
        exit $p.ExitCode
    } catch {
        Write-Host ''
        Write-Host 'Elevation was cancelled or is not possible from this session.' -ForegroundColor Yellow
        Write-Host 'Run this from an administrator PowerShell instead:'
        Write-Host "  powershell -ExecutionPolicy Bypass $($argList[3..($argList.Count-1)] -join ' ')"
        Finish 1
    }
}

# ---- helpers -------------------------------------------------------------------

function Get-Marker {
    if (Test-Path $MarkerFile) { Get-Content $MarkerFile -Raw | ConvertFrom-Json } else { $null }
}

function Set-Marker([string]$installedBy, [int]$port) {
    New-Item -ItemType Directory -Force $MarkerDir | Out-Null
    @{ installedBy = $installedBy; port = $port } | ConvertTo-Json | Set-Content $MarkerFile -Encoding ASCII
}

function New-RandomPassword {
    $chars = 'abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789'
    $bytes = New-Object byte[] 8
    [Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
    -join ($bytes | ForEach-Object { $chars[$_ % $chars.Length] })
}

# VNC stores passwords DES-encrypted with a fixed, well-known key (bits of each key byte reversed).
function Protect-VncPassword([string]$pw) {
    $key = foreach ($b in [byte[]](23, 82, 107, 6, 35, 78, 88, 7)) {
        $r = 0; for ($i = 0; $i -lt 8; $i++) { if ($b -band (1 -shl $i)) { $r = $r -bor (1 -shl (7 - $i)) } }; [byte]$r
    }
    $des = [Security.Cryptography.DES]::Create(); $des.Mode = 'ECB'; $des.Padding = 'None'; $des.Key = [byte[]]$key
    $plain = New-Object byte[] 8; [Text.Encoding]::ASCII.GetBytes($pw).CopyTo($plain, 0)
    [byte[]]$des.CreateEncryptor().TransformFinalBlock($plain, 0, 8)
}

# Hand the password to the extension in a file only the target user can read.
function Write-Handoff([string]$pw) {
    if (-not $HandoffDir -or -not $ForUser) {
        Write-Host "No hand-off location given. The VNC password is: $pw" -ForegroundColor Yellow
        return
    }
    New-Item -ItemType Directory -Force $HandoffDir | Out-Null
    $file = Join-Path $HandoffDir 'password.txt'
    Set-Content -Path $file -Value $pw -Encoding ASCII -NoNewline
    $acl = New-Object Security.AccessControl.FileSecurity
    $acl.SetAccessRuleProtection($true, $false)
    $acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule($ForUser, 'FullControl', 'Allow')))
    Set-Acl -Path $file -AclObject $acl
}

function Get-PortOwner([int]$port) {
    $c = Get-NetTCPConnection -State Listen -LocalPort $port -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($c) { (Get-Process -Id $c.OwningProcess -ErrorAction SilentlyContinue).ProcessName } else { $null }
}

function Select-Port {
    $owner = Get-PortOwner 5900
    if (-not $owner -or $owner -eq 'tvnserver') { return 5900 }
    Write-Host "Port 5900 is used by '$owner'; choosing another port."
    foreach ($p in 5917..5999) { if (-not (Get-PortOwner $p)) { return $p } }
    throw 'No free port found between 5917 and 5999.'
}

function Get-TightVncProductCode {
    $keys = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*',
            'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*'
    Get-ItemProperty $keys -ErrorAction SilentlyContinue |
        Where-Object { $_.DisplayName -like 'TightVNC*' -and $_.PSChildName -like '{*}' } |
        Select-Object -First 1 -ExpandProperty PSChildName
}

function Wait-RfbGreeting([int]$port) {
    for ($i = 0; $i -lt 20; $i++) {
        try {
            $owner = Get-PortOwner $port
            if ($owner -eq 'tvnserver') {
                $client = [Net.Sockets.TcpClient]::new('127.0.0.1', $port)
                $stream = $client.GetStream(); $stream.ReadTimeout = 3000
                $bytes = New-Object byte[] 12
                $n = $stream.Read($bytes, 0, 12)
                $client.Close()
                if ([Text.Encoding]::ASCII.GetString($bytes, 0, $n) -like 'RFB *') { return $true }
            }
        } catch { }
        Start-Sleep -Milliseconds 500
    }
    return $false
}

# ---- uninstall -----------------------------------------------------------------

if ($Uninstall) {
    Write-Host '== Remote Desktop: remove full-access mode ==' -ForegroundColor Cyan
    $marker = Get-Marker
    if ($marker -and $marker.installedBy -eq 'extension') {
        $code = Get-TightVncProductCode
        if ($code) {
            Write-Host 'Uninstalling TightVNC...'
            $p = Start-Process msiexec.exe -ArgumentList "/x $code /qn /norestart" -Wait -PassThru
            if ($p.ExitCode -ne 0) { Write-Host "Uninstall returned exit code $($p.ExitCode)." -ForegroundColor Yellow }
        }
        Remove-Item 'HKLM:\SOFTWARE\TightVNC' -Recurse -ErrorAction SilentlyContinue
        Write-Host 'TightVNC removed.' -ForegroundColor Green
    } else {
        Write-Host 'TightVNC was not installed by the extension, so it is left installed.'
    }
    Remove-Item $MarkerFile -ErrorAction SilentlyContinue
    Remove-Item $MarkerDir -ErrorAction SilentlyContinue
    Finish 0
}

# ---- install / configure ---------------------------------------------------------

Write-Host '== Remote Desktop: full-access mode setup ==' -ForegroundColor Cyan
$marker = Get-Marker
$installed = Test-Path $RegPath
$mode = 'install'

if ($installed -and $marker -and $marker.installedBy -ne 'user') {
    $mode = 'reconfigure'
    $installedBy = $marker.installedBy
} elseif ($installed) {
    Write-Host ''
    Write-Host 'TightVNC is already installed on this machine.'
    Write-Host '  [T] Take over: accept connections only from this machine and set a new random password.'
    Write-Host '      Any existing remote access to it from other computers stops working.'
    Write-Host '  [K] Keep its settings: only allow connections from this machine as well.'
    Write-Host '      You will be asked for its existing password the first time you connect.'
    Write-Host '  [C] Cancel'
    switch -Regex (Read-Host 'Choose T, K or C') {
        '^[Tt]' { $mode = 'reconfigure'; $installedBy = 'user-takeover' }
        '^[Kk]' { $mode = 'keep' }
        default { Write-Host 'Cancelled.'; Finish 1 }
    }
}

if ($mode -eq 'keep') {
    Set-ItemProperty $RegPath -Name AllowLoopback -Value 1 -Type DWord
    $port = (Get-ItemProperty $RegPath -Name RfbPort -ErrorAction SilentlyContinue).RfbPort
    if (-not $port) { $port = 5900 }
    Restart-Service tvnserver
    Set-Marker 'user' $port
    if (Wait-RfbGreeting $port) {
        Write-Host "Done. The extension will use TightVNC on port $port." -ForegroundColor Green
        Finish 0
    }
    Write-Host "TightVNC did not answer on port $port." -ForegroundColor Red
    Finish 1
}

$port = Select-Port

if ($mode -eq 'install') {
    if (-not (Get-Command winget -ErrorAction SilentlyContinue)) {
        Write-Host 'winget is not available. Install TightVNC manually from https://www.tightvnc.com and run this again.' -ForegroundColor Red
        Finish 1
    }
    # The password is set afterwards in the registry, so it never appears on a command line.
    $msiArgs = @(
        '/quiet', '/norestart', 'ADDLOCAL=Server',
        'SET_USEVNCAUTHENTICATION=1', 'VALUE_OF_USEVNCAUTHENTICATION=1',
        'SET_ALLOWLOOPBACK=1', 'VALUE_OF_ALLOWLOOPBACK=1',
        'SET_LOOPBACKONLY=1', 'VALUE_OF_LOOPBACKONLY=1',
        'SET_ACCEPTHTTPCONNECTIONS=1', 'VALUE_OF_ACCEPTHTTPCONNECTIONS=0',
        'SET_RFBPORT=1', "VALUE_OF_RFBPORT=$port"
    ) -join ' '
    Write-Host 'Installing TightVNC Server with winget...'
    winget install --id GlavSoft.TightVNC --exact --silent --accept-package-agreements --accept-source-agreements --override $msiArgs
    if (-not (Test-Path $RegPath)) {
        Write-Host "Installation did not complete (winget exit code $LASTEXITCODE)." -ForegroundColor Red
        Finish 1
    }
    $installedBy = 'extension'
}

# Enforce the security-relevant settings and set a fresh random password.
$password = New-RandomPassword
Set-ItemProperty $RegPath -Name AllowLoopback         -Value 1 -Type DWord
Set-ItemProperty $RegPath -Name LoopbackOnly          -Value 1 -Type DWord
Set-ItemProperty $RegPath -Name AcceptHttpConnections -Value 0 -Type DWord
Set-ItemProperty $RegPath -Name UseVncAuthentication  -Value 1 -Type DWord
Set-ItemProperty $RegPath -Name RfbPort               -Value $port -Type DWord
Set-ItemProperty $RegPath -Name Password              -Value (Protect-VncPassword $password) -Type Binary

# Nothing should be reachable from the network.
$rules = Get-NetFirewallRule -Direction Inbound -ErrorAction SilentlyContinue | Where-Object { $_.DisplayName -match 'TightVNC' -and $_.Enabled -eq 'True' }
if ($rules) {
    $rules | Disable-NetFirewallRule
    Write-Host "Disabled $(@($rules).Count) inbound firewall rule(s) for TightVNC."
}

Write-Host 'Restarting the TightVNC service (open desktop tabs will reconnect)...'
Set-Service tvnserver -StartupType Automatic
Restart-Service tvnserver

Write-Handoff $password
Set-Marker $installedBy $port
$password = $null

if (Wait-RfbGreeting $port) {
    Write-Host ''
    Write-Host "Full-access mode is ready (TightVNC service on 127.0.0.1:$port, localhost only)." -ForegroundColor Green
    Write-Host 'In VS Code run "Remote Desktop: Open Desktop".'
    Finish 0
}
Write-Host "TightVNC did not answer on 127.0.0.1:$port. Check the 'tvnserver' service." -ForegroundColor Red
Finish 1
