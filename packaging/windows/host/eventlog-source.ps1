<#
.SYNOPSIS
Installs or uninstalls the best-effort ArcenPier Windows Event Log source.

.DESCRIPTION
Registers (or removes) the classic Application-channel event source used by
`hosts/windows/src/eventlog.rs` to report the shared Lifecycle Event Log
vocabulary (`RegisterEventSourceW("ArcenPier")` / `ReportEventW`). This
script is owned independently of
`hosts/windows/credential-provider/registration-common.ps1`: the Pier
service and the Credential Provider have different install/uninstall
lifetimes and must not share a registration script or state.

No compiled message DLL ships in v1: every rendered lifecycle record is raw,
deterministic insertion strings (see `eventlog.rs::build_insertion_strings`),
so this script never writes an `EventMessageFile` or `CategoryMessageFile`
value.

Install is idempotent: running it again as the same owner re-asserts the
same values and succeeds. Both Install and Uninstall refuse to touch a
registration that was not created by this script (no `ArcenOwned` marker, or
a marker with a different value) — for example a third-party or built-in
Windows source that happens to be named `ArcenPier`.

.PARAMETER Install
Registers the `ArcenPier` Application event source. Requires an elevated
64-bit PowerShell session unless -SkipContextCheck is also passed (tests
only).

.PARAMETER Uninstall
Removes the `ArcenPier` Application event source if, and only if, it is
owned by Arcen. Requires an elevated 64-bit PowerShell session unless
-SkipContextCheck is also passed (tests only).

.PARAMETER SkipContextCheck
Test-only escape hatch that skips the 64-bit/elevation assertion so unit
tests can exercise the registry logic under an ordinary, unprivileged test
session. Production installs must not pass this switch.

.EXAMPLE
.\eventlog-source.ps1 -Install

.EXAMPLE
.\eventlog-source.ps1 -Uninstall
#>
[CmdletBinding()]
param(
    [switch]$Install,
    [switch]$Uninstall,
    [switch]$SkipContextCheck
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Provider/source name and channel. Must match `eventlog::EVENT_PROVIDER` /
# `eventlog::EVENT_CHANNEL` in `hosts/windows/src/eventlog.rs`.
$script:EventSourceName = 'ArcenPier'
$script:EventLogChannel = 'Application'
$script:EventSourceSubkey =
    "SYSTEM\CurrentControlSet\Services\EventLog\$($script:EventLogChannel)\$($script:EventSourceName)"

# A registry value with no reserved meaning to the Windows Event Log
# service, used only so this script can prove it created a given
# registration before it agrees to modify or delete it.
$script:OwnershipMarkerName = 'ArcenOwned'
$script:OwnershipMarkerValue = 'arcen-pier-windows'

# EVENTLOG_ERROR_TYPE (1) | EVENTLOG_WARNING_TYPE (2) | EVENTLOG_INFORMATION_TYPE (4).
$script:TypesSupportedValue = 7

function Assert-ArcenEventSourceContext {
    <#
    .SYNOPSIS
    Verifies a 64-bit, elevated context. Production install/uninstall calls
    must go through this; only tests bypass it via -SkipContextCheck.
    #>
    if (-not [Environment]::Is64BitOperatingSystem -or -not [Environment]::Is64BitProcess) {
        throw 'ArcenPier event source registration must run in 64-bit PowerShell on 64-bit Windows.'
    }

    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'ArcenPier event source registration requires an elevated administrator or LocalSystem.'
    }
}

function Open-ArcenEventSourceRegistry64 {
    <#
    .SYNOPSIS
    Opens the 64-bit HKLM view. Separate from, and not shared with, the
    Credential Provider's registration helpers.
    #>
    return [Microsoft.Win32.RegistryKey]::OpenBaseKey(
        [Microsoft.Win32.RegistryHive]::LocalMachine,
        [Microsoft.Win32.RegistryView]::Registry64
    )
}

function Test-ArcenEventSourceOwned {
    <#
    .SYNOPSIS
    Returns whether the event source subkey exists and carries this
    script's ownership marker.
    #>
    param(
        [Parameter(Mandatory)][Microsoft.Win32.RegistryKey]$Base,
        [Parameter(Mandatory)][string]$Subkey
    )

    $key = $Base.OpenSubKey($Subkey, $false)
    if ($null -eq $key) {
        return $false
    }
    try {
        return $key.GetValue($script:OwnershipMarkerName) -eq $script:OwnershipMarkerValue
    }
    finally {
        $key.Dispose()
    }
}

