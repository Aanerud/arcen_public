Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$script:DriverFiles = @(
    "arcen-microphone.cat"
    "arcen-microphone.inf"
    "arcen-microphone.sys"
)
$script:HardwareId = "ROOT\ARCENMICROPHONE"
$script:ServiceName = "ArcenPier"
$script:ExpectedServiceSid = "S-1-5-80-2794664030-2322002993-548807306-4095822587-2900116599"
$script:ReviewedInfSha256 = "61C72B6511A4E807C55BD2EEA279EECF08F201CD601CD84E28A16B400B01782A"
$script:StatePath = Join-Path (
    [Environment]::GetFolderPath([Environment+SpecialFolder]::CommonApplicationData)
) "Arcen\microphone-driver.json"

function Assert-Administrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "Arcen microphone servicing requires an elevated administrator shell"
    }
}

function Resolve-DriverPackage {
    param(
        [Parameter(Mandatory = $true)][string]$Directory,
        [string]$ExpectedInfSha256 = $script:ReviewedInfSha256
    )
    $resolved = (Resolve-Path -LiteralPath $Directory -ErrorAction Stop).Path
    $actual = @(
        Get-ChildItem -LiteralPath $resolved -File -Force |
            ForEach-Object Name |
            Sort-Object -CaseSensitive
    )
    $expected = @($script:DriverFiles | Sort-Object -CaseSensitive)
    if ([String]::Join("`n", $actual) -cne [String]::Join("`n", $expected)) {
        throw "driver package must contain exactly: $($expected -join ', ')"
    }
    $inf = Join-Path $resolved "arcen-microphone.inf"
    $infHash = (Get-FileHash -LiteralPath $inf -Algorithm SHA256).Hash
    if ($infHash -cne $ExpectedInfSha256) {
        throw "arcen-microphone.inf does not match the reviewed source hash"
    }

    $catalog = Join-Path $resolved "arcen-microphone.cat"
    $signature = Get-AuthenticodeSignature -LiteralPath $catalog
    if ($signature.Status -ne [Management.Automation.SignatureStatus]::Valid -or
        $null -eq $signature.SignerCertificate) {
        throw "arcen-microphone.cat has no valid production signature (status $($signature.Status))"
    }
    $subject = [string]$signature.SignerCertificate.Subject
    if ($subject -notmatch "(^|,\s*)CN=Microsoft Windows Hardware Compatibility Publisher(,|$)") {
        throw "arcen-microphone.cat is not signed by Microsoft Windows Hardware Compatibility Publisher"
    }
    $ekuOids = @(
        $signature.SignerCertificate.Extensions |
            Where-Object {
                $_ -is [Security.Cryptography.X509Certificates.X509EnhancedKeyUsageExtension]
            } |
            ForEach-Object { $_.EnhancedKeyUsages } |
            ForEach-Object { $_.Value }
    )
    if ($ekuOids -contains "1.3.6.1.4.1.311.10.3.5.1" -or
        -not (
            $ekuOids -contains "1.3.6.1.4.1.311.10.3.5" -or
            $ekuOids -contains "1.3.6.1.4.1.311.10.3.7"
        )) {
        throw "arcen-microphone.cat is not WHCP/WHQL production signed"
    }
    foreach ($name in @("arcen-microphone.inf", "arcen-microphone.sys")) {
        & certutil.exe -verifyCatalogFile $catalog (Join-Path $resolved $name) |
            Out-Null
        if ($LASTEXITCODE -ne 0) {
            throw "$name is not covered by the protected production catalog"
        }
    }
    $resolved
}

function Get-DriverPackageIdentity {
    param(
        [Parameter(Mandatory = $true)][string]$Directory,
        [string]$ExpectedInfSha256 = $script:ReviewedInfSha256
    )
    $resolved = Resolve-DriverPackage `
        -Directory $Directory `
        -ExpectedInfSha256 $ExpectedInfSha256
    $inf = Join-Path $resolved "arcen-microphone.inf"
    $driverVer = @(
        Select-String -LiteralPath $inf `
            -Pattern '^DriverVer=[^,]+,([0-9]+\.[0-9]+\.[0-9]+\.[0-9]+)$' `
            -CaseSensitive
    )
    if ($driverVer.Count -ne 1) {
        throw "arcen-microphone.inf must contain one exact DriverVer"
    }
    [pscustomobject]@{
        Directory = $resolved
        Version = $driverVer[0].Matches[0].Groups[1].Value
        InfSha256 = (Get-FileHash -LiteralPath $inf -Algorithm SHA256).Hash
    }
}

