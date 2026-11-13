# How Arcen works

Moved from the README (0.19.0).

## How it works

A **Pier** captures a desktop, encodes it, and sends it. A **Deck** receives it,
decodes it, and sends your keyboard and mouse back. Everything else is detail.

**One connection, one protocol.** Arcen speaks QUIC over UDP 18444 and nothing
else. There is no TCP fallback and no unencrypted mode. QUIC carries TLS 1.3
itself, so the encryption is part of the transport rather than bolted on.

**The client picks a result, not a codec.** The Deck asks for Auto, Speed,
Grading, or HDR and reports what it can decode. The Pier chooses the concrete
capture and encoder path, because only the Pier knows what its display and GPU
can supply. If it cannot serve the requested result, it reports the lower
fidelity plan rather than pretending.

**How frames leave the host.** Capture is platform-specific and
contract-specific. The 8-bit, ten-bit SDR, HDR, and software paths are separate
pipelines with separate admission rules; they are not one screenshot loop with
different labels.

On **Linux**, Auto and Speed use **NvFBC → CUDA → NVENC**. The frame stays on
the GPU. Grading cannot use that path because public NvFBC has no ten-bit
format, so it uses **depth-30 Xorg → MIT-SHM → packed RGB10 conversion → CUDA
upload → NVENC**. X11 has one depth per server, and at depth 30 browsers lose
the GPU, so a Linux desktop is ten-bit only when the session that creates it is
(`video.x11_desktop_depth`). Xorg has no HDR composition contract; an HDR request therefore
uses that same genuine ten-bit SDR pipeline with an explicit downgrade. A
machine without usable NVENC takes a third path: checked X11 capture and shared
conversion into source-built OpenH264. A headless Wayland session without
usable NVENC takes the same encoder through its own composition: the
compositor's shared-memory frames, Keel's hash, and the one OpenH264 session
both providers share.

On **Windows**, Auto and Speed use the 8-bit D3D11 path: Desktop Duplication
only when it proves real image delivery, otherwise WGC. Grading requires a
different WGC pool, `R16G16B16A16Float`, and re-encodes the linear scRGB with
the sRGB curve Windows linearised it with, so every eight-bit desktop code
comes back unchanged and the stream is tagged `srgb` (a Deck older than
0.18.0, which cannot present `srgb`, gets the BT.709 curve). HDR adds another upstream contract: Arcen first provisions the final
HDR EDID/topology, enables HDR on the exact bound display target, verifies
PQ/BT.2020, and only then converts the FP16 scRGB source into HDR10. WGC refusing
the FP16 pool is a hard failure; Arcen never falls back to BGRA8 while claiming
ten-bit.

**What 10-bit means depends on the source.** Linux Grading reads genuine
depth-30 Xorg codes. Windows Grading and HDR read FP16 scRGB. An 8-bit source
encoded into ten bits can still avoid RGB↔YCbCr rounding loss, but Arcen does
not confuse that mathematical benefit with a genuinely wide capture source,
and it never treats bit depth alone as proof of HDR.

**Codec choice follows the hardware — at both ends.** The Pier can only send
what the Deck can actually decode, so the Deck measures its own capability at
startup and reports it in the handshake.

On macOS that means VideoToolbox, and the answer differs by machine. Every Apple-silicon
Mac decodes H.264 and HEVC in hardware; an Intel Mac whose GPU has no decoder
for a stream decodes it in software, and reports only what a real decode
session accepted. **AV1 hardware decode arrived with the M3**. An M1, an M2, or
any Intel Mac reports no AV1 (the Intel app never offers it), and the Pier
picks HEVC instead. Nothing degrades silently; the negotiation simply lands
somewhere else.

| Host has | Deck can decode | Arcen uses |
|---|---|---|
| Modern NVIDIA GPU | AV1 (M3 or later) | Hardware AV1, encoded and decoded in hardware |
| Modern NVIDIA GPU | No AV1 (M1, M2, Intel) | Hardware HEVC |
| Older NVIDIA GPU | HEVC | Hardware HEVC, then H.264 |
| Any NVENC GPU | HEVC 4:4:4 10-bit | Hardware HEVC 4:4:4 10-bit, for colour-critical work |
| No GPU at all | H.264 | Source-built OpenH264, decoded in hardware on the Deck |

Note the last row: software encoding on the host still gets **hardware decoding
on the client**, because the output is ordinary H.264. The cost lands on the
host's CPU, not on the laptop in front of you.

The Deck does not guess. It builds a real decode session for each format and
reports only what succeeded — separately for 4:4:4, 10-bit, 12-bit and colour
range. The code is blunt about why: claiming a capability the Deck has not
demonstrated is worse than admitting it, because the host believes the claim and
sends a stream the Deck then cannot decode.

