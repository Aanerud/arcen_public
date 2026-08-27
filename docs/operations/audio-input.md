# Audio Input Operations and Release Gates

Microphone input defaults off in Deck and both Piers. Deck consent is
launch-only and must be enabled again after every process start. Enabling host
policy alone does not start capture: Deck consent, host backend availability,
authenticated session binding, and successful microphone-v1 negotiation are all
required.
Audio payloads, device-private data, SIDs, credentials, profiles, certificates,
and customer data must not enter logs or support bundles.

On Linux, configure the absolute `pactl` path and enable microphone input for the
dedicated session-user backend with `microphone_input.enabled=true`.
A startup refusal is an operational failure, not
success with a silent source. Authenticated per-user recovery runs as soon as the
session lease/environment exists, even when microphone policy is off or the peer
is legacy. Load intent is journaled before mutation and recovery verifies the
exact module name, source properties, and FIFO arguments rather than trusting a
reused numeric module ID. Teardown closes its bounded input, waits for verified
restoration, and retains the journal after nonzero, forced, or incomplete
cleanup. FIFO I/O and every `pactl` child are deadline-bounded and reaped; forced
shutdown terminates the helper process group. Service shutdown signals active
attachments first and waits for tracked cleanup reapers. An ordering
discontinuity terminates microphone publication and notifies Deck; it is not
silently accepted as a new ordering anchor.

On macOS, protected provisioning profiles, code-signing identities, and notary
credentials are external release inputs. `build-deck-app.sh` accepts only their
explicit environment references, verifies the profile CMS signature and Apple
provisioning trust chain before decode or embed, embeds the profile before
hardened-runtime signing, and never inventories, logs, or copies those inputs
into the repository. Release mode decodes only local metadata to temporary
files, verifies exact team/application identity, expiry, entitlement
authorization, and Developer ID Application certificate class, then deletes the
metadata. Production evidence requires successful notarization, staple
validation, and Gatekeeper assessment on the release artifact; none is optional
in release mode.

The CMS trust gate uses the checked-in Security.framework verifier; the
`security cms` decoder's process status is not accepted as signer evidence.
Native adversarial testing consumes an externally managed untrusted CMS fixture
without generating or logging profiles, certificates, or keys.