function Assert-ArcenServiceSid {
    $output = (& sc.exe showsid $script:ServiceName 2>&1 | Out-String)
    if ($LASTEXITCODE -ne 0 -or
        $output.IndexOf($script:ExpectedServiceSid, [StringComparison]::Ordinal) -lt 0) {
        throw "unexpected $($script:ServiceName) service SID"
    }
    $service = Get-CimInstance Win32_Service -Filter "Name = '$($script:ServiceName)'"
    if ($null -eq $service) {
        throw "$($script:ServiceName) must be installed before the microphone driver"
    }
    $serviceKey = "HKLM:\SYSTEM\CurrentControlSet\Services\$($script:ServiceName)"
    $sidType = (Get-ItemProperty -LiteralPath $serviceKey `
        -Name ServiceSidType -ErrorAction SilentlyContinue).ServiceSidType
    if ($sidType -ne 1) {
        if ([string]$service.State -ne "Stopped") {
            throw "$($script:ServiceName) must be stopped before enabling its service SID"
        }
        & sc.exe sidtype $script:ServiceName unrestricted | Out-Null
        if ($LASTEXITCODE -ne 0) {
            throw "failed to enable the $($script:ServiceName) service SID"
        }
        $sidType = (Get-ItemProperty -LiteralPath $serviceKey `
            -Name ServiceSidType -ErrorAction Stop).ServiceSidType
        if ($sidType -ne 1) {
            throw "$($script:ServiceName) service SID type was not persisted"
        }
    }
}

function Assert-ArcenServiceStopped {
    $service = Get-Service -Name $script:ServiceName -ErrorAction Stop
    if ($service.Status -ne [ServiceProcess.ServiceControllerStatus]::Stopped) {
        throw "$($script:ServiceName) must be stopped before microphone driver servicing"
    }
}

function Get-InstalledArcenDriver {
    $drivers = @(
        Get-CimInstance Win32_PnPSignedDriver |
            Where-Object {
                [string]$_.HardWareID -ieq $script:HardwareId
            }
    )
    if ($drivers.Count -gt 1) {
        throw "multiple installed Arcen microphone devices were found"
    }
    if ($drivers.Count -eq 0) {
        return $null
    }
    $inf = [string]$drivers[0].InfName
    if ($inf -cnotmatch '^oem[0-9]+\.inf$') {
        throw "Windows returned an invalid published INF name"
    }
    $drivers[0]
}

function Get-InstalledArcenInf {
    $driver = Get-InstalledArcenDriver
    if ($null -eq $driver) {
        return $null
    }
    [string]$driver.InfName
}

function Assert-InstalledDriverIdentity {
    param(
        [Parameter(Mandatory = $true)][string]$ExpectedVersion,
        [Parameter(Mandatory = $true)][string]$ExpectedInfSha256
    )
    $driver = Get-InstalledArcenDriver
    if ($null -eq $driver) {
        throw "Arcen microphone installation completed without a discoverable device"
    }
    if ([string]$driver.DriverVersion -cne $ExpectedVersion) {
        throw "installed Arcen microphone version $($driver.DriverVersion) does not match requested $ExpectedVersion"
    }
    $publishedInf = [string]$driver.InfName
    $publishedPath = Join-Path (Join-Path $env:windir "INF") $publishedInf
    $publishedHash = (Get-FileHash -LiteralPath $publishedPath -Algorithm SHA256).Hash
    if ($publishedHash -cne $ExpectedInfSha256) {
        throw "installed $publishedInf does not match the requested reviewed INF"
    }
    $publishedInf
}

function Get-ArcenPublishedInfs {
    $owned = @(
        Get-WindowsDriver -Online -All |
            Where-Object {
                [string]$_.ProviderName -ceq "Arcen" -and
                [string]$_.ClassName -ceq "MEDIA" -and
                [IO.Path]::GetFileName([string]$_.OriginalFileName) -ceq
                    "arcen-microphone.inf"
            } |
            ForEach-Object { [string]$_.Driver }
    )
    $stateInf = Get-InstalledArcenInf
    if ($stateInf) {
        $owned += $stateInf
    }
    @(
        $owned |
            Where-Object { $_ -cmatch '^oem[0-9]+\.inf$' } |
            Sort-Object -Unique
    )
}

function Get-ArcenDeviceInstanceIds {
    @(
        Get-CimInstance Win32_PnPEntity |
            Where-Object {
                @($_.HardwareID) -icontains $script:HardwareId
            } |
            ForEach-Object { [string]$_.PNPDeviceID }
    )
}

