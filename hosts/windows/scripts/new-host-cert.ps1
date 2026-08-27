#Requires -Version 7.0
<#
.SYNOPSIS
    Generates or explicitly renews Arcen Pier PEM TLS material.

.DESCRIPTION
    With no arguments, generates only when both host.crt and host.key are absent.
    A complete existing pair is never overwritten. -Renew issues a new certificate
    with the existing P-256 key. -ForceNewKey explicitly changes trust by issuing
    with a new key. The helper never uses or mutates a Windows certificate store.
#>
[CmdletBinding()]
param(
    [string]$Subject,
    [string[]]$ExtraDns = @(),
    [string[]]$ExtraIp = @(),
    [switch]$Renew,
    [switch]$AdoptLegacyHelperPair,
    [switch]$ForceNewKey,
    [switch]$Force,
    [string]$OutputRoot,
    [switch]$LibraryOnly,
    [switch]$TestOnlySkipAcl
)

$ErrorActionPreference = 'Stop'
$script:GeneratedMarkerVersion = 3

function Test-IsAdministrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Assert-NoReparsePath {
    param(
        [Parameter(Mandatory)][string]$Path,
        [switch]$AllowMissingLeaf
    )

    $full = [IO.Path]::GetFullPath($Path)
    $root = [IO.Path]::GetPathRoot($full)
    $relative = $full.Substring($root.Length)
    $current = $root
    $parts = $relative.Split(
        [IO.Path]::DirectorySeparatorChar,
        [StringSplitOptions]::RemoveEmptyEntries
    )
    for ($index = 0; $index -lt $parts.Count; $index++) {
        $current = Join-Path $current $parts[$index]
        if (-not (Test-Path -LiteralPath $current)) {
            if ($AllowMissingLeaf -and $index -eq $parts.Count - 1) { return }
            continue
        }
        $item = Get-Item -LiteralPath $current -Force
        if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw "Refusing reparse point in TLS transaction path."
        }
    }
}

function Get-RestrictedAclPlan {
    return @(
        @{ Sid = 'S-1-5-18'; Rights = 'FullControl'; Type = 'Allow' },
        @{ Sid = 'S-1-5-32-544'; Rights = 'FullControl'; Type = 'Allow' }
    )
}

function Set-RestrictedDirectory {
    param([Parameter(Mandatory)][string]$Path)

    Assert-NoReparsePath -Path $Path -AllowMissingLeaf
    if (-not (Test-Path -LiteralPath $Path)) {
        New-Item -ItemType Directory -Path $Path | Out-Null
    }
    Assert-NoReparsePath -Path $Path
    if ($script:SkipAcl) { return }
    $acl = [Security.AccessControl.DirectorySecurity]::new()
    $acl.SetAccessRuleProtection($true, $false)
    $admin = [Security.Principal.SecurityIdentifier]::new('S-1-5-32-544')
    $acl.SetOwner($admin)
    $inheritance = [Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
    foreach ($entry in Get-RestrictedAclPlan) {
        $sid = [Security.Principal.SecurityIdentifier]::new($entry.Sid)
        $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
            $sid,
            $entry.Rights,
            $inheritance,
            [Security.AccessControl.PropagationFlags]::None,
            $entry.Type
        ))
    }
    Set-Acl -LiteralPath $Path -AclObject $acl
}

function Set-RestrictedFile {
    param([Parameter(Mandatory)][string]$Path)

    Assert-NoReparsePath -Path $Path
    if ($script:SkipAcl) { return }
    $acl = [Security.AccessControl.FileSecurity]::new()
    $acl.SetAccessRuleProtection($true, $false)
    $admin = [Security.Principal.SecurityIdentifier]::new('S-1-5-32-544')
    $acl.SetOwner($admin)
    foreach ($entry in Get-RestrictedAclPlan) {
        $sid = [Security.Principal.SecurityIdentifier]::new($entry.Sid)
        $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
            $sid, $entry.Rights, $entry.Type
        ))
    }
    Set-Acl -LiteralPath $Path -AclObject $acl
}