### Keel: send what changed, and nothing else

Keel ([`shared/keel`](../shared/keel)) is the part of every Pier that decides
whether a frame is worth sending. It works at two levels, and they are easy to
confuse.

**Frame level — on for every hardware pipeline.** An encoder submits a frame
when the desktop changed, when the Deck asked for a keyframe, once more to
flush NVENC's one-deep output queue, and otherwise once a second as a
keepalive. A still desktop therefore costs a fraction of a megabit, not a full
stream: a Windows RTX session measured 150–400 kbit/s while idle and 5–9 Mbit/s
while a video played. Each Pier gets the change signal from its own operating
system:

| Pier | Change signal |
|---|---|
| Linux, Auto/Speed (NvFBC) | NvFBC's new-frame flag. The CUDA capture interface has no per-block damage, so this path has frame-level gating only. |
| Linux, Grading/HDR (XShm) | X Damage events |
| Linux, Wayland | The compositor's per-buffer damage regions (GNOME 49 and later), which also feed the QP map; GNOME 40 sends none, so every frame there is full damage |
| Windows, every preset | Desktop Duplication and WGC deliver a frame only when the desktop changed; their dirty/move rectangles and dirty regions feed the QP map |
| macOS | ScreenCaptureKit dirty rectangles |
| Software fallback | Keel's own 16×16 block hash, which also skips unchanged rows during colour conversion |

**Block level — an encoder hint, off by default.** With `video.qp_map` set
(one value, or per preset: `{"auto": "on", "speed": "on"}`), Keel turns the
same damage into an NVENC QP map so changed blocks get the bits. It engages
only where damage is free: OS rectangles on Windows, X Damage on Linux
Grading/HDR, compositor damage regions on Wayland. The Linux NvFBC
fast path refuses it rather than add a full-frame
GPU readback. Every encoder logs
`QP map policy=… engaged=… damage_source=…`, so whether it ran is never a
guess. See [`docs/architecture/qp-maps.md`](architecture/qp-maps.md).

Modern codecs already code an unchanged block almost for free, so Keel's gain
is mostly the frames it never encodes. What it does not do yet is *refine*: a
static region that arrived during motion stays at its first quality until it
changes. Spending spare bits to sharpen blocks once they stop changing — the
"build to lossless" behaviour other remote-desktop products use — is open work.
The proposed design,
[ADR 0016](adr/0016-lossless-refinement.md), goes further than sharpening:
once a Grading or HDR block has been still for a moment, the Pier sends its
source code values losslessly, in the stream's own bit depth and transfer,
and the Deck proves the still picture is bit-exact with a digest; Speed and
the eight-bit fast path are never refined.

### How frames reach the glass on the Deck

Both the 8-bit and the 10-bit streams normally present decoded `IOSurface`s
through a dedicated `CAMetalLayer`, with no CPU copy; the ordinary window
surface remains only as a fallback that the Deck reports when it is used. A shared display-refresh pacer
([`shared/media/src/presentation.rs`](../shared/media/src/presentation.rs)),
driven by the window's `CVDisplayLink`, shows at most one new picture per
refresh from a queue of at most three, dropping the oldest on overflow and
anything stale after a stall. Two lessons from real sessions are built in:

- **Never take a frame you cannot show.** When every drawable is still in
  flight, the refresh waits and the frame stays queued for the next one.
  Taking it and then finding no drawable silently threw away about one frame
  in four on a 60 Hz panel.
- **A network stall arrives as a burst.** A 250–300 ms Wi-Fi or VPN stall
  releases every frame produced meanwhile at once. The Deck's inbox holds about
  half a second, so the burst is decoded and the newest frame shown; a smaller
  inbox discarded the chain, asked for a keyframe, and the large keyframe made
  the next burst bigger.

The Deck logs `dedicated presenter telemetry` every second — received,
submitted, confirmed, drops by cause, slot waits, and refresh intervals — and
the host's health check compares decoded against presented frames, so a
presentation problem shows up even when nobody notices it on screen.

## Three ways to run a Pier

Hardware encoding is the fast path, but it is not the only intended one.

**On a workstation with an NVIDIA GPU.** The full range, including HEVC 4:4:4
10-bit for colour-critical work. Both the Linux and Windows Piers do this.

**On a virtual machine.** Proxmox, VMware, and similar hypervisors usually give
a guest no encoder at all. Arcen uses source-built OpenH264. This is a
first-class way to run it rather than a consolation prize: it makes Arcen a
practical alternative to RDP or VNC on ordinary server hardware, with the same
QUIC and TLS 1.3 as everywhere else. You give up 4:4:4 and 10-bit, not
encryption and not the protocol.