function Ensure-ArcenRootDevice {
    $existing = @(Get-ArcenDeviceInstanceIds)
    if ($existing.Count -gt 1) {
        throw "multiple Arcen microphone root devices were found"
    }
    if ($existing.Count -eq 1) {
        return
    }
    if (-not ("ArcenRootDevice" -as [type])) {
        $source = @"
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
public static class ArcenRootDevice {
    [StructLayout(LayoutKind.Sequential)]
    private struct SP_DEVINFO_DATA {
        public uint cbSize;
        public Guid ClassGuid;
        public uint DevInst;
        public IntPtr Reserved;
    }
    [DllImport("setupapi.dll", SetLastError = true)]
    private static extern IntPtr SetupDiCreateDeviceInfoList(
        ref Guid classGuid, IntPtr hwndParent);
    [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool SetupDiCreateDeviceInfo(
        IntPtr deviceInfoSet, string deviceName, ref Guid classGuid,
        string deviceDescription, IntPtr hwndParent, uint creationFlags,
        ref SP_DEVINFO_DATA deviceInfoData);
    [DllImport(
        "setupapi.dll",
        EntryPoint = "SetupDiSetDeviceRegistryPropertyW",
        ExactSpelling = true,
        SetLastError = true)]
    private static extern bool SetupDiSetDeviceRegistryProperty(
        IntPtr deviceInfoSet, ref SP_DEVINFO_DATA deviceInfoData,
        uint property, byte[] propertyBuffer, uint propertyBufferSize);
    [DllImport("setupapi.dll", SetLastError = true)]
    private static extern bool SetupDiCallClassInstaller(
        uint installFunction, IntPtr deviceInfoSet,
        ref SP_DEVINFO_DATA deviceInfoData);
    [DllImport("setupapi.dll", SetLastError = true)]
    private static extern bool SetupDiDestroyDeviceInfoList(IntPtr deviceInfoSet);

    public static void Create() {
        Guid mediaClass = new Guid("4d36e96c-e325-11ce-bfc1-08002be10318");
        IntPtr set = SetupDiCreateDeviceInfoList(ref mediaClass, IntPtr.Zero);
        if (set == new IntPtr(-1)) {
            throw new Win32Exception(Marshal.GetLastWin32Error());
        }
        try {
            SP_DEVINFO_DATA data = new SP_DEVINFO_DATA();
            data.cbSize = (uint)Marshal.SizeOf(data);
            if (!SetupDiCreateDeviceInfo(
                set, "ARCENMICROPHONE", ref mediaClass, null, IntPtr.Zero,
                1, ref data)) {
                throw new Win32Exception(Marshal.GetLastWin32Error());
            }
            byte[] hardwareId = System.Text.Encoding.Unicode.GetBytes(
                "ROOT\\ARCENMICROPHONE\0\0");
            if (!SetupDiSetDeviceRegistryProperty(
                set, ref data, 1, hardwareId, (uint)hardwareId.Length)) {
                throw new Win32Exception(Marshal.GetLastWin32Error());
            }
            if (!SetupDiCallClassInstaller(0x19, set, ref data)) {
                throw new Win32Exception(Marshal.GetLastWin32Error());
            }
        }
        finally {
            SetupDiDestroyDeviceInfoList(set);
        }
    }
}
"@
        Add-Type -TypeDefinition $source -ErrorAction Stop
    }
    [ArcenRootDevice]::Create()
    for ($attempt = 0; $attempt -lt 25; ++$attempt) {
        $created = @(Get-ArcenDeviceInstanceIds)
        if ($created.Count -gt 1) {
            throw "microphone registration created multiple root devices"
        }
        if ($created.Count -eq 1) {
            return
        }
        Start-Sleep -Milliseconds 200
    }
    throw "microphone root device hardware ID was not registered"
}

function Write-DriverState {
    param(
        [Parameter(Mandatory = $true)][string]$PublishedInf,
        [Parameter(Mandatory = $true)][string]$Version,
        [Parameter(Mandatory = $true)][string]$InfSha256
    )
    $parent = Split-Path -Parent $script:StatePath
    New-Item -ItemType Directory -Path $parent -Force | Out-Null
    $temporary = "$script:StatePath.tmp-$PID"
    @{
        version = 1
        published_inf = $PublishedInf
        driver_version = $Version
        inf_sha256 = $InfSha256
    } | ConvertTo-Json | Set-Content -LiteralPath $temporary -Encoding utf8NoBOM
    Move-Item -LiteralPath $temporary -Destination $script:StatePath -Force
}

function Remove-DriverState {
    Remove-Item -LiteralPath $script:StatePath -Force -ErrorAction SilentlyContinue
}