function Write-DurableBytes {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][byte[]]$Bytes
    )

    Assert-NoReparsePath -Path $Path -AllowMissingLeaf
    $stream = [IO.FileStream]::new(
        $Path,
        [IO.FileMode]::CreateNew,
        [IO.FileAccess]::Write,
        [IO.FileShare]::None,
        4096,
        [IO.FileOptions]::WriteThrough
    )
    try {
        $stream.Write($Bytes, 0, $Bytes.Length)
        $stream.Flush($true)
    } finally {
        $stream.Dispose()
    }
}

function Write-DurableText {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string]$Text
    )

    $bytes = [Text.UTF8Encoding]::new($false).GetBytes($Text)
    try {
        Write-DurableBytes -Path $Path -Bytes $bytes
    } finally {
        [Array]::Clear($bytes, 0, $bytes.Length)
    }
}

function Write-Journal {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)]$Value
    )

    $temporary = "$Path.new"
    if (Test-Path -LiteralPath $temporary) {
        Assert-NoReparsePath -Path $temporary
        Remove-Item -LiteralPath $temporary -Force
    }
    Write-DurableText -Path $temporary -Text (
        $Value | ConvertTo-Json -Depth 6 -Compress
    )
    Set-RestrictedFile -Path $temporary
    if (Test-Path -LiteralPath $Path) {
        Assert-NoReparsePath -Path $Path
        [IO.File]::Move($temporary, $Path, $true)
    } else {
        [IO.File]::Move($temporary, $Path)
    }
}

function Enter-TlsTransactionLock {
    param([Parameter(Mandatory)][string]$Root)

    $path = Join-Path $Root 'host-cert.transaction.lock'
    Assert-NoReparsePath -Path $path -AllowMissingLeaf
    try {
        $stream = [IO.FileStream]::new(
            $path,
            [IO.FileMode]::OpenOrCreate,
            [IO.FileAccess]::ReadWrite,
            [IO.FileShare]::None,
            128,
            [IO.FileOptions]::WriteThrough
        )
    } catch {
        throw 'Another host-certificate transaction is active.'
    }
    try {
        Set-RestrictedFile -Path $path
        return ,$stream
    } catch {
        $stream.Dispose()
        throw
    }
}

function Get-TlsTransactionLayout {
    param(
        [Parameter(Mandatory)][string]$Root,
        [Parameter(Mandatory)][string]$Token
    )

    $names = @(
        'host.crt',
        'host.key',
        'host.fingerprint.txt',
        'host.spki-sha256.txt',
        'host.generated-by-arcen.txt'
    )
    return @($names | ForEach-Object {
        [ordered]@{
            destination = Join-Path $Root $_
            stage = Join-Path $Root ".$_.$Token.stage"
            backup = Join-Path $Root ".$_.$Token.backup"
        }
    })
}

function Get-ValidatedJournalFiles {
    param(
        [Parameter(Mandatory)][string]$Root,
        [Parameter(Mandatory)]$Journal
    )

    $token = [string]$Journal.token
    if ($token -cnotmatch '^[0-9a-f]{32}$') {
        throw 'Refusing TLS transaction journal with an invalid token.'
    }
    $expected = @(Get-TlsTransactionLayout -Root $Root -Token $token)
    $actual = @($Journal.files)
    if ($actual.Count -ne $expected.Count) {
        throw 'Refusing TLS transaction journal with an incomplete file set.'
    }
    $comparison = [StringComparer]::OrdinalIgnoreCase
    $seen = [Collections.Generic.HashSet[string]]::new($comparison)
    foreach ($file in $actual) {
        $destination = [IO.Path]::GetFullPath([string]$file.destination)
        $match = $expected | Where-Object {
            $comparison.Equals($_.destination, $destination)
        }
        if (@($match).Count -ne 1 -or -not $seen.Add($destination)) {
            throw 'Refusing TLS transaction journal with an unexpected destination.'
        }
        foreach ($property in @('stage', 'backup')) {
            $candidate = [IO.Path]::GetFullPath([string]$file.$property)
            if (-not $comparison.Equals($match.$property, $candidate)) {
                throw 'Refusing TLS transaction journal with an unexpected transaction path.'
            }
        }
    }
    return $actual
}

