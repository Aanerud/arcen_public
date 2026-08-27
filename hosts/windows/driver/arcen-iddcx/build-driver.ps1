param(
    [ValidateSet("Debug", "Release")]
    [string]$Configuration = "Release",
    [ValidateSet("x64")]
    [string]$Platform = "x64"
)

$ErrorActionPreference = "Stop"
& (Join-Path $PSScriptRoot "verify-driver-source.ps1")

$vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path -LiteralPath $vswhere)) {
    throw "Visual Studio Installer vswhere.exe is required"
}
$installation = & $vswhere -latest -products * `
    -requires Microsoft.VisualStudio.Component.Windows11SDK.26100 `
    -property installationPath
if (-not $installation) {
    throw "Visual Studio with the Windows 11 SDK/WDK build tools is required"
}
$msbuild = Join-Path $installation "MSBuild\Current\Bin\amd64\MSBuild.exe"
if (-not (Test-Path -LiteralPath $msbuild)) {
    throw "MSBuild amd64 executable was not found"
}

& $msbuild (Join-Path $PSScriptRoot "arcen-iddcx.vcxproj") `
    /m /t:Rebuild /nologo /verbosity:minimal `
    "/p:Configuration=$Configuration" "/p:Platform=$Platform" `
    /p:SignMode=Off
if ($LASTEXITCODE -ne 0) {
    throw "unsigned Arcen IddCx WDK build failed with exit code $LASTEXITCODE"
}

Write-Host "Built unsigned Arcen IddCx artifacts. This script never signs, installs, or deploys them."
