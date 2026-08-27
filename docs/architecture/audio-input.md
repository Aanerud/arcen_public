# Audio Input Architecture

Audio Input is an opt-in microphone-v1 subprotocol on the unchanged global
protocol v3. It applies only to direct macOS Deck connections to Linux and
Windows Piers. Span and Windows Deck remain dormant.

Deck advertises microphone-v1 only after the user enables its launch-only local
toggle. Microphone consent is never restored from persisted settings, so every
process launch requires a fresh opt-in.
The Pier advertises only when operator policy is enabled and its host backend is
actually available. After authentication, both peers negotiate Opus or explicit
fixed-rate PCM fallback and a nonzero attachment generation. Opus carries an
Opus bitrate tier; PCM carries `bitrate: off` plus an additive exact
`pcm_bitrate_kbps: 768`, so the wire never labels PCM with an Opus ceiling.
Microphone bytes use the
dedicated sequenced `AudioUpstream` frame; the existing host-to-client `Audio`
frame is unchanged. Any absent capability, policy refusal, invalid generation,
malformed payload, or unnegotiated upstream frame is rejected.

`arcen-media` owns deterministic, I/O-free post-decode behavior: 48 kHz mono
signed 16-bit PCM, 20 ms/960-sample frames, a ten-frame fixed store, three-frame
target, wrapping sequence/timestamp checks, duplicate and late drop, and exact
silence underflow. Generation, ordering, and payload bounds are classified
before stateful decode; only a successful decode commits ordering. Opus state
and output storage are reused, and decoder scratch, jitter slots, and PCM
buffers are zeroed on failure, reset, and drop. Binary and serde
contracts remain in `arcen-protocol`; unsafe native device work remains in its
platform owner.

Deck uses AVAudioEngine after the host enables the stream and macOS grants
permission. Capture startup is cancellable and does not block transport keepalives.
The capture callback validates channel layout and stride, normalizes to the fixed
format, stamps capture-clock sequence/timestamp before queueing, and uses a
bounded two-frame drop-oldest queue so capture loss remains visible as a
sequence gap. A session-lifetime cancellation latch is checked before startup,
result installation, callback publication, and every socket write. Disable,
disconnect, or generation replacement synchronously sets that latch, cancels and
joins pending startup, stops the engine before network close under one close
deadline, clears scratch buffers, and removes the visible mic-active state.
Missing that deadline terminates Deck fail closed rather than detaching capture
into a live process.
Engine/device/permission/progress failures close capture and send a typed,
generation-bound microphone-v1 stop control so the Pier immediately withdraws
publication without ending an otherwise healthy session.

Linux runs authenticated per-user recovery whenever the session lease and user
environment become available, including policy-off, legacy-client, and backend
failure paths. A cryptographically random nonzero generation prevents restart
replay. Before any mutation the helper journals exact module/source/FIFO intent,
then creates a generation-specific `module-pipe-source`, records the prior
default, and feeds only prevalidated bounded frames. Cleanup restores the
default, verifies the exact module arguments before unload, and removes the FIFO
through staged retryable cleanup. FIFO writes remain nonblocking and
deadline-bounded; every `pactl` call is time-bounded and reaped, and forced
shutdown terminates the helper process group. Failed or forced cleanup retains
the journal until restoration is verified. Arguments are passed directly to
`pactl`; no shell is involved. A rejected ordering discontinuity never rebases
the ingress anchor; it terminates that negotiated microphone stream and sends a
typed failure so capture cannot remain silently wedged.

Windows safe Rust binds inbound decode and playout to WTS ID, binary SID, and a
monotonic generation. The independently authored kernel contract defines a
fixed ten-frame ring with oldest-drop overrun, exact-silence underrun, secure
clearing, and stale/cross-session rejection. A capture-only PortCls adapter
registers topology and fixed-format WaveRT filters, owns cyclic-buffer position
and aligned notification behavior on a bounded interrupt-time catch-up timeline,
emits silence on underrun, and synchronously zeros every active stream on stop,
power loss, and surprise removal. Each capture stream snapshots the originating
create IRP's documented requestor WTS session. Shared-mode capture is opened by
the Windows audio engine, so the driver does not incorrectly compare that
service process's primary SID with the interactive user. The trusted feeder
binding still carries the host-validated WTS/SID/generation identity. Readers
run silent while unbound, reject another WTS session, and adopt a newer
generation only for the same WTS session; mismatches receive silence without
draining audio. A service-SID-only control device validates
exact buffered IOCTL layouts, binds one file context to WTS/SID/generation, and
zeroes every stale or consumed frame. The host uses a two-frame mailbox and one
absolute lifecycle deadline for its overlapped-I/O worker; stop bypasses queued
audio and joins before attachment exit. If a driver operation cannot be
cancelled and reaped safely, cleanup is fatal to the owning session-agent
process, so that process cannot bind another attachment around live kernel I/O.

Windows exposes no supported public default-endpoint setter. Arcen therefore
does not own, mutate, journal, or restore recording roles. The installed
`Arcen Microphone` endpoint remains selectable in Windows and per-application
audio settings; the user, operator, or recording application chooses it.
Endpoint availability is reported from the fail-closed driver control probe,
not inferred from or presented as default-role ownership.