function Recover-TlsTransaction {
    param([Parameter(Mandatory)][string]$Root)

    $journalPath = Join-Path $Root 'host-cert.transaction.json'
    if (-not (Test-Path -LiteralPath $journalPath)) { return }
    Assert-NoReparsePath -Path $journalPath
    Set-RestrictedFile -Path $journalPath
    $journal = Get-Content -LiteralPath $journalPath -Raw | ConvertFrom-Json
    if ($journal.version -ne 1 -or -not $journal.files) {
        throw 'Refusing malformed TLS transaction journal.'
    }

    if ($journal.phase -notin @('staging', 'staged', 'backed_up', 'committed')) {
        throw 'Refusing TLS transaction journal with an unknown phase.'
    }
    $files = @(Get-ValidatedJournalFiles -Root $Root -Journal $journal)
    if ($journal.phase -ne 'committed') {
        foreach ($file in $files) {
            Assert-NoReparsePath -Path $file.destination -AllowMissingLeaf
            Assert-NoReparsePath -Path $file.stage -AllowMissingLeaf
            Assert-NoReparsePath -Path $file.backup -AllowMissingLeaf
            if (Test-Path -LiteralPath $file.backup) {
                if (Test-Path -LiteralPath $file.destination) {
                    Remove-Item -LiteralPath $file.destination -Force
                }
                Move-Item -LiteralPath $file.backup -Destination $file.destination
            } elseif ($journal.phase -eq 'backed_up' -and
                (Test-Path -LiteralPath $file.destination)) {
                # This destination did not exist before the transaction.
                Remove-Item -LiteralPath $file.destination -Force
            }
        }
    }
    foreach ($file in $files) {
        foreach ($candidate in @($file.stage, $file.backup)) {
            if (Test-Path -LiteralPath $candidate) {
                Assert-NoReparsePath -Path $candidate
                Remove-Item -LiteralPath $candidate -Force
            }
        }
    }
    Remove-Item -LiteralPath $journalPath -Force
    Write-Host 'Recovered an interrupted TLS material transaction.'
}

function New-PemBlock {
    param(
        [Parameter(Mandatory)][string]$Header,
        [Parameter(Mandatory)][byte[]]$Body
    )

    $base64 = [Convert]::ToBase64String(
        $Body,
        [Base64FormattingOptions]::InsertLineBreaks
    )
    return "-----BEGIN $Header-----`n$base64`n-----END $Header-----`n"
}

function Get-CertificatePins {
    param([Parameter(Mandatory)][Security.Cryptography.X509Certificates.X509Certificate2]$Certificate)

    $sha = [Security.Cryptography.SHA256]::Create()
    $publicKey = $null
    $spki = $null
    try {
        $certificateHash = $sha.ComputeHash($Certificate.RawData)
        $publicKey = [Security.Cryptography.X509Certificates.ECDsaCertificateExtensions]::GetECDsaPublicKey(
            $Certificate
        )
        if (-not $publicKey) {
            $publicKey = [Security.Cryptography.X509Certificates.RSACertificateExtensions]::GetRSAPublicKey(
                $Certificate
            )
        }
        if (-not $publicKey) {
            throw 'The host certificate public-key algorithm cannot export SPKI.'
        }
        $spki = $publicKey.ExportSubjectPublicKeyInfo()
        $spkiHash = $sha.ComputeHash($spki)
        return @{
            Certificate = (($certificateHash | ForEach-Object { $_.ToString('X2') }) -join ':')
            Spki = 'sha256/' + [Convert]::ToBase64String($spkiHash)
        }
    } finally {
        if ($publicKey) { $publicKey.Dispose() }
        if ($spki) { [Array]::Clear($spki, 0, $spki.Length) }
        $sha.Dispose()
    }
}

function New-ManagedMarker {
    param([Parameter(Mandatory)]$Pins)

    return [ordered]@{
        version = $script:GeneratedMarkerVersion
        certificate_sha256 = $Pins.Certificate
        spki_sha256 = $Pins.Spki
    } | ConvertTo-Json -Compress
}

