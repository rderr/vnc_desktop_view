# Watches whether remote input can reach the foreground window when TightVNC
# runs in application mode (as the user, at normal privilege).
#
# Windows' UIPI silently drops injected mouse/keyboard input while the
# foreground window belongs to a higher-privilege process (e.g. an
# "Run as administrator" window), and a normal process can't see or control
# the secure desktop (UAC prompts, lock screen, Ctrl+Alt+Del screen).
#
# Prints one JSON line whenever the state changes:
#   {"blocked":false}
#   {"blocked":true,"reason":"elevated","process":"powershell","title":"Administrator: Windows PowerShell"}
#   {"blocked":true,"reason":"secure"}
# Exits when the parent process (the VS Code extension host) goes away.

param([int]$ParentPid = 0, [int]$IntervalMs = 1000)

$ErrorActionPreference = 'Stop'

Add-Type -Namespace RemoteDesktop -Name Native -UsingNamespace System.Text -MemberDefinition @'
[DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
[DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
[DllImport("user32.dll", SetLastError = true)] static extern IntPtr OpenInputDesktop(uint flags, bool inherit, uint access);
[DllImport("user32.dll")] static extern bool CloseDesktop(IntPtr desktop);
[DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern bool GetUserObjectInformation(IntPtr obj, int index, StringBuilder buf, int len, out int needed);
[DllImport("kernel32.dll", SetLastError = true)] static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
[DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);
[DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();
[DllImport("advapi32.dll", SetLastError = true)] static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);
[DllImport("advapi32.dll", SetLastError = true)] static extern bool GetTokenInformation(IntPtr token, int cls, IntPtr buf, int len, out int ret);
[DllImport("advapi32.dll")] static extern IntPtr GetSidSubAuthorityCount(IntPtr sid);
[DllImport("advapi32.dll")] static extern IntPtr GetSidSubAuthority(IntPtr sid, uint index);

const uint PROCESS_QUERY_LIMITED_INFORMATION = 0x1000;
const uint TOKEN_QUERY = 0x0008;
const int TokenIntegrityLevel = 25;

/// Mandatory integrity level RID (0x2000 medium, 0x3000 high, 0x4000 system), or -1 if it can't be read.
static int IntegrityOfProcess(IntPtr process) {
    IntPtr token;
    if (!OpenProcessToken(process, TOKEN_QUERY, out token)) return -1;
    try {
        int len;
        GetTokenInformation(token, TokenIntegrityLevel, IntPtr.Zero, 0, out len);
        if (len <= 0) return -1;
        IntPtr buf = Marshal.AllocHGlobal(len);
        try {
            if (!GetTokenInformation(token, TokenIntegrityLevel, buf, len, out len)) return -1;
            IntPtr sid = Marshal.ReadIntPtr(buf);
            int count = Marshal.ReadByte(GetSidSubAuthorityCount(sid));
            return Marshal.ReadInt32(GetSidSubAuthority(sid, (uint)(count - 1)));
        } finally { Marshal.FreeHGlobal(buf); }
    } finally { CloseHandle(token); }
}

public static int OwnIntegrity() { return IntegrityOfProcess(GetCurrentProcess()); }

/// Process id owning the foreground window, or 0.
public static uint ForegroundPid() {
    IntPtr hwnd = GetForegroundWindow();
    if (hwnd == IntPtr.Zero) return 0;
    uint pid; GetWindowThreadProcessId(hwnd, out pid);
    return pid;
}

/// Integrity level of a process; -1 when a normal process isn't allowed to read it
/// (which itself means it runs at a higher privilege).
public static int IntegrityOfPid(uint pid) {
    IntPtr p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid);
    if (p == IntPtr.Zero) return -1;
    try { return IntegrityOfProcess(p); } finally { CloseHandle(p); }
}

/// Name of the desktop currently receiving input: "Default" normally,
/// "Winlogon" for UAC prompts / lock screen; null if we may not open it (also secure).
public static string InputDesktopName() {
    IntPtr d = OpenInputDesktop(0, false, 0x0001 /* DESKTOP_READOBJECTS */);
    if (d == IntPtr.Zero) return null;
    try {
        var sb = new StringBuilder(256); int needed;
        GetUserObjectInformation(d, 2 /* UOI_NAME */, sb, sb.Capacity * 2, out needed);
        return sb.ToString();
    } finally { CloseDesktop(d); }
}
'@

$self = [RemoteDesktop.Native]::OwnIntegrity()
$last = ''

while ($true) {
    if ($ParentPid -and -not (Get-Process -Id $ParentPid -ErrorAction SilentlyContinue)) { exit 0 }

    $state = [ordered]@{ blocked = $false }
    try {
        $desktop = [RemoteDesktop.Native]::InputDesktopName()
        if ($desktop -ne 'Default') {
            $state.blocked = $true
            $state.reason = 'secure'
        } else {
            $fgPid = [RemoteDesktop.Native]::ForegroundPid()
            if ($fgPid -ne 0) {
                $level = [RemoteDesktop.Native]::IntegrityOfPid($fgPid)
                if ($level -eq -1 -or $level -gt $self) {
                    $p = Get-Process -Id $fgPid -ErrorAction SilentlyContinue
                    $state.blocked = $true
                    $state.reason = 'elevated'
                    $state.process = if ($p) { $p.ProcessName } else { "pid $fgPid" }
                    $state.title = if ($p) { $p.MainWindowTitle } else { '' }
                }
            }
        }
    } catch {
        $state = [ordered]@{ blocked = $false; error = $_.Exception.Message }
    }

    $json = $state | ConvertTo-Json -Compress
    if ($json -ne $last) {
        [Console]::Out.WriteLine($json)
        [Console]::Out.Flush()
        $last = $json
    }
    Start-Sleep -Milliseconds $IntervalMs
}
