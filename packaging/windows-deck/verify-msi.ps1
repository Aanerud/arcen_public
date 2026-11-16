# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS

<#
.SYNOPSIS
Checks a built Windows Deck MSI against the packaging contract.

.DESCRIPTION
Reads the MSI database read-only through the Windows Installer automation
interface and fails on anything the packaging policy forbids:

- no Binary-table payload and no DLL, script or embedded-EXE custom action,
  so no WiX (MS-RL) code ships inside an Arcen MSI and nothing runs that is
  not one of our own installed, signed-with-the-package executables;
- exactly the expected files, including the five legal files;
- the build ID in the package properties;
- Deck: per-machine, the Start-menu shortcut, WER LocalDumps as minidumps;
- USB component: the LocalSystem auto-start service, its failure actions,
  stop/remove on uninstall, and the deferred, non-impersonated uninstall
  cleanup scheduled after StopServices and skipped on upgrade.

Usage: verify-msi.ps1 -Msi "Arcen Deck.msi" -Kind Deck -BuildId <id> [-ReleaseCandidate 0|1]
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Msi,
    [Parameter(Mandatory = $true)][ValidateSet("Deck", "Usb")][string]$Kind,
    [Parameter(Mandatory = $true)][string]$BuildId,
    [ValidateSet("0", "1")][string]$ReleaseCandidate = "1"
)

Set-StrictMode -Version 3.0
$ErrorActionPreference = "Stop"

$path = (Resolve-Path -LiteralPath $Msi).Path
$installer = New-Object -ComObject WindowsInstaller.Installer
# 0 = msiOpenDatabaseModeReadOnly
$database = $installer.OpenDatabase($path, 0)
$failures = New-Object System.Collections.Generic.List[string]

function Fail([string]$Message) { $failures.Add($Message) }

function Test-Table([string]$Name) {
    $view = $database.OpenView("SELECT ``Name`` FROM ``_Tables`` WHERE ``Name`` = '$Name'")
    [void]$view.Execute()
    $record = $view.Fetch()
    [void]$view.Close()
    return ($null -ne $record)
}

function Get-Rows([string]$Table, [string[]]$Columns) {
    $rows = New-Object System.Collections.Generic.List[object]
    if (Test-Table $Table) {
        $select = ($Columns | ForEach-Object { "``$_``" }) -join ", "
        $view = $database.OpenView("SELECT $select FROM ``$Table``")
        [void]$view.Execute()
        while ($true) {
            $record = $view.Fetch()
            if ($null -eq $record) { break }
            $fields = [ordered]@{}
            for ($index = 0; $index -lt $Columns.Count; $index++) {
                $fields[$Columns[$index]] = [string]$record.StringData($index + 1)
            }
            $rows.Add((New-Object PSObject -Property $fields))
        }
        [void]$view.Close()
    }
    return $rows.ToArray()
}

function Get-Property([string]$Name) {
    foreach ($entry in Get-Rows "Property" @("Property", "Value")) {
        if ($entry.Property -eq $Name) { return $entry.Value }
    }
    return $null
}

# --- common -------------------------------------------------------------------
if ((Get-Property "ARCENBUILDID") -ne $BuildId) {
    Fail "ARCENBUILDID is '$(Get-Property 'ARCENBUILDID')', expected '$BuildId'"
}
if ((Get-Property "ARCENRELEASECANDIDATE") -ne $ReleaseCandidate) {
    Fail "ARCENRELEASECANDIDATE is '$(Get-Property 'ARCENRELEASECANDIDATE')', expected '$ReleaseCandidate'"
}
$comments = Get-Property "ARPCOMMENTS"
if (($ReleaseCandidate -eq "0") -ne ("$comments".StartsWith("Lab preview, not a release candidate."))) {
    Fail "Apps & features comment '$comments' misstates the release-candidate state"
}
if ((Get-Property "ALLUSERS") -ne "1") {
    Fail "not a per-machine package (ALLUSERS='$(Get-Property 'ALLUSERS')')"
}
if ((Get-Property "Manufacturer") -ne "TUTTIFRUTTI AS") {
    Fail "Manufacturer is '$(Get-Property 'Manufacturer')'"
}
foreach ($upgrade in Get-Rows "Upgrade" @("UpgradeCode", "Attributes")) {
    if ($upgrade.UpgradeCode -ne (Get-Property "UpgradeCode")) {
        Fail "Upgrade table names a foreign UpgradeCode $($upgrade.UpgradeCode)"
    }
}
if (-not @(Get-Rows "Upgrade" @("UpgradeCode")).Count) {
    Fail "no Upgrade rows: MajorUpgrade is missing"
}

$binaries = @(Get-Rows "Binary" @("Name"))
if ($binaries.Count) {
    Fail ("Binary table is not empty: " + (($binaries | ForEach-Object Name) -join ", ") +
        " (a WiX extension or embedded custom action slipped in)")
}