function Assert-ManagedPair {
    param(
        [Parameter(Mandatory)][string]$MarkerPath,
        [Parameter(Mandatory)][string]$CertificatePath,
        [Parameter(Mandatory)][string]$PrivateKeyPath,
        [string]$LegacyFingerprintPath,
        [switch]$AllowLegacyAdoption
    )

    $certificate = [Security.Cryptography.X509Certificates.X509Certificate2]::CreateFromPem(
        [IO.File]::ReadAllText($CertificatePath)
    )
    $pair = $null
    try {
        $pair = [Security.Cryptography.X509Certificates.X509Certificate2]::CreateFromPemFile(
            $CertificatePath,
            $PrivateKeyPath
        )
        $pins = Get-CertificatePins -Certificate $certificate
        if (-not (Test-Path -LiteralPath $MarkerPath)) {
            if (-not $AllowLegacyAdoption -or
                -not $LegacyFingerprintPath -or
                -not (Test-Path -LiteralPath $LegacyFingerprintPath)) {
                throw 'Refusing to overwrite enterprise/custom PEM material not marked as helper-generated.'
            }
            Assert-NoReparsePath -Path $LegacyFingerprintPath
            $legacyFingerprint = (
                Get-Content -LiteralPath $LegacyFingerprintPath -Raw
            ).Trim()
            if ($legacyFingerprint -cne "SHA256 Fingerprint=$($pins.Certificate)") {
                throw 'Refusing legacy adoption because its fingerprint does not match the certificate.'
            }
            Write-Warning 'Explicitly adopting output from the previous Arcen helper for same-key renewal.'
            return
        }
        Assert-NoReparsePath -Path $MarkerPath
        $marker = Get-Content -LiteralPath $MarkerPath -Raw | ConvertFrom-Json
        if ($marker.version -ne $script:GeneratedMarkerVersion) {
            throw 'Refusing an unsupported or stale helper ownership marker.'
        }
        if ($marker.certificate_sha256 -cne $pins.Certificate -or
            $marker.spki_sha256 -cne $pins.Spki) {
            throw 'Refusing to overwrite TLS material that no longer matches its helper ownership marker.'
        }
    } finally {
        if ($pair) { $pair.Dispose() }
        $certificate.Dispose()
    }
}

function Get-SanInputs {
    param(
        [string[]]$AdditionalDns,
        [string[]]$AdditionalIp
    )

    $computer = [Net.Dns]::GetHostName()
    $dns = [Collections.Generic.HashSet[string]]::new(
        [StringComparer]::OrdinalIgnoreCase
    )
    [void]$dns.Add($computer)
    try {
        $fqdn = [Net.Dns]::GetHostEntry($computer).HostName
        if ($fqdn) { [void]$dns.Add($fqdn) }
    } catch {}
    foreach ($name in $AdditionalDns) {
        if ($name) { [void]$dns.Add($name) }
    }

    $ips = [Collections.Generic.HashSet[string]]::new()
    Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue |
        Where-Object {
            -not $_.IPAddress.StartsWith('127.') -and
            $_.PrefixOrigin -ne 'WellKnown'
        } |
        ForEach-Object { [void]$ips.Add($_.IPAddress) }
    foreach ($address in $AdditionalIp) {
        if (-not $address) { continue }
        $parsed = $null
        if (-not [Net.IPAddress]::TryParse($address, [ref]$parsed) -or
            [Net.IPAddress]::IsLoopback($parsed)) {
            throw 'ExtraIp must contain non-loopback IP addresses.'
        }
        [void]$ips.Add($parsed.ToString())
    }
    return @{ Computer = $computer; Dns = $dns; Ips = $ips }
}

