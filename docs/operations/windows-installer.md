# Windows Installer

**Status:** the current single-file CLI installer embeds the exact verified
`arcen-pier.exe` and matching Credential Provider DLL, installs the service and
provider, and opens QUIC/UDP 18444.
`hosts\windows\build.cmd` produces it as
`target\arcen-windows-x64\install-arcen-pier.exe`; direct standalone Cargo
builds must explicitly provide both payload paths so an unrelated binary cannot
be embedded accidentally.

Upgrades retain rollback payloads and atomically migrate an existing
`%ProgramData%\Arcen\pier.json` from the legacy direct listener and TLS 1.2
floor to QUIC/UDP 18444 and TLS 1.3 while preserving unrelated fields.

The future GUI and signed-driver-capable installer should present one explicit
optional component: **Arcen Microphone input driver**. The current CLI installer
does not install that optional driver.

## Credential Provider upgrade acceptance

Credential Provider replacement must be rollback-safe: retain the previous Pier
and DLL, stop `ArcenPier` while replacing Pier, and use the owned registration
scripts rather than editing another provider. A loaded CP DLL may require a
reboot before replacement. Development labs use `install-test.ps1` explicitly;
production still requires the signed `install.ps1` path.

Before completion, validate a reboot with no signed-in user:

1. `quser` reports no interactive user and LogonUI runs in the physical-console
   session. WTS may report that identityless local console as `Active` or
   `Connected`; both are valid cold-logon states.
2. `ArcenPier` is automatic LocalSystem and listens on the configured direct
   port.
3. Broker logs show a verified fresh LogonUI CP peer in that console session.
4. A credential available only through the authorized Deck/test mechanism
   creates or unlocks that exact SID session, starts the session agent, and
   streams. Never place a password in a script, file, command line, or log.

Cold `Active`/`Connected` identityless LogonUI uses `CPUS_LOGON`; only one exact
SID-matching locked `Active` physical console may use
`CPUS_UNLOCK_WORKSTATION`. Another account, RDP,
disconnected/stale state, old CP generation, or ambiguity must fail closed.
Autologon must remain disabled.

| Operator choice | Installer behavior | Generated config |
| --- | --- | --- |
| No | Do not stage or install a microphone driver. | `"microphone_input":{"enabled":false}` |
| Yes | Validate and install the bundled production driver package before starting `ArcenPier`. | `"microphone_input":{"enabled":true}` |

The installer must not infer the choice, enable microphone policy without the
device, or install the device while leaving policy off. Upgrade, rollback, and
uninstall must preserve the recorded component choice and stop `ArcenPier`
before PnP servicing.

## Signing boundary

Arcen's virtual microphone is a kernel-mode PortCls/WaveRT audio endpoint. A
normal Windows 10 1607+ or Windows 11 production system does not load a new
unsigned kernel driver; Secure Boot systems require a signature trusted by
Windows Code Integrity. Windows Server support can require the WHCP/HLK path.
The final signing offering and target matrix require Release/Security approval.

There is no production installer checkbox that can safely override this
boundary. Test signing is a separate development-machine procedure and must
never be presented as a production install option.

The referenced OSR thread shows a `simpleaudiosample.sys` kernel crash and does
not establish a driverless endpoint or signing exemption. The
VirtualDrivers/Virtual-Audio-Driver project likewise states that its beta
requires Windows test-signing mode. These projects may inform research but
their third-party source and binaries are not cleared for import into Arcen.

Microsoft references:

- [Driver signing policy](https://learn.microsoft.com/windows-hardware/drivers/install/kernel-mode-code-signing-policy--windows-vista-and-later-)
- [Driver code-signing requirements](https://learn.microsoft.com/windows-hardware/drivers/dashboard/code-signing-reqs)
- [SysVAD virtual audio sample](https://learn.microsoft.com/samples/microsoft/windows-driver-samples/sysvad-virtual-audio-device-driver-sample/)