# Custom actions are allowed by name and base type (Type & 0x3F): the
# ARPINSTALLLOCATION setter (51), error-only actions (19), and the USB
# component's two steps, checked in detail below (34 = EXE in a directory,
# 18 = EXE from an installed file). Anything else fails.
$customActions = @(Get-Rows "CustomAction" @("Action", "Type", "Source", "Target"))
foreach ($action in $customActions) {
    $base = ([int]$action.Type) -band 0x3F
    $allowed = ($base -eq 19) -or
        ($base -eq 51 -and $action.Action -eq "SetARPINSTALLLOCATION" -and $action.Source -eq "ARPINSTALLLOCATION") -or
        ($Kind -eq "Usb" -and $base -eq 34 -and $action.Action -eq "ArcenUsbServiceFailureActions") -or
        ($Kind -eq "Usb" -and $base -eq 18 -and $action.Action -eq "ArcenUsbUninstallCleanup")
    if (-not $allowed) {
        Fail "custom action $($action.Action) (type $($action.Type), base $base) is not allowed"
    }
}

$legal = @("LICENSE", "LICENSE-EXCEPTION.md", "NOTICE", "TRADEMARKS.md", "THIRD_PARTY_NOTICES.md")
if ($Kind -eq "Deck") {
    $expectedFiles = @("arcen-deck.exe") + $legal
} else {
    $expectedFiles = @("arcen-usb-service.exe", "arcen-usb-prepare.exe") + $legal
}
# File.FileName is "SHORT~1|long name" when a short name was generated.
$files = @(Get-Rows "File" @("File", "FileName")) | ForEach-Object {
    $name = $_.FileName
    if ($name.Contains("|")) { $name = $name.Split("|")[1] }
    $name
}
$missing = $expectedFiles | Where-Object { $files -notcontains $_ }
$extra = $files | Where-Object { $expectedFiles -notcontains $_ }
if ($missing) { Fail ("missing files: " + ($missing -join ", ")) }
if ($extra) { Fail ("unexpected files: " + ($extra -join ", ")) }

# --- Deck ---------------------------------------------------------------------
if ($Kind -eq "Deck") {
    if ($customActions | Where-Object { (([int]$_.Type) -band 0x3F) -eq 18 }) {
        Fail "the Deck package must not run any executable"
    }
    if (Test-Table "ServiceInstall") { Fail "the Deck package must not install a service" }
    $shortcuts = @(Get-Rows "Shortcut" @("Shortcut", "Directory_", "Name", "Target"))
    if (-not ($shortcuts | Where-Object { $_.Directory_ -eq "ProgramMenuFolder" -and $_.Name -like "*Arcen Deck" })) {
        Fail "no 'Arcen Deck' Start-menu shortcut"
    }
    $werKey = "SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps\arcen-deck.exe"
    $registry = @(Get-Rows "Registry" @("Root", "Key", "Name", "Value"))
    $wer = @{}
    foreach ($row in $registry | Where-Object { $_.Root -eq "2" -and $_.Key -eq $werKey }) {
        $wer[$row.Name] = $row.Value
    }
    if ($wer["DumpType"] -ne "#1") { Fail "WER DumpType is '$($wer['DumpType'])', expected #1 (minidump)" }
    # "#%" marks REG_EXPAND_SZ in the Registry table.
    if ($wer["DumpFolder"] -ne "#%%LOCALAPPDATA%\Arcen\Deck\CrashDumps") {
        Fail "WER DumpFolder is '$($wer['DumpFolder'])', expected an expandable %LOCALAPPDATA%\Arcen\Deck\CrashDumps"
    }
    $others = $registry | Where-Object { $_.Key -ne $werKey }
    if ($others) { Fail ("unexpected registry rows: " + (($others | ForEach-Object Key) -join ", ")) }
}

