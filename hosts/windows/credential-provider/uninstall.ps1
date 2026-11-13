# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS

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
