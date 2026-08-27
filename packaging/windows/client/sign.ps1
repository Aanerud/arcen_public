param(
    [Parameter(Mandatory = $true)]
    [ValidateScript({ Test-Path -LiteralPath $_ -PathType Leaf })]
    [string]$Artifact
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not $env:ARCEN_SIGNTOOL_PATH) {
    throw 'ARCEN_SIGNTOOL_PATH must identify the approved signtool.exe.'
}
if (-not (Test-Path -LiteralPath $env:ARCEN_SIGNTOOL_PATH -PathType Leaf)) {
    throw 'ARCEN_SIGNTOOL_PATH must identify an existing signtool.exe.'
}
if (-not $env:ARCEN_SIGNING_CERT_THUMBPRINT) {
    throw 'ARCEN_SIGNING_CERT_THUMBPRINT must identify the approved signing certificate.'
}

$TimestampUrl = if ($env:ARCEN_TIMESTAMP_URL) {
    $env:ARCEN_TIMESTAMP_URL
} else {
    'https://timestamp.digicert.com'
}
$TimestampUri = $null
if (
    -not [Uri]::TryCreate($TimestampUrl, [UriKind]::Absolute, [ref]$TimestampUri) -or
    $TimestampUri.Scheme -notin @('https', 'http')
) {
    throw 'ARCEN_TIMESTAMP_URL must be an absolute HTTP(S) URL.'
}
if (
    $TimestampUri.Scheme -eq 'http' -and
    $env:ARCEN_ALLOW_INSECURE_TIMESTAMP -ne '1'
) {
    throw 'HTTP timestamping requires ARCEN_ALLOW_INSECURE_TIMESTAMP=1.'
}

$SignTool = (Resolve-Path -LiteralPath $env:ARCEN_SIGNTOOL_PATH).Path
$ResolvedArtifact = (Resolve-Path -LiteralPath $Artifact).Path

& $SignTool sign `
    /sha1 $env:ARCEN_SIGNING_CERT_THUMBPRINT `
    /fd SHA256 `
    /tr $TimestampUri.AbsoluteUri `
    /td SHA256 `
    $ResolvedArtifact

if ($LASTEXITCODE -ne 0) {
    throw "signtool failed with exit code $LASTEXITCODE"
}

& $SignTool verify /pa /v $ResolvedArtifact

if ($LASTEXITCODE -ne 0) {
    throw "signtool verification failed with exit code $LASTEXITCODE"
}