**On a headless host.** Same as above, provided something gives the machine a
display to capture — a hypervisor's virtual adapter, or an indirect display
driver. Arcen ships no display driver of its own. On supported NVIDIA Windows
hosts it can instead provision unused native display IDs with session-scoped
EDIDs, including the HDR sink contract, and restores them when the lease ends.
That NVIDIA provisioning path currently requires the Windows account being
signed in to belong to the local Administrators group.

The encryption does not change between these. A software-encoded session on a
Proxmox guest is protected exactly as a 4:4:4 workstation session is.

**Only one session at a time.** A Pier drives one physical desktop. Two sessions
would fight over the same screen, the same mouse, and the same encoder, so the
Pier admits one and refuses the rest. If your connection drops, it holds your
session for its reconnect window — three minutes by default, configurable up
to two hours — so you can resume where you left off.

---

## Security

Most remote-desktop projects gloss over this. Here it is plainly.

**How the Deck knows it reached the right Pier.** A Pier generates its own
certificate at install time, so there is no public authority to vouch for it.
The Deck therefore supports five modes:

| Mode | What it does |
|---|---|
| **System CA** | Ordinary certificate checking. Choose *High* in Advanced settings. It correctly refuses a self-signed Pier. |
| **Private CA** | Checks against a certificate authority you supply. The right choice for an organisation with its own. Command line only today — `--ca-bundle` — and a contribution adding it to the settings pane would be welcome. |
| **Trust on first use** | Shows you the fingerprint and asks. **The default**, because a freshly installed Pier is self-signed and this is the only mode that can connect to one without preparation. |
| **Pinned** | What trust on first use becomes once you remember a host. |
| **Insecure** | Development only. Requires *both* a setting *and* the `ARCEN_ACCEPT_INSECURE` environment variable, so one mistake cannot disable checking. A banner stays on screen for as long as it is active. |

**The first connection asks you.** The Deck shows the certificate fingerprint,
the public-key fingerprint, the validity dates, and the address that presented
them, then offers *cancel*, *trust once*, or *trust and remember*.

*Trust once* lasts until you quit. *Trust and remember* writes the host's
public-key fingerprint to `trusted_pins.json` and pins that exact identity to
that host and port — not to the saved connection, so quick-connecting to the
same address is protected too.

**A remembered host that changes its identity does not ask again — it fails.**
This is the point of the whole ceremony. Once a fingerprint is recorded, a
different one is a hard error with no "trust anyway" button, because an
impostor's certificate would otherwise raise exactly the same friendly dialog
the real host raised the first time. If the host really was reinstalled, you
can confirm that you checked with its administrator and choose *Forget and
Verify*. The Deck drops the old fingerprint, reconnects at once and shows you
the new one to compare before anything is trusted.

**Pins are compared in constant time**, so an attacker cannot learn a pin by
measuring how long a rejection takes. A pin changes only when you change it.

**Authentication belongs to the operating system.** PAM on Linux; on Windows, a
credential provider that takes part in the real login. Passwords are held in
memory that is wiped on release and never written to a log.

**The Linux and Windows Piers have no lockout of their own.** Repeated password
attempts are passed to the operating system, so throttling is whatever
`pam_faillock` or your Windows account-lockout policy already does. If you
expose a Pier to a network you do not control, configure that policy — Arcen
will not do it for you. The macOS Pier (preview) backs off by itself: after
three failures from one address or for one account it waits, doubling from
2 seconds up to 5 minutes (shared `arcen_session::auth_throttle`).

### Using your own certificate

The installer generates a self-signed certificate so a fresh Pier works
immediately. That is why the Deck asks you to confirm a fingerprint: nothing
else vouches for it.

Give the Pier a certificate from a real authority and that step disappears. The
Deck's default mode already trusts the system authorities, so the connection
simply succeeds, the same way a browser does not interrogate you about a normal
website.

Point the Pier at your own PEM certificate and private key:

```jsonc
// /etc/arcen/pier.json   (Linux)
// C:\ProgramData\Arcen\pier.json   (Windows)
"tls": {
  "mode": "pem",
  "cert": "/etc/ssl/certs/pier.example.com.crt",
  "key":  "/etc/ssl/private/pier.example.com.key",
  "minimum_version": "TLS1.3",
  "expiry_warning_days": 30,
  "expected_sans": ["pier.example.com"]
}
```

