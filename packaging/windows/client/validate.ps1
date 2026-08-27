[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$PackageDirectory = $PSScriptRoot
$Scripts = @(
    (Join-Path $PackageDirectory 'build.ps1'),
    (Join-Path $PackageDirectory 'sign.ps1'),
    $MyInvocation.MyCommand.Path
)

foreach ($Script in $Scripts) {
    $Tokens = $null
    $Errors = $null
    [System.Management.Automation.Language.Parser]::ParseFile(
        $Script,
        [ref]$Tokens,
        [ref]$Errors
    ) | Out-Null
    if ($Errors.Count -ne 0) {
        throw "PowerShell parser rejected $Script`: $($Errors[0].Message)"
    }
}

$BuildScript = Join-Path $PackageDirectory 'build.ps1'
$RepositoryRoot = Resolve-Path (Join-Path $PackageDirectory '..\..\..')
$Version = (Get-Content (Join-Path $RepositoryRoot 'VERSION') -Raw).Trim()
& $BuildScript -Version $Version -Manufacturer 'Arcen Validation' -ValidateOnly

foreach ($InvalidManufacturer in @('Arcen & Co', 'Arcen <Client>', 'Arcen "Client"')) {
    try {
        & $BuildScript -Version $Version -Manufacturer $InvalidManufacturer -ValidateOnly
        throw 'build.ps1 accepted an XML-unsafe Manufacturer value.'
    } catch {
        if ($_.Exception.Message -eq 'build.ps1 accepted an XML-unsafe Manufacturer value.') {
            throw
        }
    }
}

$PackagePath = Join-Path $PackageDirectory 'Package.wxs'
[xml]$Package = Get-Content -LiteralPath $PackagePath -Raw
$Namespace = [System.Xml.XmlNamespaceManager]::new($Package.NameTable)
$Namespace.AddNamespace('w', 'http://wixtoolset.org/schemas/v4/wxs')
$PackageElement = $Package.SelectSingleNode('/w:Wix/w:Package', $Namespace)
$Executable = $Package.SelectSingleNode(
    "/w:Wix/w:Package/w:StandardDirectory/w:Directory/w:Component/w:File[@Id='ArcenClientWindowsExe']",
    $Namespace
)

if ($null -eq $PackageElement -or $PackageElement.Scope -ne 'perMachine') {
    throw 'Package.wxs must define a per-machine MSI package.'
}
if ($null -eq $Executable -or $Executable.Source -ne '$(var.SourceDir)\arcen-client-windows.exe') {
    throw 'Package.wxs must package the explicitly supplied Windows client executable.'
}

$SignScriptText = Get-Content -LiteralPath (Join-Path $PackageDirectory 'sign.ps1') -Raw
if ($SignScriptText -notmatch "'https://timestamp\.digicert\.com'") {
    throw 'sign.ps1 must use an HTTPS timestamp default.'
}

$BuildScriptText = Get-Content -LiteralPath $BuildScript -Raw
$ExecutableSign = $BuildScriptText.IndexOf(
    '& (Join-Path $PackageDirectory ''sign.ps1'') -Artifact $Executable'
)
$WixBuild = $BuildScriptText.IndexOf('wix build')
$MsiSign = $BuildScriptText.IndexOf(
    '& (Join-Path $PackageDirectory ''sign.ps1'') -Artifact $Output'
)
if (
    $ExecutableSign -lt 0 -or
    $WixBuild -lt 0 -or
    $MsiSign -lt 0 -or
    -not ($ExecutableSign -lt $WixBuild -and $WixBuild -lt $MsiSign)
) {
    throw 'build.ps1 must preserve EXE signing before WiX and MSI signing after WiX.'
}

Write-Output 'Windows client packaging scaffold validation passed.'
