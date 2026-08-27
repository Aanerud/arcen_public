Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:ArcenClsid = '{2FBE34F2-9E7A-42FA-BFBF-44897694BE60}'
$script:LegacyArcenClsid = '{EB964364-F25C-4579-A9DE-4514C90F1B39}'
$script:FriendlyName = 'Arcen Credential Provider'
$script:ClsidSubkey = "SOFTWARE\Classes\CLSID\$($script:ArcenClsid)"
$script:InprocSubkey = "$($script:ClsidSubkey)\InprocServer32"
$script:ProviderSubkey = "SOFTWARE\Microsoft\Windows\CurrentVersion\Authentication\Credential Providers\$($script:ArcenClsid)"
$script:LegacyClsidSubkey = "SOFTWARE\Classes\CLSID\$($script:LegacyArcenClsid)"
$script:LegacyInprocSubkey = "$($script:LegacyClsidSubkey)\InprocServer32"
$script:LegacyProviderSubkey = "SOFTWARE\Microsoft\Windows\CurrentVersion\Authentication\Credential Providers\$($script:LegacyArcenClsid)"
$script:InstallDirectory = Join-Path $env:ProgramFiles 'Arcen\CredentialProvider'
$script:DestinationDll = Join-Path $script:InstallDirectory 'arcen_credential_provider.dll'
$script:TestMarker = Join-Path $script:InstallDirectory 'UNSIGNED-TEST-INSTALL.txt'

function Assert-ArcenInstallContext {
    if (-not [Environment]::Is64BitOperatingSystem -or -not [Environment]::Is64BitProcess) {
        throw 'Credential Provider registration must run in 64-bit PowerShell on 64-bit Windows.'
    }

    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Credential Provider registration requires an elevated administrator or LocalSystem.'
    }
}

function Assert-Amd64Pe {
    param([Parameter(Mandatory)][string]$Path)

    $stream = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    $reader = [IO.BinaryReader]::new($stream)
    try {
        if ($reader.ReadUInt16() -ne 0x5A4D) {
            throw 'Credential Provider DLL is not a PE image.'
        }
        $stream.Position = 0x3C
        $peOffset = $reader.ReadUInt32()
        if ($peOffset -gt ($stream.Length - 6)) {
            throw 'Credential Provider DLL has an invalid PE header offset.'
        }
        $stream.Position = $peOffset
        if ($reader.ReadUInt32() -ne 0x00004550) {
            throw 'Credential Provider DLL has an invalid PE signature.'
        }
        if ($reader.ReadUInt16() -ne 0x8664) {
            throw 'Credential Provider DLL must be AMD64.'
        }
    }
    finally {
        $reader.Dispose()
        $stream.Dispose()
    }
}

function Open-Registry64 {
    return [Microsoft.Win32.RegistryKey]::OpenBaseKey(
        [Microsoft.Win32.RegistryHive]::LocalMachine,
        [Microsoft.Win32.RegistryView]::Registry64
    )
}

function Test-RegistrySubkey {
    param(
        [Parameter(Mandatory)][Microsoft.Win32.RegistryKey]$Base,
        [Parameter(Mandatory)][string]$Path
    )

    $key = $Base.OpenSubKey($Path, $false)
    if ($null -eq $key) {
        return $false
    }

    $key.Dispose()
    return $true
}

function Test-ArcenRegistrationOwned {
    param(
        [Parameter(Mandatory)][Microsoft.Win32.RegistryKey]$Base,
        [Parameter(Mandatory)][string]$ClsidSubkey,
        [Parameter(Mandatory)][string]$InprocSubkey,
        [Parameter(Mandatory)][string]$ProviderSubkey
    )

    $clsid = $Base.OpenSubKey($ClsidSubkey, $false)
    $inproc = $Base.OpenSubKey($InprocSubkey, $false)
    $provider = $Base.OpenSubKey($ProviderSubkey, $false)
    try {
        return $null -ne $clsid -and
            $null -ne $inproc -and
            $null -ne $provider -and
            $clsid.GetValue('') -eq $script:FriendlyName -and
            $inproc.GetValue('') -eq $script:DestinationDll -and
            $inproc.GetValue('ThreadingModel') -eq 'Apartment' -and
            $provider.GetValue('') -eq $script:FriendlyName
    }
    finally {
        if ($null -ne $clsid) { $clsid.Dispose() }
        if ($null -ne $inproc) { $inproc.Dispose() }
        if ($null -ne $provider) { $provider.Dispose() }
    }
}

