[CmdletBinding()]
param(
    [string]$DriverDirectory = (Join-Path $PSScriptRoot "payload")
)

. (Join-Path $PSScriptRoot "driver-common.ps1")
Assert-Administrator
$package = Get-DriverPackageIdentity -Directory $DriverDirectory
Assert-ArcenServiceSid
Assert-ArcenServiceStopped
$inf = Join-Path $package.Directory "arcen-microphone.inf"
& pnputil.exe /add-driver $inf /install
if ($LASTEXITCODE -ne 0) {
    throw "pnputil failed to install the Arcen microphone driver"
}
Ensure-ArcenRootDevice
& pnputil.exe /scan-devices
if ($LASTEXITCODE -ne 0) {
    throw "pnputil failed to rescan after Arcen microphone registration"
}
$publishedInf = Assert-InstalledDriverIdentity `
    -ExpectedVersion $package.Version `
    -ExpectedInfSha256 $package.InfSha256
Write-DriverState `
    -PublishedInf $publishedInf `
    -Version $package.Version `
    -InfSha256 $package.InfSha256
Write-Host "Installed Arcen Microphone from signed package."
