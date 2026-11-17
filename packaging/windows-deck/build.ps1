# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS

<#
.SYNOPSIS
Builds "Arcen Deck.msi" and "Arcen USB Component.msi" on Windows x64 (MSVC).

.DESCRIPTION
One source identity, two MSIs:

1. Builds arcen-deck.exe, arcen-usb-service.exe and arcen-usb-prepare.exe in
   release with the pinned toolchain (rust-toolchain.toml, MSVC host), a
   static C runtime like the Windows Pier, and the build identity in
   ARCEN_BUILD_ID / ARCEN_SOURCE_REVISION / ARCEN_FEATURE_PROFILE /
   ARCEN_SIGNING_STATE.
2. Links a Win32 VERSIONINFO resource into each executable (company,
   product, version and the build ID; the Deck also gets its icon, the
   prepare tool its requireAdministrator manifest), compiled with rc.exe and
   passed to the final link only. No crate manifest or build script changes.
3. Checks every executable: the version resource carries the build ID,
   `arcen-deck.exe --version` prints it, no dynamic MSVC runtime is imported,
   neither USB executable is a placeholder, and the Deck is not the
   version-only stub (unless -LabPreview, which marks everything as a lab
   preview, not a release candidate).
4. Optionally Authenticode-signs the executables (before packaging) and the
   MSIs, when ARCEN_SIGN_CERT_THUMBPRINT names a code-signing certificate.
   Without it the step is skipped with a clear line; Beta 1 may ship unsigned.
5. Stages each payload with the five legal files, builds the MSIs with WiX
   v5.0.2 (no extensions), validates them (ICE + verify-msi.ps1), and writes
   SHA256SUMS.txt and build-manifest.json beside them.

WiX v5.0.2 is the pinned build tool: MS-RL source, binaries without the
Open Source Maintenance Fee EULA that WiX v6 and later attach. When no
-WixExe is given it is fetched once from the official release, checked
against its published SHA-256 and unpacked with an administrative image
(msiexec /a), which installs and registers nothing on the build machine.

.EXAMPLE
powershell -NoProfile -ExecutionPolicy Bypass -File packaging\windows-deck\build.ps1 `
    -OutputDirectory C:\arcen-build\wd-package\out `
    -CargoTargetDirectory C:\arcen-build\wd-package\target `
    -ToolsDirectory C:\arcen-build\wd-package\tools `
    -BuildId 20261114T090000Z-0.19.1-0123abcd -SourceRevision 0123abcd4567
#>
[CmdletBinding()]
param(
    [string]$OutputDirectory,
    [string]$CargoTargetDirectory,
    [string]$ToolsDirectory,
    [string]$BuildId = $env:ARCEN_BUILD_ID,
    [string]$SourceRevision = $env:ARCEN_SOURCE_REVISION,
    [string]$WixExe = $env:ARCEN_WIX_EXE,
    # clean | dirty | unknown. Detected from git when the tree is a checkout;
    # a copied tree is "unknown" unless the caller, who copied it, says.
    [ValidateSet("", "clean", "dirty", "unknown")][string]$SourceTreeState = "",
    # Package whatever the tree holds, including a Deck that is still the
    # version-only stub, for lab work on the installers themselves. The MSIs
    # and build-manifest.json then say "lab preview, not a release candidate".
    [switch]$LabPreview
)

Set-StrictMode -Version 3.0
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$WixVersion = "5.0.2"
$WixMsiUrl = "https://github.com/wixtoolset/wix/releases/download/v5.0.2/wix-cli-x64.msi"
# Published in the release's wix-cli-x64.metadata.json.
$WixMsiSha256 = "097383B2773BDD3D76A6A2CD3F4DE6357BAE59172E1B6FA5AD87CE062074E8D0"
$SourceUrl = "https://github.com/Aanerud/arcen_public"
$LegalFiles = @(
    @{ Name = "LICENSE"; Path = "LICENSE" },
    @{ Name = "LICENSE-EXCEPTION.md"; Path = "LICENSE-EXCEPTION.md" },
    @{ Name = "NOTICE"; Path = "NOTICE" },
    @{ Name = "TRADEMARKS.md"; Path = "TRADEMARKS.md" },
    @{ Name = "THIRD_PARTY_NOTICES.md"; Path = "legal\THIRD_PARTY_NOTICES.md" }
)

function Step([string]$Message) { Write-Host "==> $Message" }