Windows source includes the independent capture-only PortCls/WaveRT endpoint,
service-SID control ACL and file-context enforcement, bounded feeder, INF,
install/uninstall/upgrade/`DiRollbackDriver` servicing, and surprise-removal
cleanup. `build.cmd` builds it when a WDK
10.0.26100 SDK/WDK, a Visual Studio DriverKit component, and x64/x86 Spectre
libraries are installed, but never promotes that unsigned output. It discovers
the WDK-capable amd64 MSBuild with `vswhere` rather than treating plain Build
Tools as a kernel toolset. A release package must
supply an exact externally signed SYS/INF/CAT directory through
`ARCEN_SIGNED_MICROPHONE_PACKAGE`; packaging and installation reject a missing,
extra, unreviewed INF, catalog membership mismatch, test/attestation-only
signature, non-WHCP/WHQL signer, or installed version/hash mismatch. The staged
payload is isolated under `driver\payload\`; driverless CI validates a separate
non-release manifest rather than pretending the protected payload exists.

After installation, select `Arcen Microphone` in Windows sound input settings
or directly in the recording application that should receive Deck audio. Arcen
does not require or rewrite the Console, Multimedia, or Communications defaults,
does not expose an `input_default_device` promise, and does not use private
`IPolicyConfig` or any equivalent unsupported setter. Disabling or disconnecting
stops and zeros the feeder only; it never rewrites user audio preferences. A
driver I/O cancellation that cannot be reaped before the single lifecycle
deadline is fatal to the session-agent process; reattachment is forbidden
rather than continuing with a detached feeder.

Every candidate still requires its own native WDK build and InfVerif run.
Remaining protected evidence requires EV/Partner Center and WHCP/HLK dashboard
signing, HVCI, Driver Verifier, signed-package servicing/rollback lab runs, and
physical x64 Windows 10 1809+/11 Pro or Enterprise and Server 2022+ multi-WTS
isolation. Stop `ArcenPier` before install, uninstall, upgrade, or rollback so
no feeder handle crosses PnP removal. Unsigned or test-signed binaries are never
production artifacts.

Hosted jobs that fail before runner assignment or report zero executed steps are
not validation. Native macOS, Linux audio-server, Windows WDK, protected signing,
and physical cross-host claims require their real target environments.

The 2026-07-24 PR 52 takeover ran the shared strict gates, native Deck tests and
unsigned bundle assembly, Linux host tests/release builds plus a real
PulseAudio `module-pipe-source` create/idle-pressure/restore cycle, Windows host
tests/full release build, portable ring tests, WDK 10.0.26100 compilation,
catalog generation, and InfVerif. Exact binaries were deployed through the
documented `arcen-pier.service` and `ArcenPier` paths with microphone policy
left off. This does not claim protected Apple signing/notarization, a
production-signed Windows driver installation, authenticated product-session
audio, or physical cross-host acceptance.

## Manual direct-connection test

Run this checklist only on rights-cleared source and test accounts. Do not attach
profiles, certificates, keys, SIDs, audio payloads, or customer data to a test
report.

1. For draft-PR validation, fetch and check out the exact reviewed PR head
   (`git fetch origin pull/52/head:audio-input-under-test &&
   git switch audio-input-under-test`) and verify the recorded SHA with
   `git rev-parse HEAD`. For post-merge validation, use
   `git switch main && git pull --ff-only` and record the resulting exact main
   SHA. State which mode was tested; neither result substitutes for the other.
   Then build Deck with
   `cargo build --locked --release -p arcen-deck-macos` and
   `packaging/macos/build-deck-app.sh`. Build the fused Linux Pier with
   `cargo build --locked --release -p arcen-pier-linux`. Build Windows Pier
   with `hosts\windows\build.cmd`.
2. Use Debug verbosity or `ARCEN_LOG=arcen::audio=debug,arcen::video=debug,
   arcen::capenc=debug,arcen::session=info`. Deck writes
   `~/Library/Logs/Arcen/arcen-client.log.*`; packaged Linux writes
   `/var/log/arcen/arcen-pier.log` (or the configured `ARCEN_LOG_DIR`); Windows
   writes broker and correlated session logs under
   `%ProgramData%\Arcen\logs`.
3. Enable microphone input in exactly one Pier and opt in with Deck's launch-only
   toggle. On Windows, first verify that **Arcen Microphone** is installed, then
   select it in Windows Sound input settings or in the receiving application.
   Arcen does not select it or change any default recording role.
4. Play a YouTube video in the Pier session and confirm ordinary host-to-Deck
   audio while speaking into the Deck microphone. Exercise normal Opus, then
   keep microphone input parked unless its platform prerequisite is available.
   Toggle the microphone off/on only when that policy is intentionally enabled,
   resize the Deck window, disconnect/reconnect, and stop the Pier while audio
   is active.
5. Confirm capture stops immediately on disable/disconnect, old-generation
   frames are rejected, and silence is limited to either gap fill (at most nine
   missing frames per accepted gap, counted by `silence_frames` but not
   `underflow_frames`) or actual empty-jitter playout (counted by both fields).
   Persistent underflow or any larger ordering gap fails the run. Confirm no
   stale voice resumes after reconnect, Linux restores and removes its
   session-user source, and Windows stops/zeros its feeder without changing user
   defaults.

The stable lifecycle events to correlate by `sid` are:

| Surface | Event names |
| --- | --- |
| Negotiation/media | `mic_negotiation`, `audio_output_negotiated`, `audio_output_codec_unavailable`, `media_plan_resolved`, `media_plan_received`, `media_encoder_fallback` |
| Deck | `mic_deck_permission`, `mic_deck_capture_start`, `mic_deck_capture_active`, `mic_deck_capture_stopped`, `mic_deck_capture_failure` |
| Linux | `mic_linux_backend_probe`, `mic_linux_recovery_started`, `mic_linux_recovery_completed`, `mic_linux_recovery_failure`, `mic_linux_helper_started`, `mic_linux_helper_ready`, `mic_linux_source_ready`, `mic_linux_helper_stopped`, `mic_linux_source_restored`, `mic_linux_restore_failure`, `mic_linux_helper_failure` |
| Windows | `mic_windows_endpoint_probe`, `mic_windows_feeder_started`, `mic_windows_feeder_stopped`, `mic_windows_feeder_timeout`, `mic_windows_device_removed`, `mic_windows_feeder_failure` |
| Typed rejection | `mic_frame_rejected` |

Inspect the rate-limited `mic_deck_stats`, `mic_linux_transport_stats`,
`mic_linux_stats`, and `mic_windows_stats` events and their final
`*_teardown_summary`/`mic_windows_feeder_stopped` snapshots. Repeated typed
`mic_frame_rejected` warnings include `suppressed_since_last` and are bounded
to one event per rejection class per statistics interval. The useful fields
are `captured_frames`, `encoded_frames`, `sent_frames`, `received_frames`,
`accepted_frames`, byte totals, `capture_queue_drop_oldest`,
`transport_backpressure_drops`, `transport_timeouts`, `duplicate_frames`,
`late_frames`, `wrong_generation_frames`, `discontinuities`, `jitter_depth`,
`jitter_target`, `jitter_max`, `silence_frames`, `underflow_frames`,
`decoder_resets`, `decoder_errors`, `rejected_discontinuities`, `fifo_timeouts`,
`fifo_failures`, `telemetry_drops`,
`feeder_mailbox_drops`, `device_failures`, `unauthorized_frames`,
and feeder timeouts. Windows does not claim ring overrun/underrun values until
the native driver contract exposes a verified signal; mailbox pressure and
generic device failures remain separate. Persistent growth in drops,
underflow, timeouts, resets, fallback events, telemetry loss, or unverified
restore/cleanup is a failed run, not a warning to ignore.

This manual run does not replace native release evidence. A production Deck
still requires the external provisioning, Developer ID, hardened-runtime,
notarization, stapling, and Gatekeeper gates above. A production Windows
endpoint still requires the WDK, WHCP/HLK, EV/Partner Center, signed package,
HVCI, Driver Verifier, servicing, and physical multi-WTS gates. Linux source
creation/restoration must be exercised against its real session-user audio
server; a fake `pactl` test is not native evidence.

The shared gate includes both default media and the microphone Opus feature:

```text
cargo test --locked -p arcen-media --features audio-opus
cargo clippy --locked -p arcen-media --features audio-opus -- -D warnings
```

The unified host config and future optional Windows driver component are
documented in
[`pier-configuration.md`](pier-configuration.md) and
[`windows-installer.md`](windows-installer.md).
