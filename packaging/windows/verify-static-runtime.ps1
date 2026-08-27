[CmdletBinding()]
param(
    [string]$RepositoryRoot,
    [string]$DistributionDirectory,
    [Parameter(Mandatory = $true)]
    [string]$CargoTargetDirectory,
    [switch]$DriverlessBuild
)

$ErrorActionPreference = "Stop"

function Resolve-FileSystemLiteralPath {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path,
        [Parameter(Mandatory = $true)]
        [string]$Label
    )

    $resolved = @(Resolve-Path -LiteralPath $Path -ErrorAction Stop)
    if ($resolved.Count -ne 1) {
        throw "$Label must resolve to exactly one path: $Path"
    }
    if ($resolved[0].Provider.Name -ne "FileSystem") {
        throw "$Label must use the FileSystem provider: $Path"
    }
    $resolved[0].Path
}

if (-not $RepositoryRoot) {
    $RepositoryRoot = Join-Path $PSScriptRoot "..\.."
}
$repositoryRoot = Resolve-FileSystemLiteralPath -Path $RepositoryRoot -Label "RepositoryRoot"
$cargoTargetDirectory = Resolve-FileSystemLiteralPath `
    -Path $CargoTargetDirectory `
    -Label "CargoTargetDirectory"
$releaseBuildDirectory = Join-Path $cargoTargetDirectory "release\build"
if (-not $DistributionDirectory) {
    $DistributionDirectory = Join-Path $repositoryRoot "target\arcen-windows-x64"
}
$distributionDirectory = Resolve-FileSystemLiteralPath `
    -Path $DistributionDirectory `
    -Label "DistributionDirectory"

& (Join-Path $PSScriptRoot "verify-package-manifest.ps1") `
    -DistributionDirectory $distributionDirectory `
    -DriverlessBuild:$DriverlessBuild

& (Join-Path $repositoryRoot "scripts\verify-opusic-source.ps1") -RepositoryRoot $repositoryRoot

foreach ($tool in "lib.exe", "dumpbin.exe") {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        throw "$tool is required; run from an x64 Visual Studio Developer Command Prompt"
    }
}

$opusArchives = @(
    Get-ChildItem -LiteralPath $releaseBuildDirectory `
        -Filter "opus.lib" -File -Recurse |
        Where-Object {
            $_.FullName -match "[\\/]opusic-sys-[^\\/]+[\\/]out[\\/]lib[\\/]opus\.lib$"
        }
)
if ($opusArchives.Count -ne 1) {
    throw "Expected exactly one installed release opus.lib, found $($opusArchives.Count)"
}
$opusArchive = $opusArchives[0]
$buildRoot = Split-Path $opusArchive.DirectoryName -Parent
$cachePath = Join-Path $buildRoot "build\CMakeCache.txt"
$projectPath = Join-Path $buildRoot "build\opus.vcxproj"
$ninjaPath = Join-Path $buildRoot "build\build.ninja"

if (-not (Select-String -LiteralPath $cachePath -Pattern "^OPUS_STATIC_RUNTIME:BOOL=ON$" -Quiet)) {
    throw "CMake cache does not prove OPUS_STATIC_RUNTIME=ON: $cachePath"
}
if (Test-Path -LiteralPath $projectPath -PathType Leaf) {
    $runtimeValues = @(
        Select-String -LiteralPath $projectPath -Pattern "<RuntimeLibrary>([^<]+)</RuntimeLibrary>" |
            ForEach-Object { $_.Matches[0].Groups[1].Value }
    )
    if (-not $runtimeValues.Count -or @($runtimeValues | Where-Object { $_ -ne "MultiThreaded" }).Count) {
        throw "Generated Opus project does not exclusively use CMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded"
    }
}
elseif (Test-Path -LiteralPath $ninjaPath -PathType Leaf) {
    $flagLines = @(Select-String -LiteralPath $ninjaPath -Pattern "^\s*FLAGS = ")
    if (-not $flagLines.Count) {
        throw "Generated Opus Ninja build has no compiler flag records"
    }
    foreach ($line in $flagLines.Line) {
        if ($line -notmatch "(?:^|\s)[/-]MT(?:\s|$)" -or $line -match "(?:^|\s)[/-]MDd?(?:\s|$)") {
            throw "Generated Opus Ninja build does not exclusively use CMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded: $line"
        }
    }
}
else {
    throw "Could not find generated Opus MSVC project or Ninja build evidence"
}

$memberNames = @(& lib.exe /nologo /list $opusArchive.FullName)
if ($LASTEXITCODE -ne 0 -or -not $memberNames.Count) {
    throw "Could not enumerate opus.lib members"
}

$extractRoot = Join-Path ([IO.Path]::GetTempPath()) ("arcen-opus-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $extractRoot | Out-Null
try {
    for ($index = 0; $index -lt $memberNames.Count; $index++) {
        $member = $memberNames[$index].Trim()
        if (-not $member) {
            continue
        }
        $objectPath = Join-Path $extractRoot ("member-{0:D4}.obj" -f $index)
        & lib.exe /nologo "/extract:$member" "/out:$objectPath" $opusArchive.FullName
        if ($LASTEXITCODE -ne 0) {
            throw "Could not extract opus.lib member: $member"
        }
        $directives = (& dumpbin.exe /nologo /directives $objectPath) -join "`n"
        if ($LASTEXITCODE -ne 0) {
            throw "Could not inspect opus.lib member: $member"
        }
        if ($directives -notmatch "(?i)DEFAULTLIB:LIBCMT(?:\s|$)") {
            throw "opus.lib member does not require LIBCMT: $member"
        }
        if ($directives -match "(?i)DEFAULTLIB:(?:MSVCRT|MSVCRTD)") {
            throw "opus.lib member contains a dynamic-CRT directive: $member"
        }
    }
}
finally {
    Remove-Item -LiteralPath $extractRoot -Recurse -Force -ErrorAction SilentlyContinue
}

$artifacts = @(Get-ChildItem -LiteralPath $distributionDirectory -File -Force |
    Where-Object { $_.Extension -in ".exe", ".dll" })
if ($artifacts.Count -ne 4) {
    throw "Expected exactly four packaged Windows binaries, found $($artifacts.Count)"
}
$forbiddenRuntime = "(?i)(vcruntime[^\\\s]*\.dll|msvcp[^\\\s]*\.dll|msvcrt\.dll|ucrtbase\.dll|libgcc_s[^\\\s]*|libstdc\+\+[^\\\s]*|libwinpthread[^\\\s]*|opus\.dll|openh264[^\\\s]*\.dll)"
foreach ($artifact in $artifacts) {
    $dependents = (& dumpbin.exe /nologo /dependents $artifact.FullName) -join "`n"
    if ($LASTEXITCODE -ne 0) {
        throw "Could not inspect packaged artifact: $($artifact.Name)"
    }
    if ($dependents -match $forbiddenRuntime) {
        throw "$($artifact.Name) has a forbidden runtime dependency: $($Matches[1])"
    }
}

$nestedCodec = @(Get-ChildItem -LiteralPath $distributionDirectory -File -Recurse -Force |
    Where-Object { $_.Name -match "(?i)^(?:lib)?(?:opus|openh264)[^\\/]*\.(?:dll|lib|a|dylib)$" })
if ($nestedCodec.Count) {
    throw "Windows package contains an untracked codec payload: $($nestedCodec.FullName -join ', ')"
}

Write-Host "Verified static Opus CRT evidence plus $($artifacts.Count) packaged Windows binaries."
