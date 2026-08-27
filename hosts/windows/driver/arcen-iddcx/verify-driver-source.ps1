param(
    [string]$SourceDirectory = $PSScriptRoot
)

$ErrorActionPreference = "Stop"
$source = (Resolve-Path -LiteralPath $SourceDirectory).Path
$manifestPath = Join-Path $source "source-manifest.json"
$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
if ($manifest.schema_version -ne 1) {
    throw "unsupported IddCx source manifest schema"
}

$expected = @{}
foreach ($property in $manifest.files.PSObject.Properties) {
    $expected[$property.Name.Replace("/", "\")] = $property.Value.ToUpperInvariant()
}
$actual = @(
    Get-ChildItem -LiteralPath $source -File -Recurse |
        Where-Object {
            $_.FullName -ne $manifestPath -and
            $_.FullName -notmatch '[\\/](bin|obj)[\\/]'
        } |
        ForEach-Object {
            $_.FullName.Substring($source.Length + 1)
        }
)
$missing = @($expected.Keys | Where-Object { $_ -notin $actual })
$extra = @($actual | Where-Object { $_ -notin $expected.Keys })
if ($missing.Count -ne 0 -or $extra.Count -ne 0) {
    throw "IddCx source manifest mismatch; missing=[$($missing -join ', ')]; extra=[$($extra -join ', ')]"
}
foreach ($relative in $expected.Keys) {
    $path = Join-Path $source $relative
    $hash = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash
    if ($hash -cne $expected[$relative]) {
        throw "IddCx source manifest hash mismatch: $relative"
    }
}

$forbiddenExtensions = @(
    ".cat", ".cer", ".dll", ".exe", ".lib", ".obj", ".p12", ".pdb",
    ".pfx", ".sys"
)
foreach ($path in $actual) {
    if ([IO.Path]::GetExtension($path).ToLowerInvariant() -in $forbiddenExtensions) {
        throw "IddCx source tree contains forbidden payload: $path"
    }
}

$driver = Get-Content -LiteralPath (Join-Path $source "arcen_iddcx_driver.cpp") -Raw
$contract = Get-Content -LiteralPath (Join-Path $source "arcen_iddcx_contract.h") -Raw
$project = Get-Content -LiteralPath (Join-Path $source "arcen-iddcx.vcxproj") -Raw
$inf = Get-Content -LiteralPath (Join-Path $source "arcen-iddcx.inf") -Raw
foreach ($pattern in @(
    "IddCxAdapterInitAsync"
    "IddCxAdapterSetRenderAdapter"
    "IddCxAdapterDisplayConfigUpdate"
    "IddCxMonitorCreate"
    "IddCxMonitorArrival"
    "IddCxMonitorDeparture"
    "IddCxSwapChainReleaseAndAcquireBuffer"
    "IddCxSwapChainFinishedProcessingFrame"
    "WdfDeviceCreateSymbolicLink"
    "WdfDeviceCreateDeviceInterface"
    "WdfRequestRetrieveInputBuffer"
    "WdfRequestRetrieveOutputBuffer"
)) {
    if ($driver.IndexOf($pattern, [StringComparison]::Ordinal) -lt 0) {
        throw "IddCx driver is missing required lifecycle/API behavior: $pattern"
    }
}
foreach ($pattern in @(
    "ARCEN_IDDCX_CAP_DYNAMIC_MONITORS"
    "ARCEN_IDDCX_CAP_RENDER_ADAPTER_AFFINITY"
    "ARCEN_IDDCX_CAP_ATOMIC_REPLACE"
    "ARCEN_IDDCX_CAP_HANDLE_CLEANUP_ROLLBACK"
    "ARCEN_IDDCX_CAP_SWAPCHAIN_DRAIN"
)) {
    if ($contract.IndexOf($pattern, [StringComparison]::Ordinal) -lt 0) {
        throw "IddCx contract is missing strict capability evidence: $pattern"
    }
}
if ($project -notmatch "<SignMode>Off</SignMode>" -or
    $project -match "<EntryPointSymbol>") {
    throw "IddCx project must remain unsigned and use the UMDF-provided entry point"
}
if ($inf -notmatch "UmdfExtensions=IddCx0104" -or
    $inf -notmatch "PnpLockdown=1" -or
    $inf -notmatch 'DeviceGroupId",0x00000000,"ArcenIddCx"' -or
    $inf -notmatch 'D:P\(A;;GA;;;SY\)\(A;;GA;;;BA\)\(A;;GA;;;S-1-5-80-2794664030-2322002993-548807306-4095822587-2900116599\)') {
    throw "IddCx INF is missing its version, lockdown, device-group, or protected-ACL invariant"
}

Write-Host "Verified exact, source-only Arcen IddCx driver manifest and lifecycle invariants."
