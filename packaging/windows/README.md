# Windows Packaging

The active host artifacts are `arcen-pier-windows` and
`arcen-credential-provider`; installer/service registration, signing,
provenance, upgrades, and uninstall behavior remain Release/Security work.

`client/` is a dormant legacy scaffold for the future Windows Deck. It still
uses the pre-Pier/Deck package identity `arcen-client-windows`, is out of
workspace `members`, and is not built by current CI. Host and future client
artifacts must remain independently installable.

`pier.json` is the Windows template for the shared common Pier schema. Install
it as `%ProgramData%\Arcen\pier.json` after selecting platform values; its
common sections match Linux `/etc/arcen/pier.json`. See
[`docs/operations/pier-configuration.md`](../../docs/operations/pier-configuration.md).
The package ships `logging.level: 0` plus shared QoS defaults. Development
overrides (currently Level 2 on pier-windows.example.internal) belong in deployment configuration,
never in this production template.

The single Pier/installer payload includes all Windows streaming pipelines:
eight-bit DDA/WGC for Auto/Speed, mandatory WGC FP16 scRGB for ten-bit
Grading, and the exact-target HDR EDID/state plus FP16-to-PQ path for HDR.
These are runtime-selected paths inside the same signed-in session agent and
capenc binary, not optional components. Rebuild the Pier before rebuilding the
embedding installer.

## Native codec/static-runtime boundary

`hosts\windows\build.cmd` is the active source-build entry point. It requires an
x64 Visual Studio developer environment, exact MSVC Rust host, CMake, C/C++
compiler/archive/inspection tools; sets Rust `+crt-static` and native `/MT`;
and builds the fused Pier with `nvenc,software-h264`.

`verify-static-runtime.ps1` inspects bundled Opus static archive members and
every packaged PE. It rejects dynamic MSVC CRT/C++ directives, GNU
compiler/thread runtimes, OpenH264/Opus runtime DLLs, and nested codec payloads.
Passing that inspection is not Authenticode signing or a release approval. The
package remains blocked on signing, complete SBOM/notices, H.264 distribution
review, and physical acceptance.

Audio Input has an exact release manifest for the signed
`driver\payload\arcen-microphone.{sys,inf,cat}` set and its deterministic
install/uninstall/upgrade/rollback scripts. Driver source has an exact
allowlist plus a portable MSVC test over the production ring. When WDK headers
are available, `build.cmd` also builds the PortCls project without signing and
does not promote it. Release staging occurs only when
`ARCEN_SIGNED_MICROPHONE_PACKAGE` names an exact externally signed SYS/INF/CAT
directory; both staging and installation validate signatures. See
`docs/operations/audio-input.md`.
The future optional microphone-driver installer flow is documentation-only in
[`docs/operations/windows-installer.md`](../../docs/operations/windows-installer.md).

The first-party IddCx source lives at
`hosts/windows/driver/arcen-iddcx`. Its exact source manifest and portable
lifecycle contracts run in CI. `build-driver.ps1` may produce unsigned WDK
evidence on an approved build host, but it never signs, installs, stages, or
deploys the provider. No IddCx DLL/INF/CAT is present in the Windows package
manifest. Adding one requires Release/Security approval of the exact signed
payload, EV/Partner Center attestation flow, servicing/rollback behavior, and
the Windows Server WHQL/HLK decision.

## `host/` — Pier-owned registration scripts

`host/eventlog-source.ps1` registers (or removes) the best-effort `ArcenPier`
Windows Event Log source consumed by `hosts/windows/src/eventlog.rs`
(`RegisterEventSourceW` / `ReportEventW`). It is owned independently of
`hosts/windows/credential-provider/registration-common.ps1`: the Pier
service and the Credential Provider have different install/uninstall
lifetimes and must not share a registration script.

The single-file Pier installer embeds this exact script, writes it to
`%ProgramData%\Arcen\eventlog-source.ps1`, and invokes it during install and
uninstall. The commands below remain the supported manual repair path; they
are not an additional step after a normal installer run. Registration remains
best-effort: a PowerShell policy failure or foreign pre-existing source is
reported as a warning while protected JSONL file logging and the essential
service/Credential Provider installation continue.

```powershell
.\host\eventlog-source.ps1 -Install
.\host\eventlog-source.ps1 -Uninstall
```

Both operations require an elevated 64-bit PowerShell session. Install is
idempotent (a repeat run by the same owner succeeds) and refuses to
overwrite or delete a registration it did not create (identified by an
`ArcenOwned` registry marker, not by the source name alone). v1 ships no
compiled message DLL; every native record carries its own raw, deterministic
insertion strings with exact readable Arcen lifecycle IDs. The shared
observability runtime owns the bounded 64-record nonblocking worker and counted
drop notices. See [`hosts/windows/INSTALL.md`](../../hosts/windows/INSTALL.md)
for when to run this relative to the service lifecycle, and
`host/eventlog-source.Tests.ps1` for its Pester coverage (installable via
`Invoke-Pester` — safe to run anywhere; it only touches an isolated `HKCU`
test subkey, never the real registration).

The Windows installer creates the protected `%ProgramData%\Arcen` tree, the
runtime/log/TLS/support directories, and the service registration. There is no
offline entitlement setup step.

## Support bundles

The Pier binary provides a service-independent
`arcen-pier.exe support-bundle [--out <DIR>]` command. The protected default is
`%ProgramData%\Arcen\Support`; installation must create it under the existing
SYSTEM/Administrators-only Arcen data ACL. Failure to use the default is
explicit and never falls back silently. Bundles are sensitive operational data;
see [`hosts/windows/INSTALL.md`](../../hosts/windows/INSTALL.md) and
[`docs/operations/support-bundles.md`](../../docs/operations/support-bundles.md).
