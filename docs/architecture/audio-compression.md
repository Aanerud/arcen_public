# Audio Compression

**Status: implemented for active direct-QUIC Linux and Windows Piers with the
macOS Deck (2026-07-22).** Global protocol version 3 and its binary media
discriminants remain unchanged. Opus is enabled only by exact audio subprotocol
v1 negotiation; peers without that capability remain on byte-compatible PCM.

Audio input, media-plan resolution, Span/gateway transport, and dormant clients
are outside this delivery.

## Shared-first boundary

`arcen-protocol` owns the additive audio-v1 wire contract.
`arcen-media` owns pure fixed-format negotiation, bitrate selection, timestamp,
jitter, PLC, reconnect, and codec policy. Windows WASAPI and the Linux Pier's
`audiocap` subprocess mode remain capture adapters; macOS CoreAudio remains the
playout adapter.

The fixed v1 media shape is:

- 48,000 Hz, stereo, signed 16-bit PCM input;
- 20 ms, 960 samples/channel, 1,920 interleaved samples;
- Opus packets bounded to 1,275 bytes;
- constrained VBR tiers `32`, `64`, `128`, `256`, and `510` kbps, plus `off`;
- DTX and in-band FEC disabled;
- 60 ms playout target, 110 ms trim threshold, 200 ms hard cap, and at most
  three 20 ms PLC frames.

`arcen-media/audio-opus` is non-default. The default shared graph contains no
`opusic-c`, `opusic-sys`, or `cmake`. The enabled adapter uses safe
`opusic-c` APIs with caller-owned reusable input, output, and conversion
buffers. Arcen's shared media code remains `#![forbid(unsafe_code)]`.

## Protocol-v3 compatibility and audio-v1 selection

Both hellos default `audio_output` to absent. An audio-v1 capability declares
the exact protocol version, ordered codecs, fixed format, and disabled FEC/DTX.
The host resolves that capability with its required `audio.enabled` and
`audio.compressed` policy. `compressed=false` advertises PCM only;
`compressed=true` advertises Opus only at the fixed 128 kbit/s tier. The
existing `QualitySettings.enable_audio` may mute audio, but its bitrate value
does not override host policy. The host confirms an explicit v1 choice through
`audio_stream_result`.

Real Opus media is legal only after the Deck accepts an enabled, valid
audio-v1 result selecting Opus. Missing, malformed, mismatched, or unsupported
capabilities fail closed. Legacy peers do not receive a new result and continue
to receive PCM. The historical Linux behavior that labeled raw PCM as Opus is
interpreted only in Deck legacy mode and cannot enable the real Opus decoder.

The binary audio header remains eight bytes:

| Offset | Size | Meaning |
| --- | --- | --- |
| 0 | 1 | `FrameType::Audio = 0x10` |
| 1 | 1 | `AudioCodec::Opus = 0x00` or `AudioCodec::Pcm = 0x01` |
| 2 | 2 | reserved zero |
| 4 | 4 | wrapping big-endian `timestamp_ms` |

## Attachment and failure lifecycle

Every attachment owns fresh encoder/decoder, jitter, PLC, and negotiated-mode
state. Reconnect cannot reuse compressed history. Bitrate-only changes update
the existing encoder; disable/re-enable or codec changes replace/reset it.

Capture, encode, decode, queue, and playout failures are audio-local. They drop
or disable bounded audio work without terminating video, input, or the direct
direct transport attachment. The configured codec never silently falls back to the other
codec. Queues remain bounded and apply counted-loss/nonblocking
discipline. Hot-path counters use relaxed atomics. Telemetry may contain only
bounded enum-like reasons, tiers, byte/frame counts, and queue/PLC counters; it
must not contain samples, packets, hashes, waveforms, device identifiers, user
data, or unbounded native error strings. This aligns with the approved
Observability Standard vocabulary direction without implementing that future
runtime or wire work.

A suspended Linux PulseAudio monitor is valid inactivity, not helper failure.
Pier keeps the same framed stdout read alive across an idle notice and restarts
its `audiocap` subprocess only after EOF, a read error, or failed child liveness. A stream
that resumes more than 100 ms after a prior chunk records one capture gap.
Deck counts CoreAudio active-to-prebuffer underruns, latency-trim events/samples,
and media-worker feed gaps under the existing playback mutex; the realtime
callback does not allocate or log.

## Dependency and distribution

The feature locks `opusic-c` 1.6.1 and path-patches exact `opusic-sys` 0.7.3 to
`third_party/opusic-sys-0.7.3-arcen1`. The complete crates.io source and bundled
libopus 1.6.1 tree are governed by
`ARCEN_SOURCE_MANIFEST.sha256`. The sole Arcen source patch drives
`OPUS_STATIC_RUNTIME` from Cargo's MSVC `crt-static` target feature.

Windows release builds require `+crt-static`, verify the retained CMake
configuration, inspect every `opus.lib` member for `LIBCMT` and against
`MSVCRT`, and reject dynamic compiler/Opus runtimes from package dependents.
Linux and macOS packaging reject a shared libopus dependency or nested codec
payload. All active packages include `legal/THIRD_PARTY_NOTICES.md`. Exact
checksums, commits, source trees, patches, licenses, and the deferred SBOM
automation gap are recorded in `legal/ORIGINS.md`.

## Acceptance boundary

Deterministic protocol, negotiation, codec, packet-bound, queue, jitter, PLC,
reset, reconnect, and malformed-input tests are automated. Native package
checks run in their platform CI lanes. Physical Windows-Pier, Linux-Pier, and
macOS-Deck audio acceptance, signed/notarized distribution, and a
release-specific SBOM/inventory remain release gates and are not claimed by
this implementation record.
