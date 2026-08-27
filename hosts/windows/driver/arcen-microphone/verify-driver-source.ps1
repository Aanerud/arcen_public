[CmdletBinding()]
param(
    [string]$SourceDirectory
)

$ErrorActionPreference = "Stop"
if ([string]::IsNullOrWhiteSpace($SourceDirectory)) {
    $SourceDirectory = $PSScriptRoot
}
$source = (Resolve-Path -LiteralPath $SourceDirectory -ErrorAction Stop).Path
$expected = @(
    "README.md"
    "arcen-microphone.inf"
    "arcen-microphone.vcxproj"
    "arcen_microphone_contract.h"
    "arcen_microphone_driver.cpp"
    "arcen_microphone_guids.h"
    "arcen_microphone_ring.cpp"
    "arcen_microphone_ring.h"
    "arcen_microphone_ring_test.cpp"
    "arcen_microphone_test_shim.h"
    "driver-common.ps1"
    "install-driver.ps1"
    "rollback-driver.ps1"
    "test-driver-portable.cmd"
    "uninstall-driver.ps1"
    "upgrade-driver.ps1"
    "verify-driver-source.ps1"
)
$actual = @(
    Get-ChildItem -LiteralPath $source -File -Force |
        ForEach-Object Name |
        Sort-Object -CaseSensitive
)
$expected = @($expected | Sort-Object -CaseSensitive)
if ([String]::Join("`n", $actual) -cne [String]::Join("`n", $expected)) {
    throw "driver source manifest mismatch; expected=[$($expected -join ', ')]; actual=[$($actual -join ', ')]"
}

