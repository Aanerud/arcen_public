# Windows Host Ownership

**Owner role:** Windows Host

Own Windows capture, input injection, machine authentication integration, host
session lifecycle, and Windows host packaging coordination in this path.

Validate on Windows/MSVC with
`cargo build --locked --release -p arcen-pier-windows
-p arcen-credential-provider`, then run affected crate tests and the root
shared-crate test and strict Clippy gates. There is no single-platform
`--workspace` build. The Credential Provider is logon-path security-critical;
production artifacts require Authenticode signing.

Runtime settings live in `%ProgramData%\Arcen\pier.json`; its common schema is
identical to Linux `/etc/arcen/pier.json`, with Windows-only values under
`platform`.

## Capture and HDR pipeline boundaries

- Auto/Speed are the eight-bit path: probe DDA for real desktop images and
  otherwise use WGC BGRA8. Host cursor authority requires WGC.
- Every ten-bit contract requires a concrete WGC
  `R16G16B16A16Float` scRGB pool. Never fall back to BGRA8 and keep claiming a
  ten-bit stream.
- Grading and HDR are separate conversions over that FP16 source. Grading
  clamps to SDR reference range and applies BT.709/sRGB; HDR converts linear
  scRGB to absolute BT.2020/PQ using 80-nit Windows reference white.
- HDR additionally requires the final HDR EDID/topology, Windows 11
  `activeColorMode=HDR`, and DXGI
  `RGB_FULL_G2084_NONE_P2020` on the exact session-bound display target.
  Never count or mutate unrelated active outputs.
- Keep pre-provision NVIDIA EDID recovery armed for the complete display lease
  and restore it after normal attachment teardown. A remote request must not
  leave persistent display identity or HDR state behind.
- On disconnect, atomically close and discard outbound video queues before
  joining the writer so buffered frames cannot consume the reconnect window.

Escalate shared API or protocol changes to Shared/Architecture; authentication,
privilege, GPU, signing, packaging, and release changes to Release/Security.

## Reaching a Windows Pier

Install prefix: `C:\Program Files\Arcen\Pier\arcen-pier.exe`.
Service: `ArcenPier`. Runtime config: `%ProgramData%\Arcen\pier.json`.

Do not record real hostnames, IP addresses, or credentials in this repository —
it is public. Note that on a domain-joined host the local administrative account
is often **not** `Administrator`; guessing through account names risks a
lockout, so confirm the account out of band.

```powershell
Get-Service ArcenPier
Restart-Service ArcenPier -Force
Get-NetUDPEndpoint -LocalPort 18444 | Format-List LocalAddress,LocalPort,OwningProcess
qwinsta            # who owns the console, and in what state
```

### Read `qwinsta` before blaming the Pier

A refusal reading `Remote sign-in is unavailable: another account is actively
using the physical console.` is
[`docs/architecture/windows-console-policy.md`](../../docs/architecture/windows-console-policy.md)
behaving correctly, not a fault: another account holds the console, and **a
locked console still counts as Active**. Restarting `ArcenPier` will not change
it. Either connect as the account `qwinsta` shows owning the console, or sign
that account out.

### The installer and the documented manual path must stay in step

`hosts/windows/INSTALL.md` hardens ACLs with SIDs (`icacls /setowner
'*S-1-5-32-544'`, SID-based `/grant:r`) and has done so from the start, so the
documented manual install has always worked on a localized Windows.

The `install-arcen-pier.exe` written later granted to `Administrators` and
`Users` **by name** in its very first commit, and never
worked on a non-English Windows at all. It was not a regression — the two paths
simply diverged, and every machine that exercised the binary (including CI's
`windows-latest`) is English, so nothing caught it for four
weeks.

When changing one path's privilege or ACL handling, check the other.

### Native tablet mode is not available on this host
Windows has no USB importer — no UDE or VHF adapter exists. A Deck that requests
`wacom_usb_bridge` against this Pier does not degrade gracefully; the session
drops with `websocket error: ... Connection reset without closing handshake`.
Hard USB is Linux-only for now, so keep Windows connections on Tablet support.