function Invoke-Checked([string]$Exe, [string[]]$Arguments) {
    & $Exe @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$([IO.Path]::GetFileName($Exe)) failed with exit code $LASTEXITCODE" }
}

function Assert-NoSpace([string]$Path, [string]$Label) {
    # Paths reach rustc's -C link-arg and rc.exe verbatim.
    if ($Path -match "\s") { throw "$Label must not contain whitespace: $Path" }
}

$Repo = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
if (-not $OutputDirectory) { $OutputDirectory = Join-Path $Repo "target\windows-deck-package" }
if (-not $CargoTargetDirectory) { $CargoTargetDirectory = Join-Path $Repo "target\windows-deck" }
if (-not $ToolsDirectory) { $ToolsDirectory = Join-Path $Repo "target\windows-deck-tools" }
foreach ($dir in @($OutputDirectory, $CargoTargetDirectory, $ToolsDirectory)) {
    New-Item -ItemType Directory -Force $dir | Out-Null
}
$OutputDirectory = (Resolve-Path $OutputDirectory).Path
$CargoTargetDirectory = (Resolve-Path $CargoTargetDirectory).Path
$ToolsDirectory = (Resolve-Path $ToolsDirectory).Path
Assert-NoSpace $CargoTargetDirectory "CargoTargetDirectory"

# --- source identity ---------------------------------------------------------
$cargoToml = Get-Content -Raw (Join-Path $Repo "Cargo.toml")
if ($cargoToml -notmatch '(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"') {
    throw "no [workspace.package] version in Cargo.toml"
}
$Version = $Matches[1]
$versionFile = (Get-Content -Raw (Join-Path $Repo "VERSION")).Trim()
if ($versionFile -ne $Version) { throw "VERSION ($versionFile) disagrees with Cargo.toml ($Version)" }
if ($Version -notmatch '^(\d+)\.(\d+)\.(\d+)(?:[-+].*)?$') { throw "unexpected version '$Version'" }
$Major = [int]$Matches[1]; $Minor = [int]$Matches[2]; $Patch = [int]$Matches[3]
if ($Major -gt 255 -or $Minor -gt 255 -or $Patch -gt 65535) {
    throw "version $Version does not fit an MSI ProductVersion (255.255.65535)"
}
$MsiVersion = "$Major.$Minor.$Patch"

$insideGit = $false
if (Get-Command git -ErrorAction SilentlyContinue) {
    # Windows PowerShell turns a native command's redirected stderr into an
    # error record, which "Stop" would make fatal outside a checkout.
    $preference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        & git -C $Repo rev-parse --is-inside-work-tree 2>$null | Out-Null
        $insideGit = ($LASTEXITCODE -eq 0)
    } finally { $ErrorActionPreference = $preference }
}
if (-not $SourceRevision) {
    if (-not $insideGit) { throw "no git checkout here: pass -SourceRevision (or ARCEN_SOURCE_REVISION)" }
    $SourceRevision = (& git -C $Repo rev-parse HEAD).Trim()
}
if ($insideGit) {
    $detected = if ((& git -C $Repo status --porcelain --untracked-files=no) -join "") { "dirty" } else { "clean" }
    if ($SourceTreeState -and $SourceTreeState -ne $detected) {
        throw "-SourceTreeState $SourceTreeState contradicts the checkout, which is $detected"
    }
    $SourceTreeState = $detected
    $head = (& git -C $Repo rev-parse HEAD).Trim()
    if (-not $head.StartsWith($SourceRevision)) { throw "SourceRevision $SourceRevision is not this checkout's HEAD $head" }
}
if (-not $SourceTreeState) { $SourceTreeState = "unknown" }
if (-not $BuildId) {
    if (-not $insideGit) { throw "no git checkout here: pass -BuildId (or ARCEN_BUILD_ID)" }
    $stamp = [DateTime]::UtcNow.ToString("yyyyMMdd'T'HHmmss'Z'")
    $BuildId = "$stamp-$Version-$($SourceRevision.Substring(0, 8))"
}
# Both reach rc.exe strings, MSI properties and file names: keep them plain.
if ($BuildId -notmatch '^[A-Za-z0-9][A-Za-z0-9._+-]{0,127}$') { throw "BuildId '$BuildId' has unsupported characters" }
if ($SourceRevision -notmatch '^[0-9a-f]{7,40}$') { throw "SourceRevision '$SourceRevision' is not a hex commit" }

