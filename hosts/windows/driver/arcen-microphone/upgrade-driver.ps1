[CmdletBinding()]
param(
    [string]$DriverDirectory = (Join-Path $PSScriptRoot "payload"),
    [string]$BackupDirectory = (
        Join-Path $env:ProgramData "Arcen\microphone-driver-backup"
    )
)

. (Join-Path $PSScriptRoot "driver-common.ps1")
Assert-Administrator
Assert-ArcenServiceStopped
$currentDriver = Get-InstalledArcenDriver
if ($currentDriver) {
    $currentInf = [string]$currentDriver.InfName
    $currentVersion = [string]$currentDriver.DriverVersion
    $currentInfPath = Join-Path (Join-Path $env:windir "INF") $currentInf
    $currentInfSha256 = (
        Get-FileHash -LiteralPath $currentInfPath -Algorithm SHA256
    ).Hash
    Remove-Item -LiteralPath $BackupDirectory -Recurse -Force -ErrorAction SilentlyContinue
    $backupExport = Join-Path $BackupDirectory "export"
    New-Item -ItemType Directory -Path $backupExport -Force | Out-Null
    & pnputil.exe /export-driver $currentInf $backupExport
    if ($LASTEXITCODE -ne 0) {
        throw "failed to export current Arcen microphone package before upgrade"
    }
    $exportedInfs = @(
        Get-ChildItem -LiteralPath $backupExport `
            -Filter "arcen-microphone.inf" -File -Recurse
    )
    if ($exportedInfs.Count -ne 1) {
        throw "driver export produced $($exportedInfs.Count) Arcen INF files"
    }
    $exported = Get-DriverPackageIdentity `
        -Directory $exportedInfs[0].DirectoryName `
        -ExpectedInfSha256 $currentInfSha256
    if ($exported.Version -cne $currentVersion -or
        $exported.InfSha256 -cne $currentInfSha256) {
        throw "exported rollback package does not match the installed driver"
    }
    $backupPayload = Join-Path $BackupDirectory "payload"
    New-Item -ItemType Directory -Path $backupPayload -Force | Out-Null
    foreach ($name in $script:DriverFiles) {
        Copy-Item -LiteralPath (Join-Path $exported.Directory $name) `
            -Destination $backupPayload
    }
    Remove-Item -LiteralPath $backupExport -Recurse -Force
    @{
        version = 1
        driver_version = $currentVersion
        inf_sha256 = $currentInfSha256
    } | ConvertTo-Json | Set-Content `
        -LiteralPath (Join-Path $BackupDirectory "rollback-state.json") `
        -Encoding utf8NoBOM
}
try {
    & (Join-Path $PSScriptRoot "install-driver.ps1") -DriverDirectory $DriverDirectory
}
catch {
    if ($currentDriver) {
        & (Join-Path $PSScriptRoot "rollback-driver.ps1") `
            -BackupDirectory $BackupDirectory `
            -ExpectedVersion $currentVersion `
            -ExpectedInfSha256 $currentInfSha256
    }
    throw
}
