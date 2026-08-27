$ErrorActionPreference = "Stop"

$manifest = Join-Path $PSScriptRoot "verify-package-manifest.ps1"
$staticRuntime = Join-Path $PSScriptRoot "verify-static-runtime.ps1"
$repositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot "..\..")).Path
$opusVerifier = Join-Path $repositoryRoot "scripts\verify-opusic-source.ps1"
$tempRoot = Join-Path ([IO.Path]::GetTempPath()) ("arcen-package-manifest-" + [Guid]::NewGuid().ToString("N"))
$temp = Join-Path $tempRoot "arcen-windows-x64"
$names = @(
    "INSTALL.md"
    "THIRD_PARTY_NOTICES.md"
    "arcen-cp-harness.exe"
    "arcen-pier.exe"
    "arcen_credential_provider.dll"
    "install-arcen-pier.exe"
    "install-test.ps1"
    "install.ps1"
    "registration-common.ps1"
    "uninstall.ps1"
    "driver\payload\arcen-microphone.cat"
    "driver\payload\arcen-microphone.inf"
    "driver\payload\arcen-microphone.sys"
    "driver\driver-common.ps1"
    "driver\install-driver.ps1"
    "driver\rollback-driver.ps1"
    "driver\uninstall-driver.ps1"
    "driver\upgrade-driver.ps1"
)

function Assert-LiteralPathRejected {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Script,
        [Parameter(Mandatory = $true)]
        [hashtable]$Parameters,
        [Parameter(Mandatory = $true)]
        [string]$SuppliedPath,
        [Parameter(Mandatory = $true)]
        [string]$Label
    )

    $rejected = $false
    try {
        & $Script @Parameters
    }
    catch {
        $rejected = $_.Exception.Message.IndexOf(
            $SuppliedPath,
            [StringComparison]::Ordinal
        ) -ge 0
    }
    if (-not $rejected) {
        throw "$Label accepted wildcard path semantics: $SuppliedPath"
    }
}

New-Item -ItemType Directory -Path $temp -Force | Out-Null
try {
    foreach ($name in $names) {
        $file = Join-Path $temp $name
        New-Item -ItemType Directory -Path (Split-Path -Parent $file) -Force |
            Out-Null
        New-Item -ItemType File -Path $file | Out-Null
    }
    & $manifest -DistributionDirectory $temp

    $driverless = Join-Path $tempRoot "arcen-windows-driverless"
    Copy-Item -LiteralPath $temp -Destination $driverless -Recurse
    Remove-Item -LiteralPath (Join-Path $driverless "driver") -Recurse -Force
    & $manifest -DistributionDirectory $driverless -DriverlessBuild

    Remove-Item -LiteralPath (Join-Path $temp "install-arcen-pier.exe")
    $rejected = $false
    try {
        & $manifest -DistributionDirectory $temp
    }
    catch {
        $rejected = $_.Exception.Message -match "install-arcen-pier\.exe"
    }
    if (-not $rejected) {
        throw "Package manifest verifier accepted a missing required binary"
    }

    New-Item -ItemType File -Path (Join-Path $temp "install-arcen-pier.exe") | Out-Null
    Rename-Item -LiteralPath (Join-Path $temp "INSTALL.md") -NewName "install.md"
    $rejected = $false
    try {
        & $manifest -DistributionDirectory $temp
    }
    catch {
        $rejected = $_.Exception.Message -match "INSTALL\.md"
    }
    if (-not $rejected) {
        throw "Package manifest verifier accepted a case-mismatched required file"
    }

    Rename-Item -LiteralPath (Join-Path $temp "install.md") -NewName "INSTALL.md"
    $hiddenCodec = Join-Path $temp "openh264.dll"
    New-Item -ItemType File -Path $hiddenCodec | Out-Null
    (Get-Item -LiteralPath $hiddenCodec -Force).Attributes = [IO.FileAttributes]::Hidden
    $rejected = $false
    try {
        & $manifest -DistributionDirectory $temp
    }
    catch {
        $rejected = $_.Exception.Message -match "openh264\.dll"
    }
    if (-not $rejected) {
        throw "Package manifest verifier accepted a hidden extra codec payload"
    }

    Remove-Item -LiteralPath $hiddenCodec -Force
    $ignorableName = "INSTALL$([char]0x200B).md"
    New-Item -ItemType File -Path (Join-Path $temp $ignorableName) | Out-Null
    $rejected = $false
    try {
        & $manifest -DistributionDirectory $temp
    }
    catch {
        $rejected = $_.Exception.Message.IndexOf(
            $ignorableName,
            [StringComparison]::Ordinal
        ) -ge 0
    }
    if (-not $rejected) {
        throw "Package manifest verifier accepted an ordinally distinct Unicode extra name"
    }

    Remove-Item -LiteralPath (Join-Path $temp $ignorableName)
    $wildcardDistribution = Join-Path $tempRoot "arcen-windows-x6[4]"
    Assert-LiteralPathRejected `
        -Script $manifest `
        -Parameters @{ DistributionDirectory = $wildcardDistribution } `
        -SuppliedPath $wildcardDistribution `
        -Label "Package manifest verifier"

    $cargoTarget = Join-Path $tempRoot "windows-package"
    New-Item -ItemType Directory -Path $cargoTarget | Out-Null
    $wildcardCargoTarget = Join-Path $tempRoot "windows-packag[e]"
    Assert-LiteralPathRejected `
        -Script $staticRuntime `
        -Parameters @{
            RepositoryRoot = $repositoryRoot
            DistributionDirectory = $temp
            CargoTargetDirectory = $wildcardCargoTarget
        } `
        -SuppliedPath $wildcardCargoTarget `
        -Label "Static-runtime CargoTargetDirectory"

    $repoParent = Split-Path $repositoryRoot -Parent
    $repoLeaf = Split-Path $repositoryRoot -Leaf
    $repoLast = $repoLeaf.Substring($repoLeaf.Length - 1)
    $wildcardRepository = Join-Path $repoParent (
        $repoLeaf.Substring(0, $repoLeaf.Length - 1) + "[$repoLast]"
    )
    Assert-LiteralPathRejected `
        -Script $staticRuntime `
        -Parameters @{
            RepositoryRoot = $wildcardRepository
            DistributionDirectory = $temp
            CargoTargetDirectory = $cargoTarget
        } `
        -SuppliedPath $wildcardRepository `
        -Label "Static-runtime RepositoryRoot"

    Assert-LiteralPathRejected `
        -Script $staticRuntime `
        -Parameters @{
            RepositoryRoot = $repositoryRoot
            DistributionDirectory = $wildcardDistribution
            CargoTargetDirectory = $cargoTarget
        } `
        -SuppliedPath $wildcardDistribution `
        -Label "Static-runtime DistributionDirectory"

    $literalPackage = Join-Path $tempRoot "literal[package]"
    Copy-Item -LiteralPath $temp -Destination $literalPackage -Recurse
    & $manifest -DistributionDirectory $literalPackage

    $literalRepository = Join-Path $tempRoot "repository[root]"
    $linkType = if ([IO.Path]::DirectorySeparatorChar -eq "\") {
        "Junction"
    } else {
        "SymbolicLink"
    }
    New-Item -ItemType $linkType -Path $literalRepository -Target $repositoryRoot | Out-Null
    & $opusVerifier -RepositoryRoot $literalRepository
}
finally {
    Remove-Item -LiteralPath $tempRoot -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host "Verified missing, case-mismatched, hidden, Unicode-distinct, and wildcard-path inputs fail closed."
