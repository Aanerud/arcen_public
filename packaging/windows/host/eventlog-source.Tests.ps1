<#
.SYNOPSIS
Parser/ownership tests for packaging\windows\host\eventlog-source.ps1.

.DESCRIPTION
Exercises install/uninstall idempotency and foreign-registration refusal
against an isolated HKCU test subkey via the script's -Base/-Subkey
dependency-injection parameters. Never touches the real
`HKLM\SYSTEM\CurrentControlSet\Services\EventLog\Application\ArcenPier`
registration.

Uses explicit try/catch assertions rather than Pester 3's `Should Throw`:
the version of Pester bundled with Windows PowerShell (3.4.0) fails to
detect a thrown exception once anything in the same `Describe` scope has
set `$ErrorActionPreference = 'Stop'` (which this script's own body does),
even after it is reset back to `'Continue'`. Dot-sourcing a script always
imports its top-level preference/strict-mode state into the caller's scope,
so this avoids that interaction entirely instead of relying on it being
reset correctly.

Run with:
    Invoke-Pester -Script @{ Path = '.' }
from this directory, or point Invoke-Pester at this file directly.
#>

$scriptPath = Join-Path $PSScriptRoot 'eventlog-source.ps1'

function New-ArcenEventLogTestRegistryBase {
    return [Microsoft.Win32.RegistryKey]::OpenBaseKey(
        [Microsoft.Win32.RegistryHive]::CurrentUser,
        [Microsoft.Win32.RegistryView]::Registry64
    )
}

function Test-ArcenScriptBlockThrows {
    <#
    .SYNOPSIS
    Returns $true iff invoking $ScriptBlock throws. See the file-level
    description for why this replaces Pester 3's `Should Throw` here.
    #>
    param([Parameter(Mandatory)][scriptblock]$ScriptBlock)

    try {
        & $ScriptBlock
        return $false
    }
    catch {
        return $true
    }
}

Describe 'ArcenPier event source registration' {
    # Dot-sourcing with no -Install/-Uninstall switch only defines the
    # functions below; it performs no registry I/O.
    . $scriptPath

    $testRoot = "SOFTWARE\ArcenEventLogSourceTests\$([Guid]::NewGuid().ToString('N'))"
    $testSubkey = "$testRoot\ArcenPier"

    AfterEach {
        $hkcu = New-ArcenEventLogTestRegistryBase
        try {
            $hkcu.DeleteSubKeyTree($testRoot, $false)
        }
        catch {
            # Nothing to clean up if a test never created the root.
        }
        finally {
            $hkcu.Dispose()
        }
    }

    It 'parses the script and exposes the install/uninstall functions' {
        Get-Command Install-ArcenEventSource | Should Not BeNullOrEmpty
        Get-Command Uninstall-ArcenEventSource | Should Not BeNullOrEmpty
        Get-Command Test-ArcenEventSourceOwned | Should Not BeNullOrEmpty
    }

    It 'creates a new registration with TypesSupported, the ownership marker, and no message DLL' {
        $base = New-ArcenEventLogTestRegistryBase
        try {
            Install-ArcenEventSource -Base $base -Subkey $testSubkey -SkipContextCheck
            $key = $base.OpenSubKey($testSubkey)
            try {
                $key | Should Not BeNullOrEmpty
                $key.GetValue('TypesSupported') | Should Be 7
                $key.GetValue('ArcenOwned') | Should Be 'arcen-pier-windows'
                $key.GetValue('EventMessageFile') | Should BeNullOrEmpty
            }
            finally {
                $key.Dispose()
            }
        }
        finally {
            $base.Dispose()
        }
    }

    It 'is idempotent: a second install by the same owner succeeds and keeps one registration' {
        $base = New-ArcenEventLogTestRegistryBase
        try {
            Install-ArcenEventSource -Base $base -Subkey $testSubkey -SkipContextCheck
            $secondInstallThrew = Test-ArcenScriptBlockThrows {
                Install-ArcenEventSource -Base $base -Subkey $testSubkey -SkipContextCheck
            }
            $secondInstallThrew | Should Be $false

            $key = $base.OpenSubKey($testSubkey)
            try {
                $key.GetValue('ArcenOwned') | Should Be 'arcen-pier-windows'
                $key.GetValue('TypesSupported') | Should Be 7
            }
            finally {
                $key.Dispose()
            }
        }
        finally {
            $base.Dispose()
        }
    }

    It 'refuses to overwrite a foreign (non-Arcen-owned) registration' {
        $base = New-ArcenEventLogTestRegistryBase
        try {
            $foreign = $base.CreateSubKey($testSubkey, $true)
            try {
                $foreign.SetValue('TypesSupported', 7, [Microsoft.Win32.RegistryValueKind]::DWord)
                $foreign.SetValue('EventMessageFile', 'C:\Windows\System32\SomeOtherVendor.dll')
            }
            finally {
                $foreign.Dispose()
            }

            $installThrew = Test-ArcenScriptBlockThrows {
                Install-ArcenEventSource -Base $base -Subkey $testSubkey -SkipContextCheck
            }
            $installThrew | Should Be $true

            $key = $base.OpenSubKey($testSubkey)
            try {
                # The foreign registration must be untouched.
                $key.GetValue('EventMessageFile') | Should Be 'C:\Windows\System32\SomeOtherVendor.dll'
                $key.GetValue('ArcenOwned') | Should BeNullOrEmpty
            }
            finally {
                $key.Dispose()
            }
        }
        finally {
            $base.Dispose()
        }
    }

    It 'owned uninstall removes the registration' {
        $base = New-ArcenEventLogTestRegistryBase
        try {
            Install-ArcenEventSource -Base $base -Subkey $testSubkey -SkipContextCheck
            Uninstall-ArcenEventSource -Base $base -Subkey $testSubkey -SkipContextCheck
            $base.OpenSubKey($testSubkey) | Should BeNullOrEmpty
        }
        finally {
            $base.Dispose()
        }
    }

    It 'uninstall is idempotent when nothing is registered' {
        $base = New-ArcenEventLogTestRegistryBase
        try {
            $uninstallThrew = Test-ArcenScriptBlockThrows {
                Uninstall-ArcenEventSource -Base $base -Subkey $testSubkey -SkipContextCheck
            }
            $uninstallThrew | Should Be $false
        }
        finally {
            $base.Dispose()
        }
    }

    It 'refuses to delete a foreign (non-Arcen-owned) registration' {
        $base = New-ArcenEventLogTestRegistryBase
        try {
            $foreign = $base.CreateSubKey($testSubkey, $true)
            try {
                $foreign.SetValue('TypesSupported', 7, [Microsoft.Win32.RegistryValueKind]::DWord)
            }
            finally {
                $foreign.Dispose()
            }

            $uninstallThrew = Test-ArcenScriptBlockThrows {
                Uninstall-ArcenEventSource -Base $base -Subkey $testSubkey -SkipContextCheck
            }
            $uninstallThrew | Should Be $true
            $base.OpenSubKey($testSubkey) | Should Not BeNullOrEmpty
        }
        finally {
            $base.Dispose()
        }
    }

    It 'refuses -Install and -Uninstall together at the script entry point' {
        $bothSwitchesThrew = Test-ArcenScriptBlockThrows {
            & $scriptPath -Install -Uninstall -SkipContextCheck
        }
        $bothSwitchesThrew | Should Be $true
    }
}