function New-HostCertificateSet {
    param(
        [Parameter(Mandatory)][string]$CommonName,
        [Parameter(Mandatory)]$Sans,
        [Security.Cryptography.ECDsa]$ExistingKey
    )

    $key = $ExistingKey
    $ownsKey = $null -eq $key
    if ($ownsKey) {
        $key = [Security.Cryptography.ECDsa]::Create()
        $key.GenerateKey([Security.Cryptography.ECCurve+NamedCurves]::nistP256)
    }
    $certificate = $null
    $pkcs8 = $null
    try {
        $request = [Security.Cryptography.X509Certificates.CertificateRequest]::new(
            [Security.Cryptography.X509Certificates.X500DistinguishedName]::new("CN=$CommonName"),
            $key,
            [Security.Cryptography.HashAlgorithmName]::SHA256
        )
        $request.CertificateExtensions.Add(
            [Security.Cryptography.X509Certificates.X509BasicConstraintsExtension]::new(
                $false, $false, 0, $true
            )
        )
        $request.CertificateExtensions.Add(
            [Security.Cryptography.X509Certificates.X509KeyUsageExtension]::new(
                [Security.Cryptography.X509Certificates.X509KeyUsageFlags]::DigitalSignature,
                $true
            )
        )
        $oids = [Security.Cryptography.OidCollection]::new()
        [void]$oids.Add([Security.Cryptography.Oid]::new('1.3.6.1.5.5.7.3.1'))
        $request.CertificateExtensions.Add(
            [Security.Cryptography.X509Certificates.X509EnhancedKeyUsageExtension]::new(
                $oids, $true
            )
        )
        $sanBuilder = [Security.Cryptography.X509Certificates.SubjectAlternativeNameBuilder]::new()
        foreach ($name in $Sans.Dns) { $sanBuilder.AddDnsName($name) }
        foreach ($address in $Sans.Ips) {
            $sanBuilder.AddIpAddress([Net.IPAddress]::Parse($address))
        }
        $request.CertificateExtensions.Add($sanBuilder.Build($true))
        $certificate = $request.CreateSelfSigned(
            [DateTimeOffset]::UtcNow.AddMinutes(-5),
            [DateTimeOffset]::UtcNow.AddDays(825)
        )
        $pkcs8 = $key.ExportPkcs8PrivateKey()
        return @{
            CertificatePem = New-PemBlock -Header 'CERTIFICATE' -Body $certificate.RawData
            PrivateKeyPem = New-PemBlock -Header 'PRIVATE KEY' -Body $pkcs8
            Pins = Get-CertificatePins -Certificate $certificate
        }
    } finally {
        if ($certificate) { $certificate.Dispose() }
        if ($pkcs8) { [Array]::Clear($pkcs8, 0, $pkcs8.Length) }
        if ($ownsKey -and $key) { $key.Dispose() }
    }
}

function Publish-TlsSet {
    param(
        [Parameter(Mandatory)][string]$Root,
        [Parameter(Mandatory)]$Set
    )

    $token = [Guid]::NewGuid().ToString('N')
    $contents = [ordered]@{
        'host.crt' = $Set.CertificatePem
        'host.key' = $Set.PrivateKeyPem
        'host.fingerprint.txt' = "SHA256 Fingerprint=$($Set.Pins.Certificate)`n"
        'host.spki-sha256.txt' = "$($Set.Pins.Spki)`n"
        'host.generated-by-arcen.txt' = "$(New-ManagedMarker -Pins $Set.Pins)`n"
    }
    $files = @(Get-TlsTransactionLayout -Root $Root -Token $token)
    $journalPath = Join-Path $Root 'host-cert.transaction.json'
    $journal = [ordered]@{ version = 1; token = $token; phase = 'staging'; files = $files }
    Write-Journal -Path $journalPath -Value $journal
    try {
        for ($index = 0; $index -lt $files.Count; $index++) {
            Write-DurableText -Path $files[$index].stage -Text $contents[$contents.Keys[$index]]
            Set-RestrictedFile -Path $files[$index].stage
        }
        $journal.phase = 'staged'
        Write-Journal -Path $journalPath -Value $journal
        foreach ($file in $files) {
            Assert-NoReparsePath -Path $file.destination -AllowMissingLeaf
            Assert-NoReparsePath -Path $file.backup -AllowMissingLeaf
            if (Test-Path -LiteralPath $file.destination) {
                Move-Item -LiteralPath $file.destination -Destination $file.backup
            }
        }
        $journal.phase = 'backed_up'
        Write-Journal -Path $journalPath -Value $journal
        foreach ($file in $files) {
            Move-Item -LiteralPath $file.stage -Destination $file.destination
        }
        $journal.phase = 'committed'
        Write-Journal -Path $journalPath -Value $journal
        foreach ($file in $files) {
            if (Test-Path -LiteralPath $file.backup) {
                Remove-Item -LiteralPath $file.backup -Force
            }
        }
        Remove-Item -LiteralPath $journalPath -Force
    } catch {
        Recover-TlsTransaction -Root $Root
        throw
    }
}

function Show-CurrentPins {
    param([Parameter(Mandatory)][string]$CertificatePath)

    $certificate = [Security.Cryptography.X509Certificates.X509Certificate2]::CreateFromPem(
        [IO.File]::ReadAllText($CertificatePath)
    )
    try {
        $pins = Get-CertificatePins -Certificate $certificate
        Write-Host "Certificate SHA-256: $($pins.Certificate)"
        Write-Host "SPKI SHA-256       : $($pins.Spki)"
    } finally {
        $certificate.Dispose()
    }
}

