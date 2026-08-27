[CmdletBinding()]
param(
    [string]$BackupDirectory = (
        Join-Path $env:ProgramData "Arcen\microphone-driver-backup"
    ),
    [string]$ExpectedVersion,
    [string]$ExpectedInfSha256
)

. (Join-Path $PSScriptRoot "driver-common.ps1")
Assert-Administrator
Assert-ArcenServiceStopped

$metadataPath = Join-Path $BackupDirectory "rollback-state.json"
if (-not $ExpectedVersion -or -not $ExpectedInfSha256) {
    $metadata = Get-Content -LiteralPath $metadataPath -Raw |
        ConvertFrom-Json -ErrorAction Stop
    $ExpectedVersion = [string]$metadata.driver_version
    $ExpectedInfSha256 = [string]$metadata.inf_sha256
}
if ($ExpectedVersion -cnotmatch '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$' -or
    $ExpectedInfSha256 -cnotmatch '^[0-9A-F]{64}$') {
    throw "rollback metadata is malformed"
}

$source = @"
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

public static class ArcenDriverRollback {
    [StructLayout(LayoutKind.Sequential)]
    private struct SP_DEVINFO_DATA {
        public uint cbSize;
        public Guid ClassGuid;
        public uint DevInst;
        public IntPtr Reserved;
    }

    [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr SetupDiGetClassDevs(
        IntPtr classGuid, string enumerator, IntPtr hwndParent, uint flags);
    [DllImport("setupapi.dll", SetLastError = true)]
    private static extern bool SetupDiEnumDeviceInfo(
        IntPtr deviceInfoSet, uint memberIndex, ref SP_DEVINFO_DATA data);
    [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool SetupDiGetDeviceRegistryProperty(
        IntPtr deviceInfoSet, ref SP_DEVINFO_DATA data, uint property,
        out uint propertyType, byte[] buffer, uint bufferSize,
        out uint requiredSize);
    [DllImport("setupapi.dll", SetLastError = true)]
    private static extern bool SetupDiDestroyDeviceInfoList(IntPtr set);
    [DllImport("newdev.dll", SetLastError = true)]
    private static extern bool DiRollbackDriver(
        IntPtr deviceInfoSet, ref SP_DEVINFO_DATA deviceInfoData,
        IntPtr hwndParent, uint flags, out bool needReboot);

    public static bool Rollback(string expectedHardwareId, out bool needReboot) {
        const uint DIGCF_PRESENT = 0x2;
        const uint DIGCF_ALLCLASSES = 0x4;
        const uint SPDRP_HARDWAREID = 0x1;
        const uint ROLLBACK_FLAG_NO_UI = 0x1;
        IntPtr set = SetupDiGetClassDevs(
            IntPtr.Zero, null, IntPtr.Zero,
            DIGCF_PRESENT | DIGCF_ALLCLASSES);
        if (set == new IntPtr(-1)) {
            throw new Win32Exception(Marshal.GetLastWin32Error());
        }
        try {
            SP_DEVINFO_DATA match = new SP_DEVINFO_DATA();
            int matches = 0;
            for (uint index = 0; ; ++index) {
                SP_DEVINFO_DATA data = new SP_DEVINFO_DATA();
                data.cbSize = (uint)Marshal.SizeOf(data);
                if (!SetupDiEnumDeviceInfo(set, index, ref data)) {
                    int error = Marshal.GetLastWin32Error();
                    if (error == 259) {
                        break;
                    }
                    throw new Win32Exception(error);
                }
                byte[] ids = new byte[4096];
                uint propertyType;
                uint required;
                if (!SetupDiGetDeviceRegistryProperty(
                    set, ref data, SPDRP_HARDWAREID, out propertyType,
                    ids, (uint)ids.Length, out required)) {
                    int error = Marshal.GetLastWin32Error();
                    if (error == 13 || error == 1168) {
                        continue;
                    }
                    throw new Win32Exception(error);
                }
                string values = Encoding.Unicode.GetString(
                    ids, 0, checked((int)required));
                foreach (string id in values.Split(new char[] { '\0' },
                         StringSplitOptions.RemoveEmptyEntries)) {
                    if (String.Equals(
                        id, expectedHardwareId,
                        StringComparison.OrdinalIgnoreCase)) {
                        match = data;
                        ++matches;
                        break;
                    }
                }
            }
            if (matches != 1) {
                throw new InvalidOperationException(
                    "expected exactly one Arcen microphone devnode, found " +
                    matches);
            }
            return DiRollbackDriver(
                set, ref match, IntPtr.Zero, ROLLBACK_FLAG_NO_UI,
                out needReboot);
        }
        finally {
            SetupDiDestroyDeviceInfoList(set);
        }
    }
}
"@
if (-not ("ArcenDriverRollback" -as [type])) {
    Add-Type -TypeDefinition $source -ErrorAction Stop
}

function Restore-ExportedPackage {
    $payload = Join-Path $BackupDirectory "payload"
    $package = Get-DriverPackageIdentity `
        -Directory $payload `
        -ExpectedInfSha256 $ExpectedInfSha256
    if ($package.Version -cne $ExpectedVersion -or
        $package.InfSha256 -cne $ExpectedInfSha256) {
        throw "exported rollback package does not match rollback metadata"
    }
    $currentInf = Get-InstalledArcenInf
    foreach ($instanceId in Get-ArcenDeviceInstanceIds) {
        & pnputil.exe /remove-device $instanceId
        if ($LASTEXITCODE -ne 0) {
            throw "failed to remove the current Arcen microphone devnode"
        }
    }
    if ($currentInf) {
        & pnputil.exe /delete-driver $currentInf /uninstall /force
        if ($LASTEXITCODE -ne 0) {
            throw "failed to remove the current Arcen microphone package"
        }
    }
    & pnputil.exe /add-driver (
        Join-Path $package.Directory "arcen-microphone.inf"
    ) /install
    if ($LASTEXITCODE -ne 0) {
        throw "failed to stage the exported rollback package"
    }
    Ensure-ArcenRootDevice
    & pnputil.exe /scan-devices
    if ($LASTEXITCODE -ne 0) {
        throw "failed to rescan after restoring the exported package"
    }
}

$needReboot = $false
$rolledBack = [ArcenDriverRollback]::Rollback(
    $script:HardwareId,
    [ref]$needReboot
)
if (-not $rolledBack) {
    Restore-ExportedPackage
}
$publishedInf = Assert-InstalledDriverIdentity `
    -ExpectedVersion $ExpectedVersion `
    -ExpectedInfSha256 $ExpectedInfSha256
Write-DriverState `
    -PublishedInf $publishedInf `
    -Version $ExpectedVersion `
    -InfSha256 $ExpectedInfSha256
if ($needReboot) {
    Write-Warning "Arcen microphone rollback requires a reboot."
}
else {
    Write-Host "Rolled back Arcen Microphone and verified the prior package."
}