function Install-ArcenEventSource {
    <#
    .SYNOPSIS
    Idempotently registers the best-effort ArcenPier Application event
    source. Refuses to overwrite a foreign (non-Arcen-owned) registration.

    .PARAMETER Base
    Test-only dependency injection point: an already-open registry base key
    (for example an HKCU test root). Production callers omit this and get
    the real 64-bit HKLM view.

    .PARAMETER Subkey
    Test-only override of the registered subkey path. Production callers
    omit this and get the real
    `SYSTEM\CurrentControlSet\Services\EventLog\Application\ArcenPier` path.
    #>
    param(
        [Microsoft.Win32.RegistryKey]$Base,
        [string]$Subkey = $script:EventSourceSubkey,
        [switch]$SkipContextCheck
    )

    if (-not $SkipContextCheck) {
        Assert-ArcenEventSourceContext
    }

    $ownsRegistryHandle = $null -eq $Base
    $registry = if ($Base) { $Base } else { Open-ArcenEventSourceRegistry64 }
    try {
        $existing = $registry.OpenSubKey($Subkey, $false)
        if ($null -ne $existing) {
            $existing.Dispose()
            if (-not (Test-ArcenEventSourceOwned -Base $registry -Subkey $Subkey)) {
                throw "An event source is already registered at '$Subkey' and is not owned by " +
                    'Arcen (missing or mismatched ArcenOwned marker); refusing to overwrite it.'
            }
        }

        # CreateSubKey opens the existing key in place when it is already
        # owned by Arcen, so a second install re-asserts the same values
        # (idempotent) instead of failing or duplicating anything.
        $key = $registry.CreateSubKey($Subkey, $true)
        if ($null -eq $key) {
            throw "Unable to create or open the ArcenPier event source key: $Subkey"
        }
        try {
            $key.SetValue(
                'TypesSupported',
                $script:TypesSupportedValue,
                [Microsoft.Win32.RegistryValueKind]::DWord)
            $key.SetValue(
                $script:OwnershipMarkerName,
                $script:OwnershipMarkerValue,
                [Microsoft.Win32.RegistryValueKind]::String)
            # No EventMessageFile / CategoryMessageFile value: v1 ships no
            # compiled message DLL. Rendered records carry raw, deterministic
            # insertion strings instead (see eventlog.rs).
        }
        finally {
            $key.Dispose()
        }
    }
    finally {
        if ($ownsRegistryHandle) {
            $registry.Dispose()
        }
    }
}

function Uninstall-ArcenEventSource {
    <#
    .SYNOPSIS
    Removes the ArcenPier Application event source only if it is owned by
    Arcen. A missing key is not an error (idempotent). Refuses to delete a
    foreign registration.

    .PARAMETER Base
    Test-only dependency injection point; see Install-ArcenEventSource.

    .PARAMETER Subkey
    Test-only subkey override; see Install-ArcenEventSource.
    #>
    param(
        [Microsoft.Win32.RegistryKey]$Base,
        [string]$Subkey = $script:EventSourceSubkey,
        [switch]$SkipContextCheck
    )

    if (-not $SkipContextCheck) {
        Assert-ArcenEventSourceContext
    }

    $ownsRegistryHandle = $null -eq $Base
    $registry = if ($Base) { $Base } else { Open-ArcenEventSourceRegistry64 }
    try {
        $existing = $registry.OpenSubKey($Subkey, $false)
        if ($null -eq $existing) {
            # Already absent: uninstall is idempotent, not an error.
            return
        }
        $existing.Dispose()
        if (-not (Test-ArcenEventSourceOwned -Base $registry -Subkey $Subkey)) {
            throw "The event source at '$Subkey' is not owned by Arcen (missing or " +
                'mismatched ArcenOwned marker); refusing to delete it.'
        }

        $registry.DeleteSubKeyTree($Subkey, $false)
    }
    finally {
        if ($ownsRegistryHandle) {
            $registry.Dispose()
        }
    }
}

if ($Install -and $Uninstall) {
    throw 'Specify only one of -Install or -Uninstall.'
}
if ($Install) {
    Install-ArcenEventSource -SkipContextCheck:$SkipContextCheck
    Write-Output "ArcenPier event source registered at HKLM\$($script:EventSourceSubkey)."
}
elseif ($Uninstall) {
    Uninstall-ArcenEventSource -SkipContextCheck:$SkipContextCheck
    Write-Output "ArcenPier event source removed from HKLM\$($script:EventSourceSubkey) (if present)."
}