if ($LibraryOnly) { return }
if ($Force) {
    Write-Warning '-Force is deprecated and maps to -ForceNewKey; this changes Deck trust.'
    $ForceNewKey = $true
}
if ($Renew -and $ForceNewKey) {
    throw '-Renew and -ForceNewKey are mutually exclusive.'
}
if ($AdoptLegacyHelperPair -and -not $Renew) {
    throw '-AdoptLegacyHelperPair requires -Renew.'
}

$usingDefaultRoot = -not $OutputRoot
if ($TestOnlySkipAcl -and $usingDefaultRoot) {
    throw '-TestOnlySkipAcl requires an explicit non-ProgramData OutputRoot.'
}
$script:SkipAcl = [bool]$TestOnlySkipAcl
if ($usingDefaultRoot) {
    if (-not (Test-IsAdministrator)) {
        throw 'Run as Administrator when writing under ProgramData.'
    }
    $OutputRoot = Join-Path $env:ProgramData 'Arcen\tls'
}
$OutputRoot = [IO.Path]::GetFullPath($OutputRoot)
Set-RestrictedDirectory -Path $OutputRoot
$transactionLock = Enter-TlsTransactionLock -Root $OutputRoot
try {
Recover-TlsTransaction -Root $OutputRoot

$certPath = Join-Path $OutputRoot 'host.crt'
$keyPath = Join-Path $OutputRoot 'host.key'
$markerPath = Join-Path $OutputRoot 'host.generated-by-arcen.txt'
$fingerprintPath = Join-Path $OutputRoot 'host.fingerprint.txt'
$hasCert = Test-Path -LiteralPath $certPath
$hasKey = Test-Path -LiteralPath $keyPath
if ($hasCert -xor $hasKey) {
    throw 'A partial host.crt/host.key pair exists. Restore the missing file or remove both after recovery review.'
}
if ($hasCert -and -not ($Renew -or $ForceNewKey)) {
    Write-Host 'Existing complete TLS pair retained; no files were overwritten.'
    Show-CurrentPins -CertificatePath $certPath
    return
}
if (($Renew -or $ForceNewKey) -and -not $hasCert) {
    throw 'Explicit renewal/rekey requires an existing helper-generated certificate pair.'
}
if ($Renew -or $ForceNewKey) {
    Assert-ManagedPair `
        -MarkerPath $markerPath `
        -CertificatePath $certPath `
        -PrivateKeyPath $keyPath `
        -LegacyFingerprintPath $fingerprintPath `
        -AllowLegacyAdoption:$AdoptLegacyHelperPair
}

$sans = Get-SanInputs -AdditionalDns $ExtraDns -AdditionalIp $ExtraIp
if (-not $Subject) { $Subject = $sans.Computer }
$existingKey = $null
try {
    if ($Renew) {
        Assert-NoReparsePath -Path $keyPath
        $existingKey = [Security.Cryptography.ECDsa]::Create()
        $keyPem = [IO.File]::ReadAllText($keyPath)
        try {
            $existingKey.ImportFromPem($keyPem)
        } finally {
            $keyPem = $null
        }
    }
    if ($ForceNewKey) {
        Write-Warning 'Generating a new key changes SPKI trust; update every Deck pin.'
    }
    $set = New-HostCertificateSet `
        -CommonName $Subject `
        -Sans $sans `
        -ExistingKey $existingKey
    Publish-TlsSet -Root $OutputRoot -Set $set
} finally {
    if ($existingKey) { $existingKey.Dispose() }
}

Write-Host "Wrote: $certPath"
Write-Host "Wrote: $keyPath"
Write-Host "Wrote: $(Join-Path $OutputRoot 'host.fingerprint.txt')"
Write-Host "Wrote: $(Join-Path $OutputRoot 'host.spki-sha256.txt')"
Write-Host "Certificate SHA-256: $($set.Pins.Certificate)"
Write-Host "SPKI SHA-256       : $($set.Pins.Spki)"
} finally {
    $transactionLock.Dispose()
}
