# Audio input operations

A Deck can send its microphone into the remote session. The Deck user turns
it on; the host administrator allows it.

## Turning it on and off

On the host, microphone input follows `microphone_input.enabled` in
`pier.json`. The key is required: a `pier.json` without it is refused at
start. The installers' templates set it to `true` on Linux
(`packaging/linux/arcen-pier.json`), Windows (`packaging/windows/pier.json`)
and macOS (`packaging/macos/arcen-pier.json`). An upgrade keeps an existing
`pier.json`; the Linux installer overwrites it only with `--force`.

To turn it off, set `"microphone_input": { "enabled": false }` and restart the
Pier:

| Host | Restart |
| --- | --- |
| Linux | `systemctl restart arcen-pier` |
| Windows | `Restart-Service ArcenPier` |
| macOS | `sudo launchctl kickstart -k system/pier.arcen.tech.service` |

On the Deck, microphone input is off at every launch until the user turns it
on; consent is never restored from settings.

Capture starts only when all of these hold: the host allows it, the Deck user
consents, the host's audio backend is available, the session is authenticated,
and both sides negotiate microphone-v1.

Audio payloads, device-private data, SIDs, credentials, profiles,
certificates and customer data never enter logs or support bundles.

## macOS Pier

The installer places `ArcenMicrophone.driver` in
`/Library/Audio/Plug-Ins/HAL` and restarts coreaudiod. When a Deck opts in,
applications on the host can choose **Arcen Microphone** as their recording
input. The device is always visible and plays silence while no
authenticated session is attached.

If the driver is missing or its LaunchDaemon service is unavailable, the Pier
refuses the stream with `BackendUnavailable`, so the Deck is told at once.
Uninstalling removes the driver and restarts coreaudiod.

## Linux Pier

Configure the absolute `pactl` path and set `microphone_input.enabled` to
`true`. The Pier creates the microphone source in the signed-in user's own
audio server, never as root.

- A refusal at start is a failure to fix, never a silent source.
- Recovery runs as soon as the user's session exists, even when microphone
  input is off or the Deck is older.
- The Pier records its intent before it changes the audio server, and
  verifies the exact module name, source properties and FIFO arguments when
  it restores, rather than trusting a reused module ID.
- Teardown closes the input, waits until the restore is verified, and keeps
  the journal when cleanup fails or is forced.
- FIFO I/O and every `pactl` child have deadlines and are reaped; a forced
  shutdown ends the helper's process group.
- A break in ordering ends microphone publication and tells the Deck; it is
  never accepted as a new starting point.

## Windows Pier

A Windows Pier refuses the stream until a signed microphone driver is
installed. Arcen does not ship that driver.

The source tree has the capture-only PortCls/WaveRT endpoint with a
service-SID control ACL, a bounded feeder, the INF, install, uninstall,
upgrade and `DiRollbackDriver` servicing, and surprise-removal cleanup.
`build.cmd` builds it when these are installed: the WDK 10.0.26100 SDK and
WDK, the Visual Studio DriverKit component, and the x64/x86 Spectre
libraries. It finds the WDK-capable amd64 MSBuild with `vswhere`. It never
promotes that unsigned output.

A release package takes an exact, externally signed SYS/INF/CAT directory
from `ARCEN_SIGNED_MICROPHONE_PACKAGE`. Packaging and installation reject a
missing or extra file, an unreviewed INF, a catalog mismatch, a test or
attestation-only signature, a signer other than WHCP/WHQL, and an installed
version or hash that does not match. The payload is staged under
`driver\payload\`.

Stop `ArcenPier` before you install, uninstall, upgrade or roll back the
driver, so no feeder handle crosses the device removal. Unsigned and
test-signed binaries are never production artefacts.

**Choosing the device.** After installation, select **Arcen Microphone** in
the Windows sound input settings or in the recording application. Arcen never
changes the Console, Multimedia or Communications defaults, and calls no
private API to set them. Disabling or disconnecting stops the feeder and
zeros its audio; it never changes the user's audio preferences.

If driver I/O cannot be cancelled before the lifecycle deadline, the session
agent ends rather than reattaching with a detached feeder.

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
and feeder timeouts. Windows reports no ring overrun or underrun values until
the driver exposes a verified signal; mailbox pressure and generic device
failures are reported separately. Persistent growth in drops, underflow,
timeouts, resets, fallback events, telemetry loss, or unverified
restore/cleanup points to a fault, not a warning to ignore.

## For contributors

The shared tests cover default media and the microphone Opus feature:

```text
cargo test --locked -p arcen-media --features audio-opus
cargo clippy --locked -p arcen-media --features audio-opus -- -D warnings
```

Host configuration and the optional Windows driver component are documented
in [`pier-configuration.md`](pier-configuration.md) and
[`windows-installer.md`](windows-installer.md). How the macOS Deck's signing
inputs are checked is in [`macos-signing.md`](macos-signing.md#how-the-build-checks-the-signing-inputs).