# --- USB component -----------------------------------------------------------
if ($Kind -eq "Usb") {
    $services = @(Get-Rows "ServiceInstall" @("Name", "ServiceType", "StartType", "StartName", "Component_"))
    $service = $services | Where-Object { $_.Name -eq "ArcenUsbComponent" }
    if (-not $service -or $services.Count -ne 1) {
        Fail "expected exactly the ArcenUsbComponent service"
    } else {
        if ($service.ServiceType -ne "16") { Fail "service type $($service.ServiceType), expected 16 (own process)" }
        if ($service.StartType -ne "2") { Fail "start type $($service.StartType), expected 2 (automatic)" }
        if ($service.StartName -ne "LocalSystem") { Fail "service account '$($service.StartName)', expected LocalSystem" }
    }
    $control = @(Get-Rows "ServiceControl" @("Name", "Event", "Wait"))
    $event = ($control | Where-Object { $_.Name -eq "ArcenUsbComponent" }).Event
    # 0x1 start on install, 0x2 stop on install, 0x20 stop on uninstall,
    # 0x80 delete on uninstall.
    if ([int]$event -ne (0x1 -bor 0x2 -bor 0x20 -bor 0x80)) {
        Fail "ServiceControl events $event, expected start on install, stop always, remove on uninstall"
    }
    if (@(Get-Rows "MsiServiceConfigFailureActions" @("MsiServiceConfigFailureActions")).Count) {
        Fail "MsiServiceConfigFailureActions rows fail with error 1939; set failure actions with sc.exe"
    }
    $failureStep = $customActions | Where-Object { $_.Action -eq "ArcenUsbServiceFailureActions" }
    if (-not $failureStep) {
        Fail "no service failure-actions step"
    } else {
        $type = [int]$failureStep.Type
        if (($type -band 0x3F) -ne 34 -or -not ($type -band 0x400) -or -not ($type -band 0x800) -or ($type -band 0x40)) {
            Fail "failure-actions step has type $type, expected a deferred, SYSTEM, checked EXE"
        }
        if ($failureStep.Source -ne "System64Folder" -or
            $failureStep.Target -ne '"[System64Folder]sc.exe" failure ArcenUsbComponent reset= 86400 actions= restart/5000/restart/30000//0') {
            Fail "failure-actions step runs '$($failureStep.Source)' '$($failureStep.Target)'"
        }
    }
    $cleanup = $customActions | Where-Object { $_.Action -eq "ArcenUsbUninstallCleanup" }
    if (-not $cleanup) {
        Fail "no uninstall cleanup custom action"
    } else {
        $type = [int]$cleanup.Type
        # 0x400 deferred, 0x800 no impersonation; 0x40 (ignore exit code) must be clear.
        if (($type -band 0x3F) -ne 18) { Fail "cleanup is not an installed-EXE action (type $type)" }
        if (-not ($type -band 0x400)) { Fail "cleanup is not deferred (type $type)" }
        if (-not ($type -band 0x800)) { Fail "cleanup impersonates the user (type $type)" }
        if ($type -band 0x40) { Fail "cleanup ignores its exit code (type $type)" }
        if ($cleanup.Source -ne "ArcenUsbServiceExe" -or $cleanup.Target -ne "uninstall") {
            Fail "cleanup runs '$($cleanup.Source) $($cleanup.Target)'"
        }
    }
    $sequence = @(Get-Rows "InstallExecuteSequence" @("Action", "Condition", "Sequence"))
    $failureAt = $sequence | Where-Object { $_.Action -eq "ArcenUsbServiceFailureActions" }
    $installServices = $sequence | Where-Object { $_.Action -eq "InstallServices" }
    $startServices = $sequence | Where-Object { $_.Action -eq "StartServices" }
    if (-not $failureAt -or -not ([int]$failureAt.Sequence -gt [int]$installServices.Sequence -and
            [int]$failureAt.Sequence -lt [int]$startServices.Sequence)) {
        Fail "failure-actions step is not between InstallServices and StartServices"
    } elseif ($failureAt.Condition -ne 'NOT Installed') {
        Fail "failure-actions step condition '$($failureAt.Condition)'"
    }
    $step = $sequence | Where-Object { $_.Action -eq "ArcenUsbUninstallCleanup" }
    $stop = $sequence | Where-Object { $_.Action -eq "StopServices" }
    $delete = $sequence | Where-Object { $_.Action -eq "DeleteServices" }
    $remove = $sequence | Where-Object { $_.Action -eq "RemoveFiles" }
    if (-not $step) {
        Fail "cleanup is not sequenced"
    } else {
        $at = [int]$step.Sequence
        if (-not ($at -gt [int]$stop.Sequence -and $at -lt [int]$delete.Sequence -and $at -lt [int]$remove.Sequence)) {
            Fail "cleanup runs at $at, not between StopServices and DeleteServices/RemoveFiles"
        }
        if ($step.Condition -ne '($UsbServiceExe=2) AND NOT UPGRADINGPRODUCTCODE') {
            Fail "cleanup condition '$($step.Condition)' is not: service executable removed, not by an upgrade"
        }
    }
    $registry = @(Get-Rows "Registry" @("Key"))
    if ($registry.Count) { Fail "the USB component package must write no registry values" }
    $removeFolders = @(Get-Rows "RemoveFile" @("FileKey", "DirProperty", "FileName", "InstallMode"))
    if (-not ($removeFolders | Where-Object { $_.DirProperty -eq "ProgramDataArcenFolder" -and $_.FileName -eq "" -and $_.InstallMode -eq "2" })) {
        Fail "no on-uninstall removal of an empty %ProgramData%\Arcen"
    }
    $installDirectory = @(Get-Rows "Directory" @("Directory", "Directory_Parent", "DefaultDir")) |
        Where-Object { $_.Directory -ceq "UsbInstallFolder" }
    if (-not $installDirectory) { Fail "the install folder must have the private Id UsbInstallFolder" }
}

[System.Runtime.InteropServices.Marshal]::ReleaseComObject($database) | Out-Null
if ($failures.Count) {
    foreach ($failure in $failures) { Write-Host "FAIL $([IO.Path]::GetFileName($path)): $failure" }
    exit 1
}
Write-Host "verified $([IO.Path]::GetFileName($path)): $Kind package contract holds (build $BuildId)"
exit 0