# A release candidate is built from one exact, clean, fully named revision;
# anything else is a lab preview and says so in the MSIs and the manifest.
$NotCandidate = New-Object System.Collections.Generic.List[string]
if ($SourceTreeState -ne "clean") { $NotCandidate.Add("source tree state is $SourceTreeState") }
if ($SourceRevision -notmatch '^[0-9a-f]{40}$') { $NotCandidate.Add("source revision $SourceRevision is not a full commit") }

# --- signing configuration ----------------------------------------------------
$Thumbprint = $env:ARCEN_SIGN_CERT_THUMBPRINT
$Signing = $false
if ($Thumbprint) {
    $Thumbprint = $Thumbprint.Replace(" ", "").ToUpperInvariant()
    if ($Thumbprint -notmatch '^[0-9A-F]{40}$') { throw "ARCEN_SIGN_CERT_THUMBPRINT is not a SHA-1 thumbprint" }
    if (-not $env:ARCEN_SIGN_TIMESTAMP_URL) {
        throw "ARCEN_SIGN_TIMESTAMP_URL is required when signing (an untimestamped signature dies with the certificate)"
    }
    $Signing = $true
    $SigningState = "authenticode"
} else {
    $SigningState = "unsigned"
}

Step "Arcen Windows Deck packages $Version, build $BuildId, revision $SourceRevision ($SigningState)"

# --- toolchain and MSVC environment -------------------------------------------
$toolchainToml = Get-Content -Raw (Join-Path $Repo "rust-toolchain.toml")
if ($toolchainToml -notmatch '(?m)^\s*channel\s*=\s*"([^"]+)"') { throw "no channel in rust-toolchain.toml" }
$Toolchain = "$($Matches[1])-x86_64-pc-windows-msvc"
$Cargo = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
$Rustc = Join-Path $env:USERPROFILE ".cargo\bin\rustc.exe"
if (-not (Test-Path $Cargo)) { $Cargo = (Get-Command cargo).Source }
if (-not (Test-Path $Rustc)) { $Rustc = (Get-Command rustc).Source }
# Child tools (the notices generator, build scripts) look for cargo on PATH.
$env:PATH = "$(Split-Path $Cargo);$env:PATH"
$hostTuple = (& $Rustc "+$Toolchain" --print host-tuple).Trim()
if ($hostTuple -ne "x86_64-pc-windows-msvc") { throw "$Toolchain reports host '$hostTuple', expected x86_64-pc-windows-msvc" }

function Import-MsvcEnvironment {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) { throw "vswhere.exe not found; install Visual Studio with the x64 C++ tools" }
    $install = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if (-not $install) { throw "no Visual Studio with the x64 C++ toolset" }
    $vcvars = Join-Path $install "VC\Auxiliary\Build\vcvars64.bat"
    $lines = & cmd.exe /d /s /c "`"$vcvars`" >nul && set"
    if ($LASTEXITCODE -ne 0) { throw "vcvars64.bat failed" }
    foreach ($line in $lines) {
        if ($line -match '^([^=]+)=(.*)$') { Set-Item -Path "env:$($Matches[1])" -Value $Matches[2] }
    }
}
Import-MsvcEnvironment
foreach ($tool in @("link.exe", "rc.exe", "dumpbin.exe", "mt.exe")) {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) { throw "$tool not on PATH after vcvars64.bat" }
}
$RcExe = (Get-Command rc.exe).Source
$MtExe = (Get-Command mt.exe).Source
$SignTool = $null
if ($Signing) {
    $SignTool = (Get-Command signtool.exe -ErrorAction SilentlyContinue)
    if (-not $SignTool) { throw "signing requested but signtool.exe is not on PATH" }
    $SignTool = $SignTool.Source
}

# --- WiX v5.0.2 ---------------------------------------------------------------
function Resolve-Wix {
    $exe = $WixExe
    if (-not $exe) {
        $image = Join-Path $ToolsDirectory "wix-$WixVersion"
        $found = Get-ChildItem -Recurse -Filter wix.exe $image -ErrorAction SilentlyContinue | Select-Object -First 1
        if (-not $found) {
            $msi = Join-Path $ToolsDirectory "wix-cli-x64-$WixVersion.msi"
            if (-not (Test-Path $msi)) {
                Step "fetching WiX $WixVersion from the official release"
                [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
                Invoke-WebRequest -UseBasicParsing -Uri $WixMsiUrl -OutFile $msi
            }
            $hash = (Get-FileHash -Algorithm SHA256 $msi).Hash
            if ($hash -ne $WixMsiSha256) {
                Remove-Item -Force $msi
                throw "WiX MSI SHA-256 $hash does not match the published $WixMsiSha256; deleted"
            }
            if (Test-Path $image) { Remove-Item -Recurse -Force $image }
            # An administrative image only unpacks the files: nothing is
            # installed or registered on this machine.
            $process = Start-Process msiexec.exe -Wait -PassThru -ArgumentList @(
                "/a", "`"$msi`"", "/qn", "TARGETDIR=`"$image`"")
            if ($process.ExitCode -ne 0) { throw "msiexec /a of the WiX MSI failed: $($process.ExitCode)" }
            $found = Get-ChildItem -Recurse -Filter wix.exe $image | Select-Object -First 1
            if (-not $found) { throw "wix.exe not found in the WiX image" }
        }
        $exe = $found.FullName
    }
    $reported = (& $exe --version).Trim()
    if (-not $reported.StartsWith("$WixVersion+") -and $reported -ne $WixVersion) {
        throw "wix.exe reports '$reported'; this build is pinned to WiX $WixVersion (v6+ binaries carry the OSMF EULA)"
    }
    return $exe
}
$Wix = Resolve-Wix
Step "WiX $WixVersion at $Wix"