Relative paths resolve inside the Arcen configuration directory; absolute paths
are used as given. Restart the service afterwards. `mode` accepts `pem` only —
there is no PKCS#12 or system-store option.

`expected_sans` is worth setting. The Pier validates its own certificate against
the names listed and **refuses to start** if the certificate does not cover
them. That fails closed on the common mistake of installing a certificate for
the wrong hostname, rather than serving a certificate every Deck will reject.
`expiry_warning_days` warns before the certificate lapses.

Then match the Deck to your situation:

| Your certificate | Deck mode | What the user sees |
|---|---|---|
| From a public authority | System CA — choose *High* | Connects. No prompt. |
| From your organisation's own authority | Private CA — `--ca-bundle` on the command line | Connects. No prompt. |
| Self-signed, as installed | Trust on first use — the default | Fingerprint prompt once, then remembered |

If you are running more than a handful of Piers, the middle row is the one
worth the effort. Issue them from your internal authority, hand the Deck your CA
bundle once, and no one has to compare fingerprints again.

A certificate covering extra names is easiest to get right at install time:

```sh
sudo ./install-arcen-pier --extra-san pier.example.com --extra-san 203.0.113.10
```

Add `--force` to replace a certificate that already exists.

To report a vulnerability, see [SECURITY.md](../SECURITY.md). Please use private
disclosure.

---

## Architecture

Arcen is one rule repeated: **put it in `shared/` unless the operating system
forbids it.**

Eleven shared crates hold the logic. They are pure Rust with no operating-system
calls, which means they compile everywhere, they are tested everywhere, and a
new platform inherits them for free.

| Crate | Holds |
|---|---|
| [`protocol`](../shared/protocol) | The wire format. The single source of truth for what a Pier and a Deck say to each other. |
| [`transport`](../shared/transport) | QUIC and TLS: certificate lifecycle, validation, pinning. |
| [`media`](../shared/media) | Codec negotiation, colour, clipboard rules, video plane maths. |
| [`input`](../shared/input) | Mouse, keyboard, and pen: motion, ordering, cursor authority. |
| [`outputs`](../shared/outputs) | Monitor lifecycle and multi-display arrangement. |
| [`session`](../shared/session) | Reconnection and session-restore decisions. |
| [`identity`](../shared/identity) | Resume grants and acknowledgement evidence. |
| [`keel`](../shared/keel) | Damage tracking: which 16×16 blocks changed. |
| [`telemetry`](../shared/telemetry) | The event vocabulary. |
| [`observability`](../shared/observability) | Structured logging that cannot block a video thread. |
| [`usb-bridge`](../shared/usb-bridge) | USB passthrough policy and state. |

Two rules keep this honest, and CI enforces both:

1. **Shared crates never depend on a host, a client, or packaging.** The
   dependency arrow points one way.
2. **Shared crates stay light.** `arcen-transport` must not drag a QUIC runtime
   into a program that only wanted certificate handling, and `arcen-media` must
   not drag in a native codec build. CI proves this by inspecting the dependency
   tree, not by trusting the author.

Platform code does only what platforms must: capture pixels, talk to a GPU
encoder, inject input, authenticate a user, put a window on screen.

---

## Writing a client for another platform

This is the most useful contribution available, and it is smaller than it looks.

**Most of the Deck is already portable.** Of roughly 108,000 lines, about
18,000 are the parts a port replaces. The user interface uses `egui`, which already runs on
Windows and Linux. The audio pipeline, the frame queue, and the multi-monitor
logic contain no macOS calls whatsoever.

What you would actually replace:

| Part | Lines | Why it is platform-specific |
|---|---|---|
| `pipeline/video_decoder.rs` | 5,152 | Uses VideoToolbox. Windows would use Media Foundation or D3D11; Linux, VA-API. **This is the real work.** |
| `ui/video_metal_layer.rs` | 5,272 | Presents decoded frames through a dedicated `CAMetalLayer`, including the ten-bit and EDR path. |
| `display/` | 3,572 | Reading monitor layout and scale factors. |
| `tablet/` | 2,642 | Pen pressure and tilt. |
| `hid/` | 1,338 | Device passthrough. |

What you would keep unchanged: every shared crate, the whole user interface, the
QUIC transport, the audio path, the frame queue, and the reconnection logic.

A sensible order:

1. Connect and authenticate, using `arcen-transport` and `arcen-protocol`.
2. Decode video with your platform's decoder and display it.
3. Send input through `arcen-input`.
4. Add audio, then multi-monitor, then devices.

Read [`clients/macos/ARCHITECTURE.md`](../clients/macos/ARCHITECTURE.md) first. It
documents the decisions and the reasons, including the ones that were wrong the
first time.
