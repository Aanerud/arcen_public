# Arcen Windows Client Packaging

> **STATUS (2026-07-14): DORMANT HISTORICAL SCAFFOLD.** The future product is
> the Windows Deck. This scaffold still targets the legacy
> `arcen-client-windows` package and artifact names shown below; that crate is
> out of workspace `members`, so the build does not run until the milestone is
> reactivated, renamed, and added to Windows CI.

This scaffold is designed to build an independently installable x64 MSI with
WiX Toolset v4.

## Build

On a Windows build host with Rust, the MSVC target toolchain, and WiX v4:

```powershell
.\build.ps1 -Version 0.1.0 -Manufacturer "<Release/Security-approved legal identity>"
```

The unsigned MSI is written to `dist\arcen-client-windows-<version>-x64.msi`.

Validate the PowerShell syntax, XML-safe build inputs, MSI authoring invariants,
and secure timestamp default without compiling:

```powershell
.\validate.ps1
```

## Signing

Signing is Release/Security-owned. Provide the signing tool and certificate
selection through protected runner environment variables, then request the
two-stage build:

```powershell
$env:ARCEN_SIGNTOOL_PATH = "C:\Program Files (x86)\Windows Kits\10\bin\<sdk>\x64\signtool.exe"
$env:ARCEN_SIGNING_CERT_THUMBPRINT = "<certificate thumbprint>"
.\build.ps1 -Version 0.1.0 -Manufacturer "<approved legal identity>" -Sign
```

`build.ps1 -Sign` signs and verifies `arcen-client-windows.exe` before WiX
consumes it, then signs and verifies the resulting MSI. WiX is never allowed to
package an unverified executable in a signed build. `sign.ps1` remains available
for Release/Security-controlled recovery workflows.

`ARCEN_TIMESTAMP_URL` may override the default HTTPS RFC 3161 timestamp service.
An explicitly configured HTTP service is rejected unless
`ARCEN_ALLOW_INSECURE_TIMESTAMP=1`; that exception requires Release/Security
approval.
No key material, password, certificate, or thumbprint belongs in this directory.
Installer identity, signing policy, timestamp authority, and distribution require
Release/Security approval before release.