# --- release inputs -----------------------------------------------------------
$python = Get-Command python -ErrorAction SilentlyContinue
if (-not $python) { $python = Get-Command py -ErrorAction SilentlyContinue }
if (-not $python) { throw "Python 3 is required to check the third-party notices" }
Step "third-party notices are current (scripts\generate-third-party-notices.py --check)"
$env:RUSTUP_TOOLCHAIN = $Toolchain
Push-Location $Repo
try { Invoke-Checked $python.Source @("scripts\generate-third-party-notices.py", "--check") } finally { Pop-Location }
Remove-Item env:RUSTUP_TOOLCHAIN
if (Test-Path (Join-Path $Repo "scripts\verify-opusic-source.ps1")) {
    Invoke-Checked "powershell.exe" @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File",
        (Join-Path $Repo "scripts\verify-opusic-source.ps1"), "-RepositoryRoot", $Repo)
}

# --- resources ----------------------------------------------------------------
$ResourceDir = Join-Path $CargoTargetDirectory "arcen-resources"
if (Test-Path $ResourceDir) { Remove-Item -Recurse -Force $ResourceDir }
New-Item -ItemType Directory -Force $ResourceDir | Out-Null

function New-IconFile([string]$Destination) {
    $pngs = Get-ChildItem (Join-Path $PSScriptRoot "icon") -Filter "ArcenDeck-*.png" |
        Sort-Object { [int]($_.BaseName -replace '^ArcenDeck-', '') }
    if (-not $pngs) { throw "no icon PNGs in packaging\windows-deck\icon" }
    $images = @()
    foreach ($png in $pngs) {
        $bytes = [IO.File]::ReadAllBytes($png.FullName)
        $size = [int]($png.BaseName -replace '^ArcenDeck-', '')
        $width = ([int]$bytes[16] -shl 24) -bor ([int]$bytes[17] -shl 16) -bor ([int]$bytes[18] -shl 8) -bor [int]$bytes[19]
        $height = ([int]$bytes[20] -shl 24) -bor ([int]$bytes[21] -shl 16) -bor ([int]$bytes[22] -shl 8) -bor [int]$bytes[23]
        if ($width -ne $size -or $height -ne $size -or $size -gt 256) { throw "$($png.Name) is ${width}x${height}" }
        $images += , @($size, $bytes)
    }
    $stream = New-Object IO.MemoryStream
    $writer = New-Object IO.BinaryWriter($stream)
    $writer.Write([UInt16]0); $writer.Write([UInt16]1); $writer.Write([UInt16]$images.Count)
    $offset = 6 + 16 * $images.Count
    foreach ($image in $images) {
        $dimension = if ($image[0] -ge 256) { 0 } else { $image[0] }
        $writer.Write([byte]$dimension); $writer.Write([byte]$dimension)
        $writer.Write([byte]0); $writer.Write([byte]0)
        $writer.Write([UInt16]1); $writer.Write([UInt16]32)
        $writer.Write([UInt32]$image[1].Length); $writer.Write([UInt32]$offset)
        $offset += $image[1].Length
    }
    foreach ($image in $images) { $writer.Write([byte[]]$image[1]) }
    $writer.Flush()
    [IO.File]::WriteAllBytes($Destination, $stream.ToArray())
}