function Uninstall-LegacyArcenCredentialProvider {
    param([switch]$TestInstall)

    Assert-ArcenInstallContext
    $isTestInstall = Test-Path -LiteralPath $script:TestMarker
    if ($isTestInstall -and -not $TestInstall) {
        throw 'This is an unsigned test install. Re-run with -TestInstall to acknowledge the rollback.'
    }
    if (-not $isTestInstall -and $TestInstall) {
        throw 'The test-install marker is absent; refusing a test rollback against a production install.'
    }

    $registry = Open-Registry64
    try {
        if ((Test-RegistrySubkey -Base $registry -Path $script:ClsidSubkey) -or
            (Test-RegistrySubkey -Base $registry -Path $script:ProviderSubkey)) {
            throw 'The current Arcen CLSID is registered; refusing legacy uninstall because both registrations may share the DLL.'
        }
        if (-not (Test-ArcenRegistrationOwned `
                -Base $registry `
                -ClsidSubkey $script:LegacyClsidSubkey `
                -InprocSubkey $script:LegacyInprocSubkey `
                -ProviderSubkey $script:LegacyProviderSubkey)) {
            throw 'Legacy registration is absent or is not owned by Arcen; refusing to modify it.'
        }
        $registry.DeleteSubKeyTree($script:LegacyProviderSubkey, $false)
        $registry.DeleteSubKeyTree($script:LegacyClsidSubkey, $false)
    }
    finally {
        $registry.Dispose()
    }

    Remove-Item -LiteralPath $script:TestMarker -Force -ErrorAction SilentlyContinue
    try {
        Remove-Item -LiteralPath $script:DestinationDll -Force -ErrorAction Stop
    }
    catch {
        Write-Warning "Legacy registration was removed, but the DLL is still loaded. Reboot before installing the new provider: $($script:DestinationDll)"
    }
}

function Install-ArcenCredentialProvider {
    param(
        [Parameter(Mandatory)][string]$SourceDll,
        [Parameter(Mandatory)][ValidateSet('Production', 'Test')][string]$Mode
    )

    Assert-ArcenInstallContext
    $resolvedSource = (Resolve-Path -LiteralPath $SourceDll).Path
    Assert-Amd64Pe -Path $resolvedSource
    $sourceSignerThumbprint = $null
    if ($Mode -eq 'Production') {
        $sourceSignature = Get-AuthenticodeSignature -LiteralPath $resolvedSource
        if ($sourceSignature.Status -ne [System.Management.Automation.SignatureStatus]::Valid -or
            $null -eq $sourceSignature.SignerCertificate) {
            throw 'The production source DLL does not have a valid Authenticode signature.'
        }
        $sourceSignerThumbprint = $sourceSignature.SignerCertificate.Thumbprint
    }

    $registry = Open-Registry64
    $createdClsid = $false
    $createdProvider = $false
    $placedDll = $false
    $createdDirectory = $false
    $stagingDll = "$($script:DestinationDll).installing"
    try {
        if (Test-ArcenRegistrationOwned `
                -Base $registry `
                -ClsidSubkey $script:LegacyClsidSubkey `
                -InprocSubkey $script:LegacyInprocSubkey `
                -ProviderSubkey $script:LegacyProviderSubkey) {
            throw "A legacy Arcen Credential Provider is installed. Run uninstall.ps1 -LegacyArcenInstall (and -TestInstall for an unsigned lab install), reboot if its DLL remains loaded, then retry."
        }
        if ((Test-RegistrySubkey -Base $registry -Path $script:ClsidSubkey) -or
            (Test-RegistrySubkey -Base $registry -Path $script:ProviderSubkey)) {
            throw 'The Arcen CLSID is already registered. Uninstall it explicitly before reinstalling.'
        }

        if (Test-Path -LiteralPath $script:DestinationDll) {
            throw "Destination DLL already exists: $($script:DestinationDll)"
        }
        if (Test-Path -LiteralPath $stagingDll) {
            throw "A stale install staging file exists: $stagingDll"
        }

        if (-not (Test-Path -LiteralPath $script:InstallDirectory)) {
            New-Item -ItemType Directory -Path $script:InstallDirectory | Out-Null
            $createdDirectory = $true
        }
        Copy-Item -LiteralPath $resolvedSource -Destination $stagingDll
        Move-Item -LiteralPath $stagingDll -Destination $script:DestinationDll
        $placedDll = $true
        if ($Mode -eq 'Production') {
            $copiedSignature = Get-AuthenticodeSignature -LiteralPath $script:DestinationDll
            if ($copiedSignature.Status -ne [System.Management.Automation.SignatureStatus]::Valid -or
                $null -eq $copiedSignature.SignerCertificate) {
                throw 'The copied production DLL does not have a valid Authenticode signature.'
            }
            if ($copiedSignature.SignerCertificate.Thumbprint -ne $sourceSignerThumbprint) {
                throw 'The copied production DLL signer does not match the verified source signer.'
            }
        }

        $clsidKey = $registry.CreateSubKey($script:ClsidSubkey, $true)
        if ($null -eq $clsidKey) {
            throw 'Unable to create the 64-bit CLSID key.'
        }
        $createdClsid = $true
        try {
            $clsidKey.SetValue('', $script:FriendlyName, [Microsoft.Win32.RegistryValueKind]::String)
        }
        finally {
            $clsidKey.Dispose()
        }

        $inprocKey = $registry.CreateSubKey($script:InprocSubkey, $true)
        if ($null -eq $inprocKey) {
            throw 'Unable to create the 64-bit InprocServer32 key.'
        }
        try {
            $inprocKey.SetValue('', $script:DestinationDll, [Microsoft.Win32.RegistryValueKind]::String)
            $inprocKey.SetValue('ThreadingModel', 'Apartment', [Microsoft.Win32.RegistryValueKind]::String)
        }
        finally {
            $inprocKey.Dispose()
        }

        $providerKey = $registry.CreateSubKey($script:ProviderSubkey, $true)
        if ($null -eq $providerKey) {
            throw 'Unable to create the 64-bit Credential Providers key.'
        }
        $createdProvider = $true
        try {
            $providerKey.SetValue('', $script:FriendlyName, [Microsoft.Win32.RegistryValueKind]::String)
        }
        finally {
            $providerKey.Dispose()
        }

        if ($Mode -eq 'Test') {
            @(
                'UNSIGNED TEST CREDENTIAL PROVIDER'
                'Remove with: uninstall.ps1 -TestInstall'
                "CLSID: $($script:ArcenClsid)"
            ) | Set-Content -LiteralPath $script:TestMarker -Encoding Ascii
        }
    }
    catch {
        if ($createdProvider) {
            $registry.DeleteSubKeyTree($script:ProviderSubkey, $false)
        }
        if ($createdClsid) {
            $registry.DeleteSubKeyTree($script:ClsidSubkey, $false)
        }
        Remove-Item -LiteralPath $stagingDll -Force -ErrorAction SilentlyContinue
        if ($placedDll) {
            Remove-Item -LiteralPath $script:DestinationDll -Force -ErrorAction SilentlyContinue
        }
        Remove-Item -LiteralPath $script:TestMarker -Force -ErrorAction SilentlyContinue
        if ($createdDirectory) {
            if (Test-Path -LiteralPath $script:InstallDirectory) {
                $remaining = @(Get-ChildItem -LiteralPath $script:InstallDirectory -Force)
                if ($remaining.Count -ne 0) {
                    Write-Warning "Credential Provider directory still contains files and was retained: $($script:InstallDirectory)"
                }
            }
        }
        throw
    }
    finally {
        $registry.Dispose()
    }
}

