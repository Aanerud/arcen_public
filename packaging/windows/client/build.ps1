param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^\d+\.\d+\.\d+$')]
    [string]$Version,

    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$Manufacturer,

    [switch]$Sign,

    [switch]$ValidateOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$Target = 'x86_64-pc-windows-msvc'
$PackageDirectory = $PSScriptRoot
$RepositoryRoot = Resolve-Path (Join-Path $PackageDirectory '..\..\..')
$TargetDirectory = Join-Path $RepositoryRoot 'target\arcen-windows-client-package'
$ExecutableDirectory = Join-Path $TargetDirectory "$Target\release"
$Executable = Join-Path $ExecutableDirectory 'arcen-client-windows.exe'
$DistDirectory = Join-Path $PackageDirectory 'dist'
$Output = Join-Path $DistDirectory "arcen-client-windows-$Version-x64.msi"
$RepositoryVersion = (Get-Content (Join-Path $RepositoryRoot 'VERSION') -Raw).Trim()

if ([string]::IsNullOrWhiteSpace($Manufacturer)) {
    throw 'Manufacturer must not be empty or whitespace.'
}
try {
    [System.Xml.XmlConvert]::VerifyXmlChars($Manufacturer) | Out-Null
} catch {
    throw 'Manufacturer contains characters that are not valid in XML.'
}
if ($Manufacturer -match '[<>&"]') {
    throw 'Manufacturer contains XML-reserved characters that are not accepted by this scaffold.'
}
if ($Version -ne $RepositoryVersion) {
    throw "Package version $Version does not match repository version $RepositoryVersion."
}
[xml](Get-Content -LiteralPath (Join-Path $PackageDirectory 'Package.wxs') -Raw) | Out-Null
if ($ValidateOnly) {
    Write-Output 'Windows client packaging inputs are valid.'
    return
}

Push-Location $RepositoryRoot
try {
    cargo build --locked --release --target $Target --target-dir $TargetDirectory --package arcen-client-windows
    $CargoExitCode = $LASTEXITCODE
} finally {
    Pop-Location
}
if ($CargoExitCode -ne 0) {
    throw "Cargo failed with exit code $CargoExitCode"
}

if (-not (Test-Path -LiteralPath $Executable -PathType Leaf)) {
    throw "Expected Windows client executable was not produced at $Executable"
}

if ($Sign) {
    & (Join-Path $PackageDirectory 'sign.ps1') -Artifact $Executable
    if ($LASTEXITCODE -ne 0) {
        throw "Executable signing failed with exit code $LASTEXITCODE"
    }
}

New-Item -ItemType Directory -Force -Path $DistDirectory | Out-Null
wix build `
    (Join-Path $PackageDirectory 'Package.wxs') `
    -arch x64 `
    -d "ArcenVersion=$Version" `
    -d "ArcenManufacturer=$Manufacturer" `
    -d "SourceDir=$ExecutableDirectory" `
    -o $Output

if ($LASTEXITCODE -ne 0) {
    throw "WiX failed with exit code $LASTEXITCODE"
}

if ($Sign) {
    & (Join-Path $PackageDirectory 'sign.ps1') -Artifact $Output
    if ($LASTEXITCODE -ne 0) {
        throw "MSI signing failed with exit code $LASTEXITCODE"
    }
}

Write-Output $Output
