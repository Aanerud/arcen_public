# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS

$scriptPath = Join-Path $PSScriptRoot 'new-host-cert.ps1'
. $scriptPath -LibraryOnly

# Pester 3's `Should Throw` does not see exceptions under PowerShell 7.
function Test-Throws {
    param([scriptblock]$Block)
    try {
        & $Block | Out-Null
        return $false
    } catch {
        return $true
    }
}

Describe 'new-host-cert helper' -Tag 'Windows' {
    BeforeEach {
        $root = Join-Path $TestDrive ([Guid]::NewGuid().ToString('N'))
        $tls = Join-Path $root 'tls'
        New-Item -ItemType Directory -Path $tls | Out-Null
        # A stand-in installer that records its arguments and exit status.
        $record = Join-Path $root 'arguments.txt'
        $fake = Join-Path $root 'install-arcen-pier.cmd'
        Set-Content -LiteralPath $fake -Value @(
            '@echo off',
            "echo %* > `"$record`"",
            'if defined ARCEN_FAKE_EXIT exit /b %ARCEN_FAKE_EXIT%',
            'exit /b 0'
        )
    }

    It 'runs the installer in TLS-only mode for the default directory' {
        (Get-ArcenTlsArguments) -join ' ' | Should Be '--tls-only'
    }

    It 'maps renewal, adoption, rekey and extra names onto installer flags' {
        (Get-ArcenTlsArguments -Renew) -join ' ' | Should Be '--tls-only --renew'
        (Get-ArcenTlsArguments -Renew -AdoptLegacyHelperPair) -join ' ' |
            Should Be '--tls-only --adopt-legacy'
        (Get-ArcenTlsArguments -ForceNewKey -ExtraDns 'alias.pier.test' -ExtraIp '192.0.2.10') -join ' ' |
            Should Be '--tls-only --force --extra-san alias.pier.test --extra-san 192.0.2.10'
    }

    It 'passes a custom TLS directory as the installer programdata root' {
        $arguments = Get-ArcenTlsArguments -OutputRoot $tls
        $arguments[1] | Should Be '--programdata'
        $arguments[2] | Should Be $root
    }

    It 'refuses incompatible modes and a directory not named tls' {
        Test-Throws { Get-ArcenTlsArguments -Renew -ForceNewKey } | Should Be $true
        Test-Throws { Get-ArcenTlsArguments -AdoptLegacyHelperPair } | Should Be $true
        Test-Throws { Get-ArcenTlsArguments -OutputRoot $root } | Should Be $true
    }

    It 'delegates to the installer and removes the earlier helper''s stale pin files' {
        Set-Content -LiteralPath (Join-Path $tls 'host.spki-sha256') -Value 'sha256/x'
        Set-Content -LiteralPath (Join-Path $tls 'host.fingerprint.txt') -Value 'stale'
        & $scriptPath -OutputRoot $tls -Installer $fake -Renew
        (Get-Content -LiteralPath $record -Raw).Trim() |
            Should Be "--tls-only --programdata $root --renew"
        Test-Path (Join-Path $tls 'host.fingerprint.txt') | Should Be $false
    }

    It 'fails when the installer fails' {
        $env:ARCEN_FAKE_EXIT = '3'
        try {
            Test-Throws { & $scriptPath -OutputRoot $tls -Installer $fake } | Should Be $true
        } finally {
            Remove-Item Env:ARCEN_FAKE_EXIT
        }
    }

    It 'never runs an installer found by a path relative to the script' {
        # Copied to C:\a\b\c, ..\..\..\target\release is C:\target\release,
        # which any user may create.
        $scriptRoot = Join-Path $root 'a\b\c'
        New-Item -ItemType Directory -Path $scriptRoot | Out-Null
        $planted = Join-Path $root 'target\release\install-arcen-pier.exe'
        New-Item -ItemType Directory -Path (Split-Path $planted) | Out-Null
        Set-Content -LiteralPath $planted -Value 'planted'
        $saved = $env:ARCEN_INSTALLER
        Remove-Item Env:ARCEN_INSTALLER -ErrorAction SilentlyContinue
        try {
            $found = try {
                Resolve-ArcenInstaller -ScriptRoot $scriptRoot
            } catch {
                $_.Exception.Message
            }
            $found | Should Not Be (Resolve-Path -LiteralPath $planted).Path
            $programFiles = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'Arcen\Pier\install-arcen-pier.exe'
            ($found -eq $programFiles -or $found -like 'install-arcen-pier.exe not found*') |
                Should Be $true
        } finally {
            if ($saved) { $env:ARCEN_INSTALLER = $saved }
        }
    }

    It 'refuses to run over an interrupted earlier-helper transaction' {
        Set-Content -LiteralPath (Join-Path $tls 'host-cert.transaction.json') -Value '{}'
        Test-Throws { & $scriptPath -OutputRoot $tls -Installer $fake } | Should Be $true
        Test-Path $record | Should Be $false
    }
}
