# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS
#Requires -Version 7.0
<#
.SYNOPSIS
    Creates, keeps, renews or rekeys the Windows Pier's PEM TLS material.

.DESCRIPTION
    A thin front end: the work is done by `install-arcen-pier.exe --tls-only`,
    the same code the installer runs, so the certificate profile, the names a
    renewal keeps, the journalled replacement and its crash recovery, the lock
    and the ACLs are the ones every Pier shares. Nothing is decided here.

    With no switch, material is created only when there is none, kept when it
    is current, and renewed over the existing key when it is due. -Renew
    reissues over the existing key now (SPKI pins stay valid). -ForceNewKey
    replaces the key (every Deck must re-pin). The helper never uses or
    mutates a Windows certificate store.

.PARAMETER OutputRoot
    The TLS directory. Defaults to %ProgramData%\Arcen\tls. Its last
    component must be `tls`: the installer keeps TLS material in <root>\tls.

.PARAMETER Installer
    install-arcen-pier.exe to run. Defaults to $env:ARCEN_INSTALLER, then the
    copy beside this script, then Program Files. A build in a repository
    checkout is named explicitly.
#>
[CmdletBinding()]
param(
    # Accepted for older scripts; the subject is always CN=Arcen Pier.
    [string]$Subject,
    [string[]]$ExtraDns = @(),
    [string[]]$ExtraIp = @(),
    [switch]$Renew,
    [switch]$AdoptLegacyHelperPair,
    [switch]$ForceNewKey,
    # Deprecated alias of -ForceNewKey.
    [switch]$Force,
    [string]$OutputRoot,
    [string]$Installer,
    [switch]$DryRun,
    [switch]$LibraryOnly
)

$ErrorActionPreference = 'Stop'

# Files only the earlier PowerShell helper wrote. Its journal cannot be
# recovered by the shared transaction, and its pin files are not refreshed
# by it, so a stale copy would show an operator the wrong fingerprint.
$script:LegacyJournal = 'host-cert.transaction.json'
$script:LegacyPinFiles = @(
    'host.fingerprint.txt',
    'host.spki-sha256.txt',
    'host.generated-by-arcen.txt'
)

function Resolve-ArcenInstaller {
    param([string]$Explicit, [string]$ScriptRoot = $PSScriptRoot)
    # Only places as trusted as this script or chosen by the operator: no
    # path relative to it, which outside a checkout can reach a folder any
    # user may create (C:\target\...), for a program run as Administrator.
    $candidates = @(
        $Explicit,
        $env:ARCEN_INSTALLER,
        (Join-Path $ScriptRoot 'install-arcen-pier.exe'),
        (Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'Arcen\Pier\install-arcen-pier.exe')
    ) | Where-Object { $_ }
    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
            return (Resolve-Path -LiteralPath $candidate).Path
        }
    }
    throw 'install-arcen-pier.exe not found; pass -Installer or set ARCEN_INSTALLER.'
}

function Get-ArcenTlsArguments {
    param(
        [string]$OutputRoot,
        [string[]]$ExtraDns = @(),
        [string[]]$ExtraIp = @(),
        [switch]$Renew,
        [switch]$AdoptLegacyHelperPair,
        [switch]$ForceNewKey,
        [switch]$DryRun
    )
    if ($Renew -and $ForceNewKey) {
        throw '-Renew and -ForceNewKey are mutually exclusive.'
    }
    if ($AdoptLegacyHelperPair -and -not $Renew) {
        throw '-AdoptLegacyHelperPair requires -Renew.'
    }
    $arguments = [Collections.Generic.List[string]]::new()
    $arguments.Add('--tls-only')
    if ($OutputRoot) {
        $full = [IO.Path]::GetFullPath($OutputRoot)
        if ((Split-Path -Leaf $full) -ne 'tls') {
            throw "-OutputRoot must be a directory named tls (the installer keeps material in <root>\tls): $full"
        }
        $arguments.Add('--programdata')
        $arguments.Add((Split-Path -Parent $full))
    }
    if ($ForceNewKey) {
        $arguments.Add('--force')
    } elseif ($AdoptLegacyHelperPair) {
        $arguments.Add('--adopt-legacy')
    } elseif ($Renew) {
        $arguments.Add('--renew')
    }
    foreach ($name in @($ExtraDns) + @($ExtraIp)) {
        if ($name) {
            $arguments.Add('--extra-san')
            $arguments.Add($name)
        }
    }
    if ($DryRun) {
        $arguments.Add('--dry-run')
    }
    return , $arguments.ToArray()
}

if ($LibraryOnly) { return }

if ($Force) {
    Write-Warning '-Force is deprecated and maps to -ForceNewKey; this changes Deck trust.'
    $ForceNewKey = $true
}
if ($Subject) {
    Write-Warning "-Subject is ignored: every Arcen certificate is issued as CN=Arcen Pier, with '$Subject' kept only if it is also a name it covers."
}
$tlsRoot = if ($OutputRoot) {
    [IO.Path]::GetFullPath($OutputRoot)
} else {
    Join-Path $env:ProgramData 'Arcen\tls'
}
if (Test-Path -LiteralPath (Join-Path $tlsRoot $script:LegacyJournal)) {
    throw "An interrupted transaction of the earlier helper ($script:LegacyJournal) is in $tlsRoot. Recover it with that helper's version, or restore the files by hand, before running this one."
}

$arguments = Get-ArcenTlsArguments -OutputRoot $OutputRoot -ExtraDns $ExtraDns -ExtraIp $ExtraIp `
    -Renew:$Renew -AdoptLegacyHelperPair:$AdoptLegacyHelperPair -ForceNewKey:$ForceNewKey -DryRun:$DryRun
$installerPath = Resolve-ArcenInstaller -Explicit $Installer
& $installerPath @arguments
$status = $LASTEXITCODE
if ($status -ne 0) {
    throw "install-arcen-pier.exe --tls-only failed with exit code $status."
}
if (-not $DryRun -and (Test-Path -LiteralPath (Join-Path $tlsRoot 'host.spki-sha256'))) {
    foreach ($name in $script:LegacyPinFiles) {
        $stale = Join-Path $tlsRoot $name
        if (Test-Path -LiteralPath $stale -PathType Leaf) {
            Remove-Item -LiteralPath $stale -Force
            Write-Host "Removed the earlier helper's $name; the shared pin files replace it."
        }
    }
}
