[CmdletBinding()]
param()

. (Join-Path $PSScriptRoot "driver-common.ps1")
Assert-Administrator
Assert-ArcenServiceStopped
$publishedInfs = @(Get-ArcenPublishedInfs)
$instanceIds = @(Get-ArcenDeviceInstanceIds)
foreach ($instanceId in $instanceIds) {
    & pnputil.exe /remove-device $instanceId
    if ($LASTEXITCODE -ne 0) {
        throw "pnputil failed to remove Arcen microphone device $instanceId"
    }
}
if ($publishedInfs.Count -eq 0 -and $instanceIds.Count -eq 0) {
    Remove-DriverState
    Write-Host "Arcen Microphone is already absent."
    return
}
foreach ($publishedInf in $publishedInfs) {
    & pnputil.exe /delete-driver $publishedInf /uninstall /force
    if ($LASTEXITCODE -ne 0) {
        throw "pnputil failed to uninstall $publishedInf"
    }
}
if ((Get-ArcenDeviceInstanceIds).Count -ne 0 -or
    @(Get-ArcenPublishedInfs).Count -ne 0) {
    throw "Arcen Microphone devices or owned packages remain after uninstall"
}
Remove-DriverState
Write-Host "Uninstalled Arcen Microphone."
