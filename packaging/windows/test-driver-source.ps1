$ErrorActionPreference = "Stop"
$source = (Resolve-Path -LiteralPath (
    Join-Path $PSScriptRoot "..\..\hosts\windows\driver\arcen-microphone"
)).Path
$verifier = Join-Path $source "verify-driver-source.ps1"
$temporary = Join-Path ([IO.Path]::GetTempPath()) (
    "arcen-driver-source-" + [Guid]::NewGuid().ToString("N")
)

& $verifier -SourceDirectory $source
Copy-Item -LiteralPath $source -Destination $temporary -Recurse
try {
    Remove-Item -LiteralPath (Join-Path $temporary "arcen_microphone_driver.cpp")
    $rejected = $false
    try {
        & $verifier -SourceDirectory $temporary
    }
    catch {
        $rejected = $_.Exception.Message -match "manifest mismatch"
    }
    if (-not $rejected) {
        throw "driver verifier accepted a missing implementation"
    }

    Copy-Item -LiteralPath (Join-Path $source "arcen_microphone_driver.cpp") `
        -Destination $temporary
    New-Item -ItemType File -Path (Join-Path $temporary "extra.sys") | Out-Null
    $rejected = $false
    try {
        & $verifier -SourceDirectory $temporary
    }
    catch {
        $rejected = $_.Exception.Message -match "manifest mismatch"
    }
    if (-not $rejected) {
        throw "driver verifier accepted an extra payload"
    }

    Remove-Item -LiteralPath (Join-Path $temporary "extra.sys")
    (Get-Content -LiteralPath (Join-Path $temporary "arcen-microphone.inf") -Raw).
        Replace("CatalogFile=arcen-microphone.cat", "CatalogFile=missing.cat") |
        Set-Content -LiteralPath (Join-Path $temporary "arcen-microphone.inf") `
            -Encoding utf8NoBOM
    $rejected = $false
    try {
        & $verifier -SourceDirectory $temporary
    }
    catch {
        $rejected = $_.Exception.Message -match "CatalogFile"
    }
    if (-not $rejected) {
        throw "driver verifier accepted a mismatched catalog declaration"
    }
}
finally {
    Remove-Item -LiteralPath $temporary -Recurse -Force -ErrorAction SilentlyContinue
}

