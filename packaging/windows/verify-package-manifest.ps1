[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$DistributionDirectory,
    [switch]$DriverlessBuild
)

$ErrorActionPreference = "Stop"
function Resolve-FileSystemLiteralPath {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path,
        [Parameter(Mandatory = $true)]
        [string]$Label
    )

    $resolved = @(Resolve-Path -LiteralPath $Path -ErrorAction Stop)
    if ($resolved.Count -ne 1) {
        throw "$Label must resolve to exactly one path: $Path"
    }
    if ($resolved[0].Provider.Name -ne "FileSystem") {
        throw "$Label must use the FileSystem provider: $Path"
    }
    $resolved[0].Path
}

$distributionDirectory = (Resolve-FileSystemLiteralPath `
    -Path $DistributionDirectory `
    -Label "DistributionDirectory").TrimEnd("\", "/")
$expected = @(
    "INSTALL.md"
    "THIRD_PARTY_NOTICES.md"
    "arcen-cp-harness.exe"
    "arcen-pier.exe"
    "arcen_credential_provider.dll"
    "install-arcen-pier.exe"
    "install-test.ps1"
    "install.ps1"
    "registration-common.ps1"
    "uninstall.ps1"
)
if (-not $DriverlessBuild) {
    $expected += @(
        "driver\payload\arcen-microphone.cat"
        "driver\payload\arcen-microphone.inf"
        "driver\payload\arcen-microphone.sys"
        "driver\driver-common.ps1"
        "driver\install-driver.ps1"
        "driver\rollback-driver.ps1"
        "driver\uninstall-driver.ps1"
        "driver\upgrade-driver.ps1"
    )
}
$actual = @(
    Get-ChildItem -LiteralPath $distributionDirectory -File -Recurse -Force |
        ForEach-Object {
            $_.FullName.Substring($distributionDirectory.Length).TrimStart("\", "/").Replace("/", "\")
        }
)
$comparer = [StringComparer]::Ordinal
$expectedSet = [Collections.Generic.HashSet[string]]::new($comparer)
$actualSet = [Collections.Generic.HashSet[string]]::new($comparer)
foreach ($name in $expected) {
    [void]$expectedSet.Add($name)
}
foreach ($name in $actual) {
    [void]$actualSet.Add($name)
}
$missing = @($expected | Where-Object { -not $actualSet.Contains($_) } | Sort-Object)
$unexpected = @($actual | Where-Object { -not $expectedSet.Contains($_) } | Sort-Object)
if ($missing.Count -or $unexpected.Count) {
    throw "Windows package manifest mismatch; missing=[$($missing -join ', ')]; unexpected=[$($unexpected -join ', ')]"
}

Write-Host "Verified exact Windows package manifest ($($expected.Count) files)."
