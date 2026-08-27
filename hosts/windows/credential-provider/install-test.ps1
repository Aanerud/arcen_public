[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$DllPath,
    [Parameter(Mandatory)][switch]$IUnderstandThisModifiesWinlogon
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'registration-common.ps1')

if (-not $IUnderstandThisModifiesWinlogon) {
    throw 'Pass -IUnderstandThisModifiesWinlogon for an explicit unsigned lab install.'
}

Install-ArcenCredentialProvider -SourceDll $DllPath -Mode Test
Write-Warning 'Installed an unsigned TEST Credential Provider. Roll back with uninstall.ps1 -TestInstall.'
