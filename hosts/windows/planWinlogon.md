# Direct cold-boot first-login — completed

**Status:** cold logon proven on `pier-windows.example.internal` on 2026-07-15; exact-console
unlock state machine added for both Windows labs on 2026-07-25.

This file previously planned a separate secure-desktop logon driver. Live implementation
showed that component was unnecessary: the documented Credential Provider + SCM service path
works when the service contract, account identity, serialization format, and CP-session
selection are all correct.

## Delivered behavior

On a rebooted Windows machine with no signed-in user:

1. The macOS Deck opens a direct QUIC connection to `arcen-pier.exe`.
2. Pier validates the password with `LogonUserW(LOGON32_LOGON_INTERACTIVE)` and retains the
   authenticated SID.
3. Pier resolves that SID to canonical `MACHINE\user` or `DOMAIN\user`.
4. The LocalSystem `ArcenPier` SCM service identifies the active physical console, recycles
   stale CP connections, and calls documented `SendSAS(FALSE)`.
5. Pier accepts only a fresh `CPUS_LOGON` Ready from `LogonUI.exe` running as SYSTEM in that
   exact console session; Ready PID must match the kernel pipe peer PID.
6. The credential is sealed to the CP's per-connection key and challenge.
7. The CP protects the password with `CredProtectW`, packs a checked
   `KERB_INTERACTIVE_UNLOCK_LOGON` for Negotiate, and offers it exactly once as the default
   autologon credential.
8. Winlogon consumes it, creates the interactive session, and reports the result.
9. Pier finds the exact console WTS session by the original authenticated SID,
   requires it to remain continuously active and unlocked through a bounded
   15-second desktop-transition gate, then launches the existing session agent
   on `winsta0\default`.
10. If that exact account already owns the locked physical console, Pier instead
    requires a fresh same-session `CPUS_UNLOCK_WORKSTATION` peer. Another user,
    RDP, disconnected/stale, unknown-protocol, or ambiguous state fails closed.

No separate `arcen-logon-driver.exe`, Winlogon-token helper, desktop-switching process, or
undocumented `SetCtrlAltDelConfig` call is shipped.

## Security invariants

- Pier must run as a real `SERVICE_WIN32_OWN_PROCESS` LocalSystem service, not a scheduled task.
- The CP pipe is local, SYSTEM-only, and rejects a non-LogonUI or non-SYSTEM peer.
- Credential dispatch is bound to verified PID, console session, usage scenario, connection
  generation, account SID, request id, ephemeral keys, and challenge.
- Any pre-SAS or replaced CP generation is rejected.
- Armed plaintext exists for at most 30 seconds and is cleared by request id; old expiry
  workers cannot clear a newer credential.
- The expiry worker pins the CP module and exits through `FreeLibraryAndExitThread`.
- Session admission after logon is SID-based; names are never trusted for binding.
- Unlock is limited to one exact authenticated SID on the locked physical
  console; it is not a generic remote-unlock mechanism.
- No plaintext credential is written to a log, file, command line, or environment variable.

## Live proof

The accepted lab path used the MSVC static-CRT artifacts from `hosts/windows/build.cmd` and the
manual install contract in `hosts/windows/INSTALL.md`.

Across two separate no-user reboots:

- `ArcenPier` started automatically as LocalSystem on UDP port `18444`.
- LogonUI connected from the physical console session.
- CP diagnostics recorded `autologon=true`, `GetSerialization: returning finished credential`,
  and `ReportResult: status=0x00000000`.
- Security recorded interactive 4624 events for the test account with
  `Logon Process: User32`.
- A new active console WTS session appeared.
- Pier logged `remote first-login: bound the newly created session` and launched the
  per-session agent.
- A deliberately invalid password returned only `Invalid credentials`; a subsequent valid
  attempt reached the Deck's `streaming` state.

The first proof used a new standard local account with no profile, demonstrating first-profile
creation. GRID display/NVENC behavior after login is separate from this completed task and is
tracked in `todo_later.md`.

## Supported and deferred boundaries

- Implemented: local machine accounts and traditional `DOMAIN\user` password serialization.
- Live-proven: local account on Windows 11 Pro x64.
- Build/API target: x64 Windows 10/11 Pro or Enterprise and Windows Server 2022+.
- Deferred: live AD validation until an AD-joined test machine is available.
- Out of scope: Entra, Okta, MFA, Kerberos brokering, and Arcen Span.
- Production release still requires Authenticode signing and a distinct Arcen shipping CLSID.