function Uninstall-ArcenCredentialProvider {
    param([switch]$TestInstall)

    Assert-ArcenInstallContext
    $isTestInstall = Test-Path -LiteralPath $script:TestMarker
    if ($isTestInstall -and -not $TestInstall) {
        throw 'This is an unsigned test install. Re-run with -TestInstall to acknowledge the rollback.'
    }
    if (-not $isTestInstall -and $TestInstall) {
        throw 'The test-install marker is absent; refusing a test rollback against a production install.'
    }

    $registry = Open-Registry64
    try {
        $clsid = $registry.OpenSubKey($script:ClsidSubkey, $false)
        if ($null -ne $clsid) {
            try {
                if ($clsid.GetValue('') -ne $script:FriendlyName) {
                    throw 'CLSID key is not owned by Arcen; refusing to delete it.'
                }
            }
            finally {
                $clsid.Dispose()
            }
        }

        $inproc = $registry.OpenSubKey($script:InprocSubkey, $false)
        if ($null -ne $inproc) {
            try {
                if ($inproc.GetValue('') -ne $script:DestinationDll -or
                    $inproc.GetValue('ThreadingModel') -ne 'Apartment') {
                    throw 'Registration does not match Arcen; refusing to delete it.'
                }
            }
            finally {
                $inproc.Dispose()
            }
        }

        $provider = $registry.OpenSubKey($script:ProviderSubkey, $false)
        if ($null -ne $provider) {
            try {
                if ($provider.GetValue('') -ne $script:FriendlyName) {
                    throw 'Credential Providers key is not owned by Arcen; refusing to delete it.'
                }
            }
            finally {
                $provider.Dispose()
            }
        }

        $registry.DeleteSubKeyTree($script:ProviderSubkey, $false)
        $registry.DeleteSubKeyTree($script:ClsidSubkey, $false)
    }
    finally {
        $registry.Dispose()
    }

    Remove-Item -LiteralPath $script:TestMarker -Force -ErrorAction SilentlyContinue
    try {
        Remove-Item -LiteralPath $script:DestinationDll -Force -ErrorAction Stop
    }
    catch {
        Write-Warning "Registration was removed, but the DLL is still loaded. Delete it after reboot: $($script:DestinationDll)"
    }
    if (Test-Path -LiteralPath $script:InstallDirectory) {
        $remaining = @(Get-ChildItem -LiteralPath $script:InstallDirectory -Force)
        if ($remaining.Count -ne 0) {
            Write-Warning "Credential Provider directory still contains files and was retained: $($script:InstallDirectory)"
        }
    }
}
