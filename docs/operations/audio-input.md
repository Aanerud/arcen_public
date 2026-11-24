# Audio Input Operations

Microphone input is off in the Deck until the user turns it on, and on the
host it follows `microphone_input.enabled` in `pier.json`. That key is
required (a `pier.json` without it is refused at start), and the templates
the installers write set it to `true` on Linux
(`packaging/linux/arcen-pier.json`), Windows (`packaging/windows/pier.json`)
and macOS (`packaging/macos/arcen-pier.json`); an existing `pier.json` is
kept on upgrade (the Linux installer overwrites it only with `--force`). To
turn it off, set `"microphone_input": { "enabled": false }` in the host's
`pier.json` and restart the Pier: `systemctl restart arcen-pier` on Linux,
`Restart-Service ArcenPier` on Windows, `sudo launchctl kickstart -k
system/pier.arcen.tech.service` on macOS. A Windows Pier still
refuses the stream unless the signed microphone driver described below is
installed, which Arcen does not ship. Deck consent is
launch-only and must be enabled again after every process start. Enabling host
policy alone does not start capture: Deck consent, host backend availability,
authenticated session binding, and successful microphone-v1 negotiation are all
required.
Audio payloads, device-private data, SIDs, credentials, profiles, certificates,
and customer data must not enter logs or support bundles.


On macOS Pier, the installer places `ArcenMicrophone.driver` in `/Library/Audio/Plug-Ins/HAL` and restarts coreaudiod. With `microphone_input.enabled=true`, a Deck that opts in to microphone capture negotiates microphone-v1 and apps can select `Arcen Microphone` as their recording input. The device is intentionally always visible and emits silence while no authenticated session is bound. If the HAL bundle is absent or the LaunchDaemon Mach service is unavailable, the host refuses the stream with `BackendUnavailable` rather than letting the Deck wait. Uninstall removes the driver and restarts coreaudiod again.

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

Stop `ArcenPier` before install, uninstall, upgrade, or rollback so no feeder
handle crosses PnP removal. Unsigned or test-signed binaries are never
production artifacts.

## Lifecycle events

The stable microphone lifecycle events to correlate by `sid` are:

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
restore/cleanup points to a fault, not a warning to ignore.

The shared gate includes both default media and the microphone Opus feature:

```text
cargo test --locked -p arcen-media --features audio-opus
cargo clippy --locked -p arcen-media --features audio-opus -- -D warnings
```

The unified host config and future optional Windows driver component are
documented in
[`pier-configuration.md`](pier-configuration.md) and
[`windows-installer.md`](windows-installer.md).
