[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$DllPath,
    [Parameter(Mandatory)][string]$BuildTarget,
    [Parameter(Mandatory)]
    [ValidatePattern('^[A-Fa-f0-9]{40}$')]
    [string]$ExpectedSignerThumbprint
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'registration-common.ps1')

if ($BuildTarget -ne 'x86_64-pc-windows-msvc') {
    throw 'Production installation requires BuildTarget=x86_64-pc-windows-msvc.'
}

$resolvedDll = (Resolve-Path -LiteralPath $DllPath).Path
$signature = Get-AuthenticodeSignature -LiteralPath $resolvedDll
if ($signature.Status -ne [System.Management.Automation.SignatureStatus]::Valid -or
    $null -eq $signature.SignerCertificate) {
    throw "Production installation requires a valid Authenticode signature (status: $($signature.Status))."
}
$expectedSigner = $ExpectedSignerThumbprint.Replace(' ', '').ToUpperInvariant()
$actualSigner = $signature.SignerCertificate.Thumbprint.Replace(' ', '').ToUpperInvariant()
if ($actualSigner -ne $expectedSigner) {
    throw "Authenticode signer mismatch (actual thumbprint: $actualSigner)."
}

Install-ArcenCredentialProvider -SourceDll $resolvedDll -Mode Production
Write-Host "Installed signed Arcen Credential Provider at $script:DestinationDll"