$driver = Get-Content -LiteralPath (
    Join-Path $source "arcen_microphone_driver.cpp"
) -Raw
$guidHeader = Get-Content -LiteralPath (
    Join-Path $source "arcen_microphone_guids.h"
) -Raw
$ringTest = Get-Content -LiteralPath (
    Join-Path $source "arcen_microphone_ring_test.cpp"
) -Raw
$rollback = Get-Content -LiteralPath (
    Join-Path $source "rollback-driver.ps1"
) -Raw
$common = Get-Content -LiteralPath (
    Join-Path $source "driver-common.ps1"
) -Raw
$inf = Get-Content -LiteralPath (
    Join-Path $source "arcen-microphone.inf"
) -Raw
$build = Get-Content -LiteralPath (
    Join-Path $source "..\..\build.cmd"
) -Raw
$stage = Get-Content -LiteralPath (
    Join-Path $PSScriptRoot "stage-signed-microphone-driver.ps1"
) -Raw
$windowsRustSource = @(
    Get-ChildItem -LiteralPath (Join-Path $source "..\..\src") `
        -Filter "*.rs" -File -Recurse |
        ForEach-Object { Get-Content -LiteralPath $_.FullName -Raw }
) -join "`n"

$requiredDriverPatterns = @(
    '#include <initguid\.h>\s*#include <portcls\.h>'
    '#include "arcen_microphone_guids\.h"'
    'IoGetRequestorSessionId\(Irp, &sessionId\)'
    'CaptureIdentityForCurrentCreate\(&identity\)'
    'ArcenMicrophoneRingAuthorizeReader\('
    'WaveRtStream::QuiesceAll\(\)'
    'IRP_MN_STOP_DEVICE'
    'KeQueryInterruptTimePrecise\(nullptr\)'
    'ARCEN_MAX_CATCHUP_FRAMES'
    '&KSNODETYPE_MICROPHONE,\s*nullptr'
    '(?s)STDMETHODIMP_\(ULONG\) Release\(\).*?KeAcquireSpinLock\(&g_Driver\.StreamLock.*?InterlockedDecrement\(&References_\).*?g_Driver\.Streams'
)
foreach ($pattern in $requiredDriverPatterns) {
    if ($driver -cnotmatch $pattern) {
        throw "driver implementation is missing required production behavior: $pattern"
    }
}
if ($driver -cmatch 'PsGetProcessSessionId') {
    throw "driver uses an undocumented process-session helper"
}
if ($driver -cmatch 'void Unregister\(\)') {
    throw "stream registry removal must be atomic with final reference release"
}
if ($guidHeader -cnotmatch 'DEFINE_GUID\(\s*GUID_DEVINTERFACE_ARCEN_MICROPHONE_CONTROL' -or
    $guidHeader -cnotmatch 'DEFINE_GUID\(\s*GUID_DEVCLASS_ARCEN_MICROPHONE') {
    throw "driver GUID declarations are not instantiated through initguid"
}
foreach ($pattern in @(
    'cross-session reader was not rejected'
    'cross-session reader drained queued audio'
    'stale reader generation accepted'
)) {
    if ($ringTest.IndexOf($pattern, [StringComparison]::Ordinal) -lt 0) {
        throw "production ring test is missing isolation evidence: $pattern"
    }
}
if ($rollback -cnotmatch
    'DiRollbackDriver\(\s*IntPtr deviceInfoSet,\s*ref SP_DEVINFO_DATA deviceInfoData,\s*IntPtr hwndParent,\s*uint flags,\s*out bool needReboot\)' -or
    $rollback -cnotmatch 'ROLLBACK_FLAG_NO_UI = 0x1' -or
    $rollback -cnotmatch 'Restore-ExportedPackage') {
    throw "rollback script does not use the documented ABI and exported fallback"
}
foreach ($pattern in @(
    'certutil\.exe -verifyCatalogFile'
    'Microsoft Windows Hardware Compatibility Publisher'
    '1\.3\.6\.1\.4\.1\.311\.10\.3\.5\.1'
    'ReviewedInfSha256'
    'Assert-InstalledDriverIdentity'
    'Get-ArcenPublishedInfs'
    'ServiceSidType'
)) {
    if ($common -cnotmatch $pattern) {
        throw "driver servicing gate is missing: $pattern"
    }
}
$reviewedHash = [regex]::Match(
    $common,
    'ReviewedInfSha256 = "([0-9A-F]{64})"'
).Groups[1].Value
$actualInfHash = (Get-FileHash -LiteralPath (
    Join-Path $source "arcen-microphone.inf"
) -Algorithm SHA256).Hash
if ($reviewedHash -cne $actualInfHash) {
    throw "servicing gate reviewed INF hash does not match source"
}
foreach ($decoration in @(
    "NTamd64.10.0.1.0.17763"
    "NTamd64.10.0.2.0.20348"
    "NTamd64.10.0.3.0.20348"
)) {
    if ($inf.IndexOf($decoration, [StringComparison]::Ordinal) -lt 0) {
        throw "INF is missing supported target decoration $decoration"
    }
}
if ($build.IndexOf("-DriverlessBuild", [StringComparison]::Ordinal) -lt 0) {
    throw "Windows build does not select the driverless validation manifest"
}
if ($stage -cnotmatch 'Join-Path \$target "payload"' -or
    $stage -cnotmatch 'Get-DriverPackageIdentity') {
    throw "protected driver staging does not isolate and validate its payload"
}
foreach ($forbidden in @(
    'IPolicyConfig'
    'input_default_device'
    'set_default\s*\('
    'microphone_lease'
    'microphone-recording-recovery'
    'WatchdogResource::Microphone'
)) {
    if ($windowsRustSource -cmatch $forbidden) {
        throw "Windows host retains unsupported default-endpoint mutation surface: $forbidden"
    }
}
foreach ($required in @(
    'pub fn backend_available\(\) -> bool'
    'feeder_stop_preempts_queued_audio'
    'ingress_clears_device_exactly_once'
    'self\.output\.zeroize\(\)'
)) {
    if ($windowsRustSource -cnotmatch $required) {
        throw "Windows host is missing endpoint/disable invariants: $required"
    }
}

Write-Host "Verified fail-closed driver source, reader isolation, WaveRT lifecycle, and servicing invariants."
