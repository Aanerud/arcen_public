[CmdletBinding()]
param(
    [switch]$TestInstall,
    [switch]$LegacyArcenInstall
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'registration-common.ps1')

if ($LegacyArcenInstall) {
    Uninstall-LegacyArcenCredentialProvider -TestInstall:$TestInstall
} else {
    Uninstall-ArcenCredentialProvider -TestInstall:$TestInstall
}
Write-Host 'Arcen Credential Provider registration removed. Other providers were not changed.'
