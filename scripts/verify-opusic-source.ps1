[CmdletBinding()]
param(
    [string]$RepositoryRoot
)

$ErrorActionPreference = "Stop"
if (-not $RepositoryRoot) {
    $RepositoryRoot = Join-Path $PSScriptRoot ".."
}
$resolvedRepositoryRoot = @(Resolve-Path -LiteralPath $RepositoryRoot -ErrorAction Stop)
if ($resolvedRepositoryRoot.Count -ne 1) {
    throw "RepositoryRoot must resolve to exactly one path: $RepositoryRoot"
}
if ($resolvedRepositoryRoot[0].Provider.Name -ne "FileSystem") {
    throw "RepositoryRoot must use the FileSystem provider: $RepositoryRoot"
}
$repositoryRoot = $resolvedRepositoryRoot[0].Path
$sourceRoot = Join-Path $repositoryRoot "third_party\opusic-sys-0.7.3-arcen1"
$manifestPath = Join-Path $sourceRoot "ARCEN_SOURCE_MANIFEST.sha256"

function Get-Sha256([string]$Path) {
    $stream = [IO.File]::OpenRead($Path)
    try {
        $sha256 = [Security.Cryptography.SHA256]::Create()
        try {
            return ([BitConverter]::ToString($sha256.ComputeHash($stream))).Replace("-", "").ToLowerInvariant()
        }
        finally {
            $sha256.Dispose()
        }
    }
    finally {
        $stream.Dispose()
    }
}

if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
    throw "Missing governed source manifest: $manifestPath"
}

$expected = @{}
foreach ($line in Get-Content -LiteralPath $manifestPath) {
    if ($line -notmatch "^([0-9a-f]{64})  (.+)$") {
        throw "Malformed source manifest line: $line"
    }
    if ($expected.ContainsKey($Matches[2])) {
        throw "Duplicate source manifest path: $($Matches[2])"
    }
    $expected[$Matches[2]] = $Matches[1]
}

$sourcePrefix = $sourceRoot.TrimEnd(
    [IO.Path]::DirectorySeparatorChar,
    [IO.Path]::AltDirectorySeparatorChar
) + [IO.Path]::DirectorySeparatorChar
$actual = @{}
foreach ($file in Get-ChildItem -LiteralPath $sourceRoot -Force -File -Recurse) {
    if ($file.FullName -eq $manifestPath) {
        continue
    }
    $relativePath = $file.FullName.Substring($sourcePrefix.Length).Replace("\", "/")
    $actual[$relativePath] = Get-Sha256 $file.FullName
}

$missing = @($expected.Keys | Where-Object { -not $actual.ContainsKey($_) } | Sort-Object)
$extra = @($actual.Keys | Where-Object { -not $expected.ContainsKey($_) } | Sort-Object)
$changed = @(
    $expected.Keys |
        Where-Object { $actual.ContainsKey($_) -and $actual[$_] -ne $expected[$_] } |
        Sort-Object
)

if ($missing.Count -or $extra.Count -or $changed.Count) {
    if ($missing.Count) {
        Write-Error "Missing governed source files: $($missing -join ', ')"
    }
    if ($extra.Count) {
        Write-Error "Unreviewed governed source files: $($extra -join ', ')"
    }
    if ($changed.Count) {
        Write-Error "Changed governed source files: $($changed -join ', ')"
    }
    throw "opusic-sys governed source verification failed"
}

Write-Host "Verified $($actual.Count) governed opusic-sys source files."