$driver = Get-Content -LiteralPath (Join-Path $source "arcen_microphone_driver.cpp") -Raw
$contract = Get-Content -LiteralPath (Join-Path $source "arcen_microphone_contract.h") -Raw
$project = Get-Content -LiteralPath (Join-Path $source "arcen-microphone.vcxproj") -Raw
$inf = Get-Content -LiteralPath (Join-Path $source "arcen-microphone.inf") -Raw
$requiredDriverTokens = @(
    "PcInitializeAdapterDriver"
    "PcAddAdapterDevice"
    "CLSID_PortWaveRT"
    "IMiniportWaveRTStreamNotification"
    "AllocateBufferWithNotification"
    "GetPosition"
    "SetState"
    "IRP_MN_SURPRISE_REMOVAL"
    "IoCreateDeviceSecure"
    "IoRegisterDeviceInterface"
    "RequestorHasServiceSid"
    "ExInitializeDriverRuntime"
    "ExAllocatePoolZero"
    "IsControlDevice"
    "IOCTL_ARCEN_MICROPHONE_BIND"
    "IOCTL_ARCEN_MICROPHONE_FEED"
    "IOCTL_ARCEN_MICROPHONE_STOP"
    "IOCTL_ARCEN_MICROPHONE_STATUS"
    "RtlSecureZeroMemory"
)
foreach ($token in $requiredDriverTokens) {
    if ($driver.IndexOf($token, [StringComparison]::Ordinal) -lt 0) {
        throw "driver source is missing required token: $token"
    }
}
foreach ($token in @(
    "ARCEN_MICROPHONE_SAMPLE_RATE 48000u"
    "ARCEN_MICROPHONE_CHANNELS 1u"
    "ARCEN_MICROPHONE_BITS_PER_SAMPLE 16u"
    "ARCEN_MICROPHONE_FRAME_SAMPLES 960u"
    "ARCEN_MICROPHONE_FEED_REQUEST"
    "ARCEN_MICROPHONE_STATUS_RESPONSE"
)) {
    if ($contract.IndexOf($token, [StringComparison]::Ordinal) -lt 0) {
        throw "driver contract is missing required token: $token"
    }
}
foreach ($file in @("arcen_microphone_driver.cpp", "arcen_microphone_ring.cpp", "arcen-microphone.inf")) {
    if ($project.IndexOf($file, [StringComparison]::Ordinal) -lt 0) {
        throw "WDK project does not include $file"
    }
}
if ($driver.IndexOf("ExAllocatePoolWithTag", [StringComparison]::Ordinal) -ge 0) {
    throw "driver source uses a deprecated pool allocation API"
}
foreach ($token in @(
    "<WindowsTargetPlatformVersion>10.0.26100.0</WindowsTargetPlatformVersion>"
    "<Driver_SpectreMitigation>Spectre</Driver_SpectreMitigation>"
    "<RuntimeLibrary>MultiThreaded</RuntimeLibrary>"
    "<SpecifyDriverVerDirectiveDate>false</SpecifyDriverVerDirectiveDate>"
    "<SpecifyDriverVerDirectiveVersion>false</SpecifyDriverVerDirectiveVersion>"
    '<FilesToPackage Include="$(TargetPath)" />'
)) {
    if ($project.IndexOf($token, [StringComparison]::Ordinal) -lt 0) {
        throw "WDK project is missing required token: $token"
    }
}
foreach ($token in @(
    "Class=MEDIA"
    "CatalogFile=arcen-microphone.cat"
    "ROOT\ARCENMICROPHONE"
    "Include=ks.inf,wdmaudio.inf"
    "PnpLockdown=1"
    "DriverCopy=13"
    "ServiceBinary=%13%\arcen-microphone.sys"
    "NTamd64.10.0.1.0.17763"
)) {
    if ($inf.IndexOf($token, [StringComparison]::Ordinal) -lt 0) {
        throw "driver INF is missing required token: $token"
    }
}
$servicingRequirements = @{
    "driver-common.ps1" = @(
        "ExpectedServiceSid"
        "Get-AuthenticodeSignature"
        "Get-InstalledArcenInf"
        "SetupDiCreateDeviceInfo"
        "SetupDiSetDeviceRegistryPropertyW"
        "SetupDiCallClassInstaller"
        "Assert-ArcenServiceStopped"
    )
    "install-driver.ps1" = @(
        "/add-driver"
        "Assert-ArcenServiceSid"
        "Assert-ArcenServiceStopped"
        "Ensure-ArcenRootDevice"
        "Write-DriverState"
    )
    "uninstall-driver.ps1" = @(
        "/delete-driver"
        "/remove-device"
        "/uninstall"
        "Assert-ArcenServiceStopped"
        "Remove-DriverState"
    )
    "upgrade-driver.ps1" = @(
        "/export-driver"
        "Assert-ArcenServiceStopped"
        "rollback-driver.ps1"
    )
    "rollback-driver.ps1" = @(
        "DiRollbackDriver"
        "newdev.dll"
        "needReboot"
        "Assert-ArcenServiceStopped"
    )
}
foreach ($entry in $servicingRequirements.GetEnumerator()) {
    $text = Get-Content -LiteralPath (Join-Path $source $entry.Key) -Raw
    foreach ($token in $entry.Value) {
        if ($text.IndexOf($token, [StringComparison]::Ordinal) -lt 0) {
            throw "$($entry.Key) is missing servicing requirement: $token"
        }
    }
}
$implementationFiles = @(
    "arcen_microphone_contract.h"
    "arcen_microphone_driver.cpp"
    "arcen_microphone_guids.h"
    "arcen_microphone_ring.cpp"
    "arcen_microphone_ring.h"
)
foreach ($file in $implementationFiles) {
    $text = Get-Content -LiteralPath (Join-Path $source $file) -Raw
    foreach ($prohibited in @("sysvad", "virtual audio cable", "wdk sample")) {
        if ($text.IndexOf($prohibited, [StringComparison]::OrdinalIgnoreCase) -ge 0) {
            throw "$file contains prohibited imported-source marker: $prohibited"
        }
    }
}
Write-Host "Verified exact independently authored Arcen microphone driver source manifest."
