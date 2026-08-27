$ErrorActionPreference = "Stop"
$repository = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot "..\..")).Path
$source = Join-Path $repository "hosts\windows\driver\arcen-iddcx"
$verifier = Join-Path $source "verify-driver-source.ps1"
$scratch = Join-Path $repository (
    "target\iddcx-source-verifier-" + [Guid]::NewGuid().ToString("N")
)

& $verifier -SourceDirectory $source
New-Item -ItemType Directory -Path $scratch -Force | Out-Null
Copy-Item -Path (Join-Path $source "*") -Destination $scratch -Recurse
try {
    Remove-Item -LiteralPath (Join-Path $scratch "arcen_iddcx_driver.cpp")
    try {
        & $verifier -SourceDirectory $scratch
        throw "IddCx source verifier accepted a missing implementation"
    }
    catch {
        if ($_.Exception.Message -notmatch "manifest mismatch") {
            throw
        }
    }

    Copy-Item -LiteralPath (Join-Path $source "arcen_iddcx_driver.cpp") `
        -Destination $scratch
    New-Item -ItemType File -Path (Join-Path $scratch "payload.sys") | Out-Null
    try {
        & $verifier -SourceDirectory $scratch
        throw "IddCx source verifier accepted an extra payload"
    }
    catch {
        if ($_.Exception.Message -notmatch "manifest mismatch") {
            throw
        }
    }

    Remove-Item -LiteralPath (Join-Path $scratch "payload.sys")
    Add-Content -LiteralPath (Join-Path $scratch "arcen_iddcx_model.h") `
        -Value "// unauthorized change"
    try {
        & $verifier -SourceDirectory $scratch
        throw "IddCx source verifier accepted changed source"
    }
    catch {
        if ($_.Exception.Message -notmatch "hash mismatch") {
            throw
        }
    }
}
finally {
    Remove-Item -LiteralPath $scratch -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host "Arcen IddCx source verifier fail-closed tests passed."
