$scriptPath = Join-Path $PSScriptRoot 'new-host-cert.ps1'

Describe 'new-host-cert helper' -Tag 'Windows' {
    BeforeEach {
        $root = Join-Path $TestDrive ([Guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Path $root | Out-Null
    }

    It 'first-issues without a certificate-store mutation and carries SAN/EKU/pins' {
        & $scriptPath -OutputRoot $root -TestOnlySkipAcl -Subject 'pier.test' `
            -ExtraDns 'alias.pier.test' -ExtraIp '192.0.2.10'
        $certPath = Join-Path $root 'host.crt'
        $keyPath = Join-Path $root 'host.key'
        Test-Path $certPath | Should Be $true
        (Get-Content $keyPath -Raw) | Should Match 'BEGIN PRIVATE KEY'
        Test-Path (Join-Path $root 'host.fingerprint.txt') | Should Be $true
        Test-Path (Join-Path $root 'host.spki-sha256.txt') | Should Be $true

        $certificate = [Security.Cryptography.X509Certificates.X509Certificate2]::CreateFromPem(
            [IO.File]::ReadAllText($certPath)
        )
        try {
            $eku = $certificate.Extensions |
                Where-Object { $_.Oid.Value -eq '2.5.29.37' } |
                ForEach-Object { $_.Format($false) }
            $san = $certificate.Extensions |
                Where-Object { $_.Oid.Value -eq '2.5.29.17' } |
                ForEach-Object { $_.Format($false) }
            $eku | Should Match '1.3.6.1.5.5.7.3.1|Server Authentication'
            $san | Should Match 'pier.test'
            $san | Should Match 'alias.pier.test'
            $san | Should Match '192.0.2.10'
            ($certificate.NotAfter - $certificate.NotBefore).TotalDays |
                Should BeGreaterThan 824
        } finally {
            $certificate.Dispose()
        }
    }

    It 'does not overwrite a complete pair without an explicit operation' {
        & $scriptPath -OutputRoot $root -TestOnlySkipAcl -Subject 'pier.test'
        $certPath = Join-Path $root 'host.crt'
        $before = [IO.File]::ReadAllBytes($certPath)
        & $scriptPath -OutputRoot $root -TestOnlySkipAcl
        [Convert]::ToBase64String([IO.File]::ReadAllBytes($certPath)) |
            Should Be ([Convert]::ToBase64String($before))
    }

    It 'renews with the same SPKI and rekeys only when explicitly requested' {
        & $scriptPath -OutputRoot $root -TestOnlySkipAcl -Subject 'pier.test'
        $firstCert = Get-Content (Join-Path $root 'host.fingerprint.txt') -Raw
        $firstSpki = Get-Content (Join-Path $root 'host.spki-sha256.txt') -Raw
        & $scriptPath -OutputRoot $root -TestOnlySkipAcl -Subject 'pier.test' -Renew
        (Get-Content (Join-Path $root 'host.spki-sha256.txt') -Raw) |
            Should Be $firstSpki
        (Get-Content (Join-Path $root 'host.fingerprint.txt') -Raw) |
            Should Not Be $firstCert
        & $scriptPath -OutputRoot $root -TestOnlySkipAcl -Subject 'pier.test' -ForceNewKey
        (Get-Content (Join-Path $root 'host.spki-sha256.txt') -Raw) |
            Should Not Be $firstSpki
    }

    It 'explicitly adopts previous-helper output for same-key renewal' {
        & $scriptPath -OutputRoot $root -TestOnlySkipAcl -Subject 'pier.test'
        $firstSpki = Get-Content (Join-Path $root 'host.spki-sha256.txt') -Raw
        Remove-Item (Join-Path $root 'host.generated-by-arcen.txt')
        $refused = $false
        try { & $scriptPath -OutputRoot $root -TestOnlySkipAcl -Renew } catch {
            $refused = $true
        }
        $refused | Should Be $true
        & $scriptPath -OutputRoot $root -TestOnlySkipAcl -Renew -AdoptLegacyHelperPair
        (Get-Content (Join-Path $root 'host.spki-sha256.txt') -Raw) |
            Should Be $firstSpki
        Test-Path (Join-Path $root 'host.generated-by-arcen.txt') | Should Be $true
    }

    It 'maps deprecated Force before rejecting incompatible renewal modes' {
        $refused = $false
        try {
            & $scriptPath -OutputRoot $root -TestOnlySkipAcl -Renew -Force
        } catch {
            $refused = $true
        }
        $refused | Should Be $true
    }

    It 'refuses partial and unmarked enterprise material' {
        Set-Content -LiteralPath (Join-Path $root 'host.crt') -Value 'partial'
        $partialRefused = $false
        try { & $scriptPath -OutputRoot $root -TestOnlySkipAcl } catch {
            $partialRefused = $true
        }
        $partialRefused | Should Be $true
        Remove-Item (Join-Path $root 'host.crt')
        & $scriptPath -OutputRoot $root -TestOnlySkipAcl -Subject 'pier.test'
        Remove-Item (Join-Path $root 'host.generated-by-arcen.txt')
        $enterpriseRefused = $false
        try { & $scriptPath -OutputRoot $root -TestOnlySkipAcl -Renew } catch {
            $enterpriseRefused = $true
        }
        $enterpriseRefused | Should Be $true
    }

    It 'binds helper ownership to the current certificate and key pair' {
        & $scriptPath -OutputRoot $root -TestOnlySkipAcl -Subject 'pier.test'
        $markerPath = Join-Path $root 'host.generated-by-arcen.txt'
        $marker = Get-Content $markerPath -Raw | ConvertFrom-Json
        $marker.certificate_sha256 = ('00:' * 31) + '00'
        $marker | ConvertTo-Json -Compress | Set-Content $markerPath
        . $scriptPath -LibraryOnly

        $refused = $false
        try {
            Assert-ManagedPair `
                -MarkerPath $markerPath `
                -CertificatePath (Join-Path $root 'host.crt') `
                -PrivateKeyPath (Join-Path $root 'host.key')
        } catch {
            $refused = $true
        }
        $refused | Should Be $true
    }

    It 'serializes concurrent transactions with an exclusive lock' {
        . $scriptPath -LibraryOnly
        $script:SkipAcl = $true
        $first = Enter-TlsTransactionLock -Root $root
        try {
            $refused = $false
            try {
                $second = Enter-TlsTransactionLock -Root $root
                $second.Dispose()
            } catch {
                $refused = $true
            }
            $refused | Should Be $true
        } finally {
            $first.Dispose()
        }
    }

    It 'exports fingerprints for RSA enterprise certificates' {
        . $scriptPath -LibraryOnly
        $rsa = [Security.Cryptography.RSA]::Create(2048)
        $certificate = $null
        try {
            $request = [Security.Cryptography.X509Certificates.CertificateRequest]::new(
                'CN=rsa.test',
                $rsa,
                [Security.Cryptography.HashAlgorithmName]::SHA256,
                [Security.Cryptography.RSASignaturePadding]::Pkcs1
            )
            $certificate = $request.CreateSelfSigned(
                [DateTimeOffset]::UtcNow.AddMinutes(-1),
                [DateTimeOffset]::UtcNow.AddDays(1)
            )
            $pins = Get-CertificatePins -Certificate $certificate
            $pins.Certificate | Should Match '^([0-9A-F]{2}:){31}[0-9A-F]{2}$'
            $pins.Spki | Should Match '^sha256/'
        } finally {
            if ($certificate) { $certificate.Dispose() }
            $rsa.Dispose()
        }
    }

    It 'recovers an interrupted backup transaction before generate-if-missing' {
        & $scriptPath -OutputRoot $root -TestOnlySkipAcl -Subject 'pier.test'
        . $scriptPath -LibraryOnly
        $script:SkipAcl = $true
        $token = 'c' * 32
        $files = @(Get-TlsTransactionLayout -Root $root -Token $token)
        $destination = Join-Path $root 'host.crt'
        $certFile = $files | Where-Object { $_.destination -eq $destination }
        foreach ($file in $files) {
            Move-Item $file.destination $file.backup
        }
        $backup = $certFile.backup
        $stage = $certFile.stage
        Set-Content $stage 'incomplete'
        @{
            version = 1
            token = $token
            phase = 'backed_up'
            files = $files
        } | ConvertTo-Json -Depth 5 |
            Set-Content (Join-Path $root 'host-cert.transaction.json')
        & $scriptPath -OutputRoot $root -TestOnlySkipAcl
        Test-Path $destination | Should Be $true
        Test-Path $backup | Should Be $false
        Test-Path $stage | Should Be $false
    }

    It 'keeps the old destination when recovery interrupts during staging' {
        & $scriptPath -OutputRoot $root -TestOnlySkipAcl -Subject 'pier.test'
        . $scriptPath -LibraryOnly
        $script:SkipAcl = $true
        $token = 'd' * 32
        $files = @(Get-TlsTransactionLayout -Root $root -Token $token)
        $destination = Join-Path $root 'host.crt'
        $original = [Convert]::ToBase64String([IO.File]::ReadAllBytes($destination))
        $certFile = $files | Where-Object { $_.destination -eq $destination }
        $stage = $certFile.stage
        Set-Content $stage 'incomplete'
        @{
            version = 1
            token = $token
            phase = 'staging'
            files = $files
        } | ConvertTo-Json -Depth 5 |
            Set-Content (Join-Path $root 'host-cert.transaction.json')
        & $scriptPath -OutputRoot $root -TestOnlySkipAcl
        [Convert]::ToBase64String([IO.File]::ReadAllBytes($destination)) |
            Should Be $original
        Test-Path $stage | Should Be $false
    }

    It 'refuses journal paths outside the TLS transaction layout' {
        & $scriptPath -OutputRoot $root -TestOnlySkipAcl -Subject 'pier.test'
        . $scriptPath -LibraryOnly
        $script:SkipAcl = $true
        $token = 'e' * 32
        $files = @(Get-TlsTransactionLayout -Root $root -Token $token)
        $outside = Join-Path $TestDrive 'outside.txt'
        Set-Content $outside 'retain'
        $files[0].destination = $outside
        @{
            version = 1
            token = $token
            phase = 'staging'
            files = $files
        } | ConvertTo-Json -Depth 5 |
            Set-Content (Join-Path $root 'host-cert.transaction.json')
        $refused = $false
        try { & $scriptPath -OutputRoot $root -TestOnlySkipAcl } catch {
            $refused = $true
        }
        $refused | Should Be $true
        (Get-Content $outside -Raw).Trim() | Should Be 'retain'
    }

    It 'exposes only SYSTEM and Administrators in the ACL plan' {
        . $scriptPath -LibraryOnly
        $plan = Get-RestrictedAclPlan
        ($plan.Sid -join ',') | Should Be 'S-1-5-18,S-1-5-32-544'
        (($plan.Rights | Select-Object -Unique) -join ',') | Should Be 'FullControl'
    }

    It 'refuses a reparse-point output root when symlink creation is available' {
        $target = Join-Path $TestDrive 'target'
        $link = Join-Path $TestDrive 'link'
        New-Item -ItemType Directory $target | Out-Null
        try {
            New-Item -ItemType SymbolicLink -Path $link -Target $target -ErrorAction Stop |
                Out-Null
        } catch {
            return
        }
        { & $scriptPath -OutputRoot $link -TestOnlySkipAcl } | Should Throw
    }
}
