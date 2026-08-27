[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$SourceDirectory,
    [Parameter(Mandatory = $true)]
    [string]$DistributionDirectory,
    [Parameter(Mandatory = $true)]
    [string]$RepositoryRoot
)

$ErrorActionPreference = "Stop"
$source = (Resolve-Path -LiteralPath $SourceDirectory -ErrorAction Stop).Path
$distribution = (Resolve-Path -LiteralPath $DistributionDirectory -ErrorAction Stop).Path
$repository = (Resolve-Path -LiteralPath $RepositoryRoot -ErrorAction Stop).Path
$expected = @("arcen-microphone.cat", "arcen-microphone.inf", "arcen-microphone.sys")
$driverSource = Join-Path $repository "hosts\windows\driver\arcen-microphone"
. (Join-Path $driverSource "driver-common.ps1")
$package = Get-DriverPackageIdentity -Directory $source
$target = Join-Path $distribution "driver"
Remove-Item -LiteralPath $target -Recurse -Force -ErrorAction SilentlyContinue
$payload = Join-Path $target "payload"
New-Item -ItemType Directory -Path $payload -Force | Out-Null
foreach ($name in $expected) {
    Copy-Item -LiteralPath (Join-Path $package.Directory $name) -Destination $payload
}
foreach ($name in @(
    "driver-common.ps1"
    "install-driver.ps1"
    "rollback-driver.ps1"
    "uninstall-driver.ps1"
    "upgrade-driver.ps1"
)) {
    Copy-Item -LiteralPath (Join-Path $driverSource $name) -Destination $target
}
Write-Host "Staged protected signed Arcen microphone package."