$IconFile = Join-Path $ResourceDir "ArcenDeck.ico"
New-IconFile $IconFile

$PrepareManifest = Join-Path $ResourceDir "arcen-usb-prepare.manifest"
@"
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="requireAdministrator" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
</assembly>
"@ | Set-Content -Encoding ASCII $PrepareManifest

function New-VersionResource {
    param([string]$Stem, [string]$Description, [string]$ProductName, [string]$Icon, [string]$Manifest)
    $rcPath = Join-Path $ResourceDir "$Stem.rc"
    $resPath = Join-Path $ResourceDir "$Stem.res"
    $productVersion = "$Version (build $BuildId)"
    $lines = @(
        "1 VERSIONINFO",
        "FILEVERSION $Major,$Minor,$Patch,0",
        "PRODUCTVERSION $Major,$Minor,$Patch,0",
        "FILEFLAGSMASK 0x3fL",
        "FILEFLAGS 0x0L",
        "FILEOS 0x40004L",
        "FILETYPE 0x1L",
        "FILESUBTYPE 0x0L",
        "BEGIN",
        "  BLOCK `"StringFileInfo`"",
        "  BEGIN",
        "    BLOCK `"040904b0`"",
        "    BEGIN",
        "      VALUE `"CompanyName`", `"TUTTIFRUTTI AS`"",
        "      VALUE `"FileDescription`", `"$Description`"",
        "      VALUE `"FileVersion`", `"$Version`"",
        "      VALUE `"InternalName`", `"$Stem`"",
        "      VALUE `"LegalCopyright`", `"Copyright (C) 2026 TUTTIFRUTTI AS. Free software under the AGPL-3.0.`"",
        "      VALUE `"OriginalFilename`", `"$Stem.exe`"",
        "      VALUE `"ProductName`", `"$ProductName`"",
        "      VALUE `"ProductVersion`", `"$productVersion`"",
        "      VALUE `"ArcenBuildId`", `"$BuildId`"",
        "      VALUE `"ArcenSourceRevision`", `"$SourceRevision`"",
        "    END",
        "  END",
        "  BLOCK `"VarFileInfo`"",
        "  BEGIN",
        "    VALUE `"Translation`", 0x409, 1200",
        "  END",
        "END"
    )
    if ($Icon) { $lines += "1 ICON `"$($Icon.Replace('\', '\\'))`"" }
    # 24 = RT_MANIFEST, 1 = CREATEPROCESS_MANIFEST_RESOURCE_ID.
    if ($Manifest) { $lines += "1 24 `"$($Manifest.Replace('\', '\\'))`"" }
    $lines | Set-Content -Encoding ASCII $rcPath
    Invoke-Checked $RcExe @("/nologo", "/fo", $resPath, $rcPath)
    # Cargo fingerprints the link arguments, not the file they name: a name
    # derived from the content makes every changed resource force a relink.
    $digest = (Get-FileHash -Algorithm SHA256 $resPath).Hash.Substring(0, 16).ToLowerInvariant()
    $named = Join-Path $ResourceDir "$Stem-$digest.res"
    Move-Item -Force $resPath $named
    return $named
}

$Executables = @(
    @{ Package = "arcen-deck-windows"; Bin = "arcen-deck"; Description = "Arcen Deck";
       Product = "Arcen Deck"; Icon = $IconFile; Manifest = $null },
    @{ Package = "arcen-usb-service"; Bin = "arcen-usb-service"; Description = "Arcen USB component service";
       Product = "Arcen USB component"; Icon = $null; Manifest = $null },
    @{ Package = "arcen-usb-prepare"; Bin = "arcen-usb-prepare"; Description = "Arcen USB component: prepare a tablet";
       Product = "Arcen USB component"; Icon = $null; Manifest = $PrepareManifest }
)

# --- build --------------------------------------------------------------------
$env:CARGO_TARGET_DIR = $CargoTargetDirectory
$env:CARGO_INCREMENTAL = "0"
$env:RUSTFLAGS = "-C target-feature=+crt-static"
$env:CFLAGS_x86_64_pc_windows_msvc = "/MT"
$env:CXXFLAGS_x86_64_pc_windows_msvc = "/MT"
$env:ARCEN_BUILD_ID = $BuildId
$env:ARCEN_SOURCE_REVISION = $SourceRevision
$env:ARCEN_FEATURE_PROFILE = "quic-default"
$env:ARCEN_SIGNING_STATE = $SigningState

$Release = Join-Path $CargoTargetDirectory "release"
Push-Location $Repo
try {
    foreach ($exe in $Executables) {
        $res = New-VersionResource -Stem $exe.Bin -Description $exe.Description -ProductName $exe.Product `
            -Icon $exe.Icon -Manifest $exe.Manifest
        Step "cargo rustc --release -p $($exe.Package) --bin $($exe.Bin) (+ version resource)"
        # `cargo rustc` hands the extra arguments to the final crate only, so
        # the resource reaches this one link and no dependency or build script.
        Invoke-Checked $Cargo @("+$Toolchain", "rustc", "--locked", "--release", "-p", $exe.Package,
            "--bin", $exe.Bin, "--", "-C", "link-arg=$res")
    }
} finally { Pop-Location }

# --- verify executables -------------------------------------------------------
foreach ($exe in $Executables) {
    $path = Join-Path $Release "$($exe.Bin).exe"
    if (-not (Test-Path $path)) { throw "missing $path" }
    $info = (Get-Item $path).VersionInfo
    if ($info.ProductVersion -ne "$Version (build $BuildId)" -or $info.CompanyName -ne "TUTTIFRUTTI AS" -or
        $info.FileVersion -ne $Version) {
        throw "$($exe.Bin).exe version resource is '$($info.CompanyName)' '$($info.FileVersion)' '$($info.ProductVersion)'"
    }
    $dependents = (& dumpbin.exe /nologo /dependents $path) -join "`n"
    if ($dependents -match '(?i)vcruntime|msvcp|ucrtbase|api-ms-win-crt-') {
        throw "$($exe.Bin).exe imports a dynamic C runtime; it must be built with +crt-static"
    }
}
$deckExe = Join-Path $Release "arcen-deck.exe"
# A developer build (cargo feature dev-tools) must never ship, even when a
# stale one sits in the target directory.
$deckText = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($deckExe))
if ($deckText.Contains("ARCEN_DECK_DEV_TOOLS_FEATURE_MARKER_DO_NOT_SHIP")) {
    throw "arcen-deck.exe was built with the dev-tools feature; a release package refuses it"
}
# arcen-deck.exe is a windows-subsystem program: PowerShell neither waits for
# it nor captures its output when called with `&`, so read it through pipes.
$psi = New-Object Diagnostics.ProcessStartInfo $deckExe, "--version"
$psi.UseShellExecute = $false
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.CreateNoWindow = $true
$versionProcess = [Diagnostics.Process]::Start($psi)
$versionError = $versionProcess.StandardError.ReadToEndAsync()
$versionText = $versionProcess.StandardOutput.ReadToEnd().Trim()
$versionProcess.WaitForExit()
if ($versionProcess.ExitCode -ne 0 -or $versionText -notmatch [regex]::Escape("build_id=$BuildId")) {
    throw "arcen-deck.exe --version does not print build_id=$BuildId (exit $($versionProcess.ExitCode); got: $versionText $($versionError.Result))"
}
Write-Host $versionText
foreach ($bin in @("arcen-usb-service", "arcen-usb-prepare")) {
    $text = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes((Join-Path $Release "$bin.exe")))
    if ($text.Contains("${bin}: not implemented yet")) {
        throw "$bin.exe is still the placeholder; build from a tree with the USB component"
    }
}
# The Deck's version-only stub (it answers --version and exits) is not a
# Deck. Only a lab preview may package it.
$deckText = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($deckExe))
if ($deckText.Contains("the Windows Deck is under construction")) {
    if (-not $LabPreview) {
        throw "arcen-deck.exe is the version-only stub, not the Deck application; pass -LabPreview to package it for installer lab work"
    }
    $NotCandidate.Add("arcen-deck.exe is the version-only stub")
}
if ($LabPreview) { $NotCandidate.Add("built with -LabPreview") }
$ReleaseCandidate = ($NotCandidate.Count -eq 0)
if ($ReleaseCandidate) {
    Step "release candidate: $BuildId from clean $SourceRevision"
} else {
    Write-Host "==> LAB PREVIEW, not a release candidate: $($NotCandidate -join '; ')"
}
$manifestOut = Join-Path $ResourceDir "prepare.extracted.manifest"
Invoke-Checked $MtExe @("-nologo", "-inputresource:$(Join-Path $Release 'arcen-usb-prepare.exe');#1", "-out:$manifestOut")
if (-not ((Get-Content -Raw $manifestOut) -match 'level="requireAdministrator"')) {
    throw "arcen-usb-prepare.exe does not carry the requireAdministrator manifest"
}

function Invoke-Sign([string]$Path, [string]$Description) {
    if (-not $Signing) { return }
    Invoke-Checked $SignTool @("sign", "/sha1", $Thumbprint, "/fd", "SHA256", "/tr", $env:ARCEN_SIGN_TIMESTAMP_URL,
        "/td", "SHA256", "/d", $Description, "/du", $SourceUrl, $Path)
    Invoke-Checked $SignTool @("verify", "/pa", "/q", $Path)
}

if ($Signing) {
    Step "Authenticode: signing with certificate $Thumbprint"
} else {
    Write-Host "==> Authenticode: SKIPPED - ARCEN_SIGN_CERT_THUMBPRINT is not set. The executables and MSIs are UNSIGNED; SmartScreen warns and Smart App Control blocks them (packaging\windows-deck\README.md)."
}

# --- stage --------------------------------------------------------------------
$Stage = Join-Path $CargoTargetDirectory "arcen-stage"
if (Test-Path $Stage) { Remove-Item -Recurse -Force $Stage }
$DeckStage = Join-Path $Stage "deck"
$UsbStage = Join-Path $Stage "usb"
New-Item -ItemType Directory -Force $DeckStage, $UsbStage | Out-Null
Copy-Item (Join-Path $Release "arcen-deck.exe") $DeckStage
Copy-Item (Join-Path $Release "arcen-usb-service.exe") $UsbStage
Copy-Item (Join-Path $Release "arcen-usb-prepare.exe") $UsbStage
foreach ($legal in $LegalFiles) {
    Copy-Item (Join-Path $Repo $legal.Path) (Join-Path $DeckStage $legal.Name)
    Copy-Item (Join-Path $Repo $legal.Path) (Join-Path $UsbStage $legal.Name)
}
Invoke-Sign (Join-Path $DeckStage "arcen-deck.exe") "Arcen Deck"
Invoke-Sign (Join-Path $UsbStage "arcen-usb-service.exe") "Arcen USB component"
Invoke-Sign (Join-Path $UsbStage "arcen-usb-prepare.exe") "Arcen USB component"

# --- MSIs ---------------------------------------------------------------------
Get-ChildItem $OutputDirectory -File | Remove-Item -Force
$Packages = @(
    @{ Kind = "Deck"; Source = "ArcenDeck.wxs"; Output = "Arcen Deck.msi"; Stage = $DeckStage; Title = "Arcen Deck" },
    @{ Kind = "Usb"; Source = "ArcenUsbComponent.wxs"; Output = "Arcen USB Component.msi"; Stage = $UsbStage; Title = "Arcen USB component" }
)
foreach ($package in $Packages) {
    $msi = Join-Path $OutputDirectory $package.Output
    Step "wix build $($package.Output)"
    Invoke-Checked $Wix @("build", "-nologo", "-arch", "x64", "-pdbtype", "none",
        "-d", "ArcenVersion=$Version", "-d", "ArcenMsiVersion=$MsiVersion", "-d", "ArcenBuildId=$BuildId",
        "-d", "ArcenSourceRevision=$SourceRevision", "-d", "ArcenSourceUrl=$SourceUrl",
        "-d", "StageDir=$($package.Stage)", "-d", "IconFile=$IconFile",
        "-d", "ArcenReleaseCandidate=$(if ($ReleaseCandidate) { '1' } else { '0' })",
        "-o", $msi, (Join-Path $PSScriptRoot "wix\$($package.Source)"))
    Step "ICE validation of $($package.Output)"
    # ICE43/ICE57 assume a shortcut in ProgramMenuFolder may be per-user; in
    # this per-machine (ALLUSERS=1) package it is the all-users Start menu, so
    # the file keypath is right and an HKCU keypath would be wrong. ICE61 is
    # the expected warning for AllowSameVersionUpgrades.
    Invoke-Checked $Wix @("msi", "validate", "-nologo", "-sice", "ICE43", "-sice", "ICE57", $msi)
    Invoke-Checked "powershell.exe" @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File",
        (Join-Path $PSScriptRoot "verify-msi.ps1"), "-Msi", $msi, "-Kind", $package.Kind, "-BuildId", $BuildId,
        "-ReleaseCandidate", $(if ($ReleaseCandidate) { "1" } else { "0" }))
    Invoke-Sign $msi $package.Title
}

# --- checksums and manifest ---------------------------------------------------
function Get-Sha256([string]$Path) { (Get-FileHash -Algorithm SHA256 $Path).Hash.ToLowerInvariant() }
function Get-SignatureState([string]$Path) {
    $signature = Get-AuthenticodeSignature -FilePath $Path
    if ($signature.Status -eq "NotSigned") { return [ordered]@{ state = "unsigned" } }
    return [ordered]@{
        state = "$($signature.Status)".ToLowerInvariant()
        signer = $(if ($signature.SignerCertificate) { $signature.SignerCertificate.Subject } else { $null })
        signer_thumbprint = $(if ($signature.SignerCertificate) { $signature.SignerCertificate.Thumbprint } else { $null })
        timestamped = ($null -ne $signature.TimeStamperCertificate)
    }
}
$artefacts = @()
foreach ($package in $Packages) {
    $msi = Join-Path $OutputDirectory $package.Output
    $artefacts += [ordered]@{ name = $package.Output; size = (Get-Item $msi).Length; sha256 = (Get-Sha256 $msi);
        signature = (Get-SignatureState $msi) }
}
$executables = @()
foreach ($file in @((Join-Path $DeckStage "arcen-deck.exe"), (Join-Path $UsbStage "arcen-usb-service.exe"),
        (Join-Path $UsbStage "arcen-usb-prepare.exe"))) {
    $executables += [ordered]@{ name = [IO.Path]::GetFileName($file); sha256 = (Get-Sha256 $file);
        product_version = (Get-Item $file).VersionInfo.ProductVersion; signature = (Get-SignatureState $file) }
}
$inputs = [ordered]@{ "Cargo.lock" = (Get-Sha256 (Join-Path $Repo "Cargo.lock")) }
foreach ($legal in $LegalFiles) { $inputs[$legal.Path.Replace('\', '/')] = (Get-Sha256 (Join-Path $Repo $legal.Path)) }
foreach ($source in @("wix\ArcenDeck.wxs", "wix\ArcenUsbComponent.wxs", "build.ps1", "verify-msi.ps1")) {
    $inputs["packaging/windows-deck/$($source.Replace('\', '/'))"] = (Get-Sha256 (Join-Path $PSScriptRoot $source))
}
$manifest = [ordered]@{
    product = "arcen-deck-windows"
    version = $Version
    msi_version = $MsiVersion
    build_id = $BuildId
    source_revision = $SourceRevision
    source_tree_state = $SourceTreeState
    release_candidate = $ReleaseCandidate
    not_a_candidate_because = @($NotCandidate)
    feature_profile = "quic-default"
    signing_state = $SigningState
    rust_toolchain = $Toolchain
    rustc = ((& $Rustc "+$Toolchain" -vV) -join "; ")
    wix = [ordered]@{ version = $WixVersion; msi_sha256 = $WixMsiSha256.ToLowerInvariant() }
    inputs = $inputs
    artefacts = $artefacts
    executables = $executables
}
$manifestPath = Join-Path $OutputDirectory "build-manifest.json"
[IO.File]::WriteAllText($manifestPath, ($manifest | ConvertTo-Json -Depth 5) + "`n", (New-Object Text.UTF8Encoding($false)))

$sums = @()
foreach ($name in @("Arcen Deck.msi", "Arcen USB Component.msi", "build-manifest.json")) {
    $sums += "$(Get-Sha256 (Join-Path $OutputDirectory $name))  $name"
}
# LF line endings so `sha256sum -c` reads it anywhere.
[IO.File]::WriteAllText((Join-Path $OutputDirectory "SHA256SUMS.txt"), ($sums -join "`n") + "`n",
    (New-Object Text.UTF8Encoding($false)))

Step "done: $OutputDirectory"
Get-Content (Join-Path $OutputDirectory "SHA256SUMS.txt") | ForEach-Object { Write-Host "    $_" }
