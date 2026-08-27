# Arcen

**The span between you and your machine.** Arcen streams a real desktop from a
powerful workstation to a thin client, over an encrypted direct connection.

Native Rust throughout. Free software under the **AGPL-3.0**.

> **There is no support.** Arcen is published because publishing it is more
> useful than letting it rot in a private repository. No company stands behind
> it and nobody is on call. See [SUPPORT.md](SUPPORT.md).

---

## What works today

Read this before you invest time.

| | State |
|---|---|
| **Linux Pier** (host) | Works. NVENC hardware encoding up to HEVC 4:4:4 10-bit, or software encoding on a machine with no GPU. |
| **Windows Pier** (host) | Works. Same range: NVENC up to HEVC 4:4:4 10-bit, or software encoding. |
| **macOS Deck** (client) | Works. Signed and notarised. |
| **Linux Deck, Windows Deck** | Do not exist. [Help wanted.](#where-help-is-wanted) |
| **macOS Pier** | Does not exist. |
| **Gateway (internet traversal)** | Not shipped. It was never finished, so it is not published as dead code. It could return — see [below](#where-help-is-wanted). |

Every claim above was tested on real hardware, not merely compiled:

- Linux Pier → macOS Deck: `native-nvenc`, 4:4:4, 893 frames, none dropped.
- Windows Pier → macOS Deck: `openh264-sw-h264` on a machine with no GPU at
  all, 1266 frames.
- 710 Linux, 676 Windows, 926 macOS, 694 shared-crate tests pass.

### What a session carries

| | |
|---|---|
| **Video** | H.264, HEVC and AV1 on NVENC, up to HEVC 4:4:4 10-bit full range. OpenH264 in software where there is no GPU. |
| **Multiple monitors** | Up to four, negotiated as one topology. The Deck can match the host's layout to its own windows. |
| **Audio out** | 48 kHz stereo, Opus-compressed by default, or uncompressed PCM if you would rather spend bandwidth than CPU. |
| **Microphone in** | **Linux hosts only.** Client microphone into the host session, Opus or fixed-rate PCM. Opt-in every launch — consent is deliberately never restored from settings — and the operator must enable it on the host too. Windows needs a signed driver Arcen does not yet ship; see [Where help is wanted](#where-help-is-wanted). |
| **Keyboard and pointer** | Absolute and relative motion, scroll, and negotiated cursor authority — the host draws the cursor, or the client does, but never both. |
| **Pen and tablet** | Pressure and tilt, as real pen events rather than mouse clicks. Linux and Windows hosts both. |
| **Clipboard** | Text and images, both directions, or restricted to one, or off. The host decides; the client cannot override it. |
| **Timezone** | The session follows the client's timezone, so timestamps read the way you expect. |
| **Login banner** | Off by default. A host can require the user to read and accept an operator-written notice *before* the Deck collects any credentials, with the exact text recorded. Useful where a legal warning is mandatory. |
| **Reconnection** | A dropped connection resumes the same session for up to two hours. The Deck reattaches with a signed grant instead of asking for the password again. |
| **Deskside privacy** | Off by default. When enabled, the physical screen is blanked and its keyboard and mouse are disabled for the duration of a remote session, so nobody standing at the machine can watch or interfere. Input and display are locked together — you cannot get one without the other. |

A graphics tablet has **three modes**, chosen per connection. The choice is
really about the network, because it decides where the pen is interpreted:

- **Tablet support** — the default, and what you want over Wi-Fi, 5G, or any
  distance. This Mac's own Wacom driver reads the pen and Arcen sends finished
  pen events to the host, so no pen sample waits for a reply and the host needs
  no Wacom driver at all. Pressure, tilt, rotation, eraser, proximity and barrel
  buttons all work. Finger touch and the tablet's own buttons stay on the Mac,
  and the tablet keeps working in Mac applications while you are connected.
- **Native tablet (USB bridged)** — for a LAN. As close to plugging the tablet
  into the host as a network allows: a privileged Arcen helper takes the device
  away from macOS and forwards its raw USB traffic, and the host's own Wacom
  driver claims it as if it were plugged in there. Arcen never interprets the
  pen, so the whole device works — including finger touch and the tablet's own
  buttons. In exchange, every pen sample makes a full round trip, so this is not
  a WAN mode, and Mac applications cannot use the tablet until you disconnect.
- **Mouse compatibility only** — no tablet redirection. The pen behaves as a
  mouse, with no pressure, tilt, eraser or proximity.

Native tablet needs a host that can present the tablet on a virtual USB
controller so the host's own driver can claim it. **Linux hosts can. Windows
hosts cannot yet** — that needs a signed driver Arcen does not ship, and it is
planned rather than abandoned; see
[Where help is wanted](#where-help-is-wanted). Tablet support is unaffected on
Windows and needs nothing installed there.

On macOS the privileged part is a small root daemon registered through
`SMAppService`, not the Deck: Arcen Deck itself never runs as root. Installing
it is one approval in Login Items. See
[ADR 0011](docs/adr/0011-macos-privileged-usb-helper.md).

Keyboard, pointer and scroll are synthesised on both hosts: they arrive as
native events the operating system generates on Arcen's behalf. On Linux this
goes through the kernel's own input layer; on Windows through the injection API.

**Not supported:** webcam redirection of any kind, and USB passthrough for
anything other than the one tablet class above. Neither is a small gap —
[`docs/architecture/macos-peripheral-access.md`](docs/architecture/macos-peripheral-access.md)
sets out what a webcam would take and which of the two routes to try first.

Arcen expects a **trusted network**. It is not hardened against the open
internet. Put it behind a VPN or a network you control.

---

## Install

### The host (Pier)

Download the installer for your platform from
[Releases](https://github.com/Aanerud/arcen_public/releases) and run it with
administrative rights. One file. No runtime to install first, no package
repository to add, no service file to write.

**Linux** — needs systemd, `openssl`, and root:

```sh
sudo ./install-arcen-pier
```

**Windows** — open **PowerShell as Administrator**, then run the installer from
the folder you downloaded it to:

```powershell
.\install-arcen-pier.exe
```

The installer creates the directories, generates a TLS certificate, registers
and starts the service, and opens **UDP 18444** on a firewall it recognises.

On Windows, **reboot afterwards**. Windows reads the list of credential
providers only when the login screen starts, so the provider Arcen just
installed stays invisible until the machine restarts. Skip the reboot and your
first remote sign-in fails, asking you to install something that is already
installed.

Useful flags on both: `--dry-run` shows what would happen and changes nothing;
`--uninstall --purge` removes everything.

### The client (Deck)

Download `Arcen-Deck-<version>-macOS.zip`, unzip it, and drag the app to
`/Applications`. It is signed and notarised, so it opens normally.

---

## Connect

1. Open Arcen Deck.
2. Enter the Pier's address. The port is 18444.
3. The first connection shows the host's certificate fingerprint. Compare it
   with the Pier and choose **trust and remember**.
4. Sign in with an ordinary account on the host machine.

Arcen does not keep its own list of users. It asks the operating system — PAM on
Linux, the normal login path on Windows — so the account you already have is the
account you use.

### Test without the interface

The Deck can connect from a terminal, which is useful for checking a host:

```sh
"Arcen Deck.app/Contents/MacOS/arcen-deck" \
  --connect <host> 18444 \
  --credentials-stdin \
  --frames 120 --timeout-secs 60
```

It reads the username and password from standard input, two lines, so neither
appears in your shell history. Progress goes to
`~/Library/Logs/Arcen/`. Look for `stream healthy` and a rising
`frames_decoded`.

---

## How it works

A **Pier** captures a desktop, encodes it, and sends it. A **Deck** receives it,
decodes it, and sends your keyboard and mouse back. Everything else is detail.

**One connection, one protocol.** Arcen speaks QUIC over UDP 18444 and nothing
else. There is no TCP fallback and no unencrypted mode. QUIC carries TLS 1.3
itself, so the encryption is part of the transport rather than bolted on.

**The client picks nothing.** The Deck reports what it can decode and what
quality it wants. The Pier decides, because only the Pier knows which encoder
its GPU actually has. If the GPU cannot do what you asked, the Pier serves a
lower-fidelity plan and says so, rather than pretending.

**Codec choice follows the hardware — at both ends.** The Pier can only send
what the Deck can actually decode, so the Deck measures its own capability at
startup and reports it in the handshake.

On macOS that means VideoToolbox, and the answer differs by machine. Every Mac
Arcen supports decodes H.264 and HEVC in hardware, but **AV1 hardware decode
arrived with the M3**. An M1, an M2, or any Intel Mac reports no AV1, and the
Pier picks HEVC instead. Nothing degrades silently; the negotiation simply lands
somewhere else.

| Host has | Deck can decode | Arcen uses |
|---|---|---|
| Modern NVIDIA GPU | AV1 (M3 or later) | Hardware AV1, encoded and decoded in hardware |
| Modern NVIDIA GPU | No AV1 (M1, M2, Intel) | Hardware HEVC |
| Older NVIDIA GPU | HEVC | Hardware HEVC, then H.264 |
| Any NVENC GPU | HEVC 4:4:4 10-bit | Hardware HEVC 4:4:4 10-bit, for colour-critical work |
| No GPU at all | anything | OpenH264 in software, decoded in hardware on the Deck |

Note the last row: software encoding on the host still gets **hardware decoding
on the client**, because the output is ordinary H.264. The cost lands on the
host's CPU, not on the laptop in front of you.

The Deck does not guess. It builds a real decode session for each format and
reports only what succeeded — separately for 4:4:4, 10-bit, 12-bit and colour
range. The code is blunt about why: claiming a capability the Deck has not
demonstrated is worse than admitting it, because the host believes the claim and
sends a stream the Deck then cannot decode.

### Three ways to run a Pier

Hardware encoding is the fast path, but it is not the only intended one.

**On a workstation with an NVIDIA GPU.** The full range, including HEVC 4:4:4
10-bit for colour-critical work. Both the Linux and Windows Piers do this.

**On a virtual machine.** Proxmox, VMware, and similar hypervisors usually give
a guest no encoder at all. Arcen falls back to OpenH264 in software, and this is
a first-class way to run it rather than a consolation prize: it makes Arcen a
practical alternative to RDP or VNC on ordinary server hardware, with the same
QUIC and TLS 1.3 as everywhere else. Desktop work and video playback both hold
up well. You give up 4:4:4 and 10-bit, not encryption and not the protocol.

**On a headless host.** Same as above, provided something gives the machine a
display to capture — a hypervisor's virtual adapter, or an indirect display
driver. Arcen ships no display driver of its own.

The encryption does not change between these. A software-encoded session on a
Proxmox guest is protected exactly as a 4:4:4 workstation session is.

**Only one session at a time.** A Pier drives one physical desktop. Two sessions
would fight over the same screen, the same mouse, and the same encoder, so the
Pier admits one and refuses the rest. If your connection drops, it holds your
session for up to two hours so you can resume where you left off.

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
the real host raised the first time.

**Pins are compared in constant time**, so an attacker cannot learn a pin by
measuring how long a rejection takes. A pin changes only when you change it.

**Authentication belongs to the operating system.** PAM on Linux; on Windows, a
credential provider that takes part in the real login. Passwords are held in
memory that is wiped on release and never written to a log.

**There is no lockout in Arcen.** Repeated password attempts are passed to the
operating system, so throttling is whatever `pam_faillock` or your Windows
account-lockout policy already does. If you expose a Pier to a network you do
not control, configure that policy — Arcen will not do it for you.

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

To report a vulnerability, see [SECURITY.md](SECURITY.md). Please use private
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
| [`protocol`](shared/protocol) | The wire format. The single source of truth for what a Pier and a Deck say to each other. |
| [`transport`](shared/transport) | QUIC and TLS: certificate lifecycle, validation, pinning. |
| [`media`](shared/media) | Codec negotiation, colour, clipboard rules, video plane maths. |
| [`input`](shared/input) | Mouse, keyboard, and pen: motion, ordering, cursor authority. |
| [`outputs`](shared/outputs) | Monitor lifecycle and multi-display arrangement. |
| [`session`](shared/session) | Reconnection and session-restore decisions. |
| [`identity`](shared/identity) | Resume grants and acknowledgement evidence. |
| [`keel`](shared/keel) | Damage tracking: which 16×16 blocks changed. |
| [`telemetry`](shared/telemetry) | The event vocabulary. |
| [`observability`](shared/observability) | Structured logging that cannot block a video thread. |
| [`usb-bridge`](shared/usb-bridge) | USB passthrough policy and state. |

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

**Most of the Deck is already portable.** Of roughly 70,000 lines, about 11,600
touch macOS at all. The user interface uses `egui`, which already runs on
Windows and Linux. The audio pipeline, the frame queue, and the multi-monitor
logic contain no macOS calls whatsoever.

What you would actually replace:

| Part | Lines | Why it is platform-specific |
|---|---|---|
| `pipeline/video_decoder.rs` | 4,590 | Uses VideoToolbox. Windows would use Media Foundation or D3D11; Linux, VA-API. **This is the real work.** |
| `display/` | 3,362 | Reading monitor layout and scale factors. |
| `tablet/` | 2,337 | Pen pressure and tilt. |
| `hid/` | 1,312 | Device passthrough. |

What you would keep unchanged: every shared crate, the whole user interface, the
QUIC transport, the audio path, the frame queue, and the reconnection logic.

A sensible order:

1. Connect and authenticate, using `arcen-transport` and `arcen-protocol`.
2. Decode video with your platform's decoder and display it.
3. Send input through `arcen-input`.
4. Add audio, then multi-monitor, then devices.

Read [`clients/macos/ARCHITECTURE.md`](clients/macos/ARCHITECTURE.md) first. It
documents the decisions and the reasons, including the ones that were wrong the
first time.

---

## Where help is wanted

Ordered by how much they matter.

**A Linux or Windows Deck.** See above. The largest gap and the clearest path.

**Signed Windows drivers — microphone and USB.** Two features are missing on
Windows for exactly the same reason, and solving one solves most of the other.

- **Microphone input.** Works on Linux, which publishes a virtual source through
  PulseAudio and needs nothing installed. Windows has no user-mode way to create
  a recording device at all: an application can *use* audio devices but cannot
  register one, so a virtual microphone must be a kernel-mode driver publishing
  a `KSCATEGORY_CAPTURE` endpoint. Every product that does this ships such a
  driver, including Microsoft's own Remote Desktop. Arcen's `arcen-microphone`
  driver exists in the tree and is excluded from released installers because it
  is unsigned.
- **USB passthrough.** Linux can attach a physical USB device to a session.
  Windows cannot, for the same reason. The design is settled in
  [`docs/adr/0012-hard-usb-on-windows-hosts.md`](docs/adr/0012-hard-usb-on-windows-hosts.md).

Neither is blocked on code. Both need **driver signing**: an EV certificate plus
a Microsoft Partner Center account for attestation, and WHQL/HLK on top of that
for Windows Server. If you already have that pipeline, this is mostly plumbing,
and it is the single most valuable thing anyone could contribute to the Windows
host.

There may also be a cheaper route worth investigating first. Arcen already
adopts a third-party GPL kernel module rather than writing its own for the Linux
USB bridge — see `usb-vhci` in
[`legal/THIRD_PARTY_NOTICES.md`](legal/THIRD_PARTY_NOTICES.md). An
already-signed open-source virtual audio driver could do the same job here and
skip the certificate problem entirely. Anyone taking this on should check that
before paying for a signing pipeline: confirm the licence combines with
AGPL-3.0, and confirm whether its signature satisfies the Windows editions you
care about, since community code-signing is not the same as Microsoft WHQL
attestation.

**A macOS Pier.** Nobody has started. macOS screen capture and session handling
differ enough from Linux and Windows to need real design work first.

**The gateway.** Today a Deck must reach a Pier directly, which in practice
means the same network or a VPN. The gateway would carry traffic between them
across the internet: one public port, many sessions multiplexed inside a single
QUIC connection, with federated identity so the host never sees a password.

It was designed and partly built, then cut before publication because shipping
an unfinished network boundary is worse than shipping none. The reasoning
survives in the decision records — see ADRs
[0002](docs/adr/0002-transport-evolution.md) and
[0003](docs/adr/0003-authentication-and-entitlement.md) — but the code does not.
Anyone picking it up starts from the design, not from a half-built service.

This one is honest about its economics. It is a large piece of work, and the
author writes Arcen with AI assistance that is billed by the token. The gateway
resumes when that bill is comfortably covered — or sooner, if someone else wants
to build it. Both routes are open, and the [coffee link](#if-it-is-useful-to-you)
is the first one.

**Smaller, self-contained pieces:**

- Webcam redirection. Nothing exists. The design question — capture through
  AVFoundation with ordinary consent, or pass the USB device through with a
  privileged helper — is laid out in
  [`docs/architecture/macos-peripheral-access.md`](docs/architecture/macos-peripheral-access.md),
  along with the measurement that decides it.
- Wayland capture on Linux, alongside the current Xorg path.
- AMD and Intel hardware encoding. Only NVENC exists today.
- Better software encoding for hosts with no GPU.

**Latency, if you like measuring things.** A performance review traced a frame
from capture to glass and an input event from the tablet to the host. The
pipeline is already bounded and drop-aware in the right places — frame queues
discard prediction chains rather than block, NVENC runs with no B-frames and no
lookahead, the media worker is off the UI thread — so what is left is copies and
serialisation, ranked here by what a measurement would actually show at 4K60.

- **Zero-copy presentation on the Deck.** A decoded frame is transferred to a
  BGRA `CVPixelBuffer`, swizzled into an RGBA `Vec`, then copied again for the
  WGPU upload. At 4K that is roughly 33 MiB per frame of avoidable traffic, and
  10-bit frames pay it even though the native `CVPixelBuffer` is already kept
  for the dedicated Metal layer. A `CVMetalTextureCache` path would remove it.
- **A bounded async decode window.** VideoToolbox is currently made synchronous
  — `wait_for_async_frames` immediately after every submit — which serialises the
  worker behind the decoder and turns any hiccup into a stall. Two frames in
  flight would overlap receive, decode and present without reordering.
- **GPU-side colour conversion on the host.** Windows maps the capture staging
  texture and converts BGRA to the encoder's format on the CPU; the Linux 10-bit
  path copies device to host, converts, and copies back. This is raw-frame
  traffic, not compressed, so it dominates the fidelity modes.
- **Binary input encoding.** Every mouse, key and pen event becomes a
  `serde_json::Value` and travels on an unbounded channel. Fine for a mouse;
  questionable for a 1000 Hz pen, where the queue can also become a latency
  buffer rather than a backlog you drop.

### How to contribute

Fork, branch, open a pull request. There is no contributor licence agreement:
your work stays under the AGPL-3.0, like everything else here.

Two things asked of any change:

1. **Put logic in `shared/` if it can live there.** Code in `clients/` or
   `hosts/` should be code that genuinely cannot be written portably.
2. **Say what you actually built and ran.** CI does not build the platform
   crates (see below), so your word is the evidence. "Built on Linux, ran the
   Pier tests, did not try Windows" is a good pull request description. Silence
   is not.

Every directory has an `AGENTS.md` that states who owns it and how to validate
it. Read the one for the area you are changing.

---

## Build

The workspace does not build in one command, and that is deliberate: the client
is macOS-only, the Windows host is Windows-only, the Linux host is Linux-only.

**Anywhere** — the shared crates:

```sh
cargo test --locked -p arcen-identity -p arcen-input -p arcen-keel \
  -p arcen-media -p arcen-observability -p arcen-outputs -p arcen-protocol \
  -p arcen-session -p arcen-telemetry -p arcen-transport -p arcen-usb-bridge
cargo clippy --locked -p arcen-protocol -p arcen-transport -- -D warnings
python3 -m unittest scripts/test_validate_observability.py
```

**Each platform, on that platform:**

```sh
# macOS
cargo build --release -p arcen-deck-macos
packaging/macos/build-deck-app.sh              # produces Arcen Deck.app

# Linux — needs libpam0g-dev and libpulse-dev
cargo build --locked --release -p arcen-pier-linux
ARCEN_PIER_BINARY=target/release/arcen-pier \
  cargo build --locked --release -p arcen-pier-linux-installer

# Windows, MSVC
hosts\windows\build.cmd
```

Before proposing a change, run the publication check. It refuses private
addresses, key material, vendor SDK payloads, and any crate that forgets to
declare its licence:

```sh
scripts/ci/check-publication-hygiene.sh
```

Releasing the macOS client requires signing and notarisation. The procedure is
in [`docs/operations/macos-signing.md`](docs/operations/macos-signing.md).

---

## Continuous integration

CI runs **manually**, on **Linux only**, and covers the shared crates, the strict
lint gate, and the publication check.

It does not build the platform crates. They need three different operating
systems, and a gate that cannot pass on the machine it runs on teaches nobody
anything. What runs here passes, so a green result means something. Platform
builds are the contributor's responsibility.

---

## Documentation

| Where | What |
|---|---|
| [`docs/architecture/`](docs/architecture) | How each part works and why |
| [`docs/adr/`](docs/adr) | Decisions, including withdrawn ones, with reasons |
| [`docs/operations/`](docs/operations) | Running and releasing |
| [`docs/security/`](docs/security) | Trust boundaries |
| `*/ARCHITECTURE.md` | Component detail, beside the code |
| `*/AGENTS.md` | Who owns a directory and how to validate it |

Decisions that were reversed are kept and marked withdrawn. A record that only
shows the wins teaches nothing.

---

## Origins

Arcen is original work. Existing remote-desktop and pixel-streaming systems
informed it — the shape of the problem, the vocabulary, the performance bar —
but no third-party source, SDK payload, or vendor implementation was copied into
it. See [`legal/ORIGINS.md`](legal/ORIGINS.md).

Third-party open-source components keep their own licences, recorded in full in
[`legal/THIRD_PARTY_NOTICES.md`](legal/THIRD_PARTY_NOTICES.md).

---

## Licence

**GNU Affero General Public License v3.0 only.** See [LICENSE](LICENSE).

Use it, study it, change it, pass it on. If you distribute a modified version —
**or run one as a service other people connect to** — you must offer those
people the source, under this same licence. That network clause is the point:
Arcen is remote-access software, and the AGPL is what keeps a modified, hosted
Arcen free as well.

There is no commercial edition, no licence key, and no enforcement code. There
never will be.

---

## If it is useful to you

Development continues while it stays interesting and the tooling bill gets paid.

**[Buy me a coffee](https://wise.com/pay/me/andreasmartina)**

Optional. It buys no support, no priority, and no influence.
