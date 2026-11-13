# The Arcen project: roadmap, help wanted, building, licensing

Moved from the README (0.19.0).

## Roadmap

What comes next, from the open items of the current release. Nothing here
is promised or dated; each item ships when it is proven on real hardware.

**Colour**

- **Bit-exact still pictures.** Lossless refinement of still Grading and HDR
  blocks ([ADR 0016](adr/0016-lossless-refinement.md), proposed): ship
  it off by default, qualify it per Pier, then let the Colour check report a
  still picture as bit-exact instead of close.
- **HDR at Retina and larger sizes on Windows.** The HDR display Arcen
  describes to Windows declares an HDMI link that cannot carry ten-bit 60 Hz
  at Retina sizes, so from 0.18.0 such a display runs at 30 Hz (the HDR
  stream's own rate), and 5K and 6K sizes do not fit the display description
  (EDID) at all. HDMI 2.1 FRL link rates for 60 Hz HDR displays and a
  DisplayID block for 5K and 6K are the routes to try.

**Pen**

- **One typed-device model.** The Deck announces each input device by type
  and capability, and the host builds the matching OS-native device, for any
  tablet at any distance (design in a separate ADR).
- **A real pen on headless Wayland desktops**, when GNOME's input emulation
  offers a stylus (see [Where help is wanted](#where-help-is-wanted)).
- **Not planned: Wintab, and Native tablet on Windows hosts.** Windows hosts
  deliver pens through Windows Ink only; set drawing applications to Windows
  Ink ([settings per application](operations/pen-tablet-input.md#4a-windows-applications-choose-windows-ink)).
  Native tablet on Windows would need a signed kernel driver the project does
  not fund ([ADR 0017](adr/0017-forward-only-pen-input.md)).

**Keyboard and cursor**

- **Wayland text beyond the layout.** Dead-key sequences, and characters the
  session's layout cannot type, which need libei's `ei_text` in Mutter.
- **The macOS Pier shows cursor images.** It sends named cursor shapes: no
  public macOS API reports another application's cursor image
  (`NSCursor.currentSystemCursor` reports the calling process's own, measured),
  and ScreenCaptureKit carries no cursor metadata. Host-cursor mode draws the
  real cursor into the picture instead.

**Network**

- **Forward error correction only if loss data justifies it.** Arcen measures
  the path first; FEC adds bandwidth to every frame and is not added on a
  guess.

**Hosts**

- **The macOS Pier as a supported host**: cold-boot login-window operation,
  multi-display on physical hardware.

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
- **USB passthrough.** A Linux Pier can attach a Deck's physical pen tablet
  to a session (Native tablet, administrator opt-in). Windows hosts do not offer it, for the same reason: the
  project does not fund the signing pipeline
  ([ADR 0017](adr/0017-forward-only-pen-input.md)). The route is recorded
  in [`docs/adr/0012-hard-usb-on-windows-hosts.md`](adr/0012-hard-usb-on-windows-hosts.md)
  and is revisited only if that pipeline is funded.

Neither is blocked on code. Both need **driver signing**: an EV certificate plus
a Microsoft Partner Center account for attestation, and WHQL/HLK on top of that
for Windows Server. If you already have that pipeline, this is mostly plumbing,
and it is the single most valuable thing anyone could contribute to the Windows
host.

There may also be a cheaper route worth investigating first. The Linux USB
bridge writes no kernel code at all: it uses the distribution's signed in-tree
`vhci-hcd` ([ADR 0020](adr/0020-linux-native-tablet-in-tree-vhci-hcd.md)),
and its lab fallback adopts a third-party GPL module (`usb-vhci` in
[`legal/THIRD_PARTY_NOTICES.md`](../legal/THIRD_PARTY_NOTICES.md)). An
already-signed open-source virtual audio driver could do the same job here and
skip the certificate problem entirely. Anyone taking this on should check that
before paying for a signing pipeline: confirm the licence combines with
AGPL-3.0, and confirm whether its signature satisfies the Windows editions you
care about, since community code-signing is not the same as Microsoft WHQL
attestation.

**Finishing the macOS Pier.** It ships as a preview: a native Rust host built
on the same shared contracts as the others, with display creation, capture,
input, clipboard, audio and packaging kept as thin macOS adapters. The
logged-in path is used from a Deck every day. What is missing is listed in
the table at the top: deskside privacy needs only a macOS adapter, since its
wire messages, configuration and policy already live in `shared/`, and the
virtual microphone that now ships needs end-to-end qualification.
Multi-display creates a virtual display per Deck display, but has not been
qualified with physical monitors attached to the Mac. Serving
the macOS login screen needs a `LoginWindow` launch agent and virtual-HID input,
because `CGEvent` posting does not reach that screen. Apple has now granted the
virtual-HID entitlement, and both pieces are built. Apple still publishes no
contract that third-party code may capture the login window, and cold-boot
operation has not been qualified. That agent also runs as root, the one place the
Pier holds more privilege than it needs; narrowing it to the virtual keyboard
alone is open work, described in
[`docs/security/macos-pier-process-model.md`](security/macos-pier-process-model.md).
FileVault's pre-boot disk-unlock screen is not supported, for a different and
simpler reason: macOS is not running yet.

**Smaller, self-contained pieces:**

- Webcam redirection. Nothing exists. The design question — capture through
  AVFoundation with ordinary consent, or pass the USB device through with a
  privileged helper — is laid out in
  [`docs/architecture/macos-peripheral-access.md`](architecture/macos-peripheral-access.md),
  along with the measurement that decides it.
- A real pen on Linux Wayland: an EIS stylus (`ei_stylus`) in Mutter, or
  headless libinput (mutter!5224) upstream, and other compositors than GNOME's.
- A Keel damage source for the Linux NvFBC fast path (the GL interface
  reports a change map that the CUDA interface does not). Lossless refinement
  is on the [roadmap](#roadmap).
- AMD and Intel hardware encoding. Only NVENC exists today.
- Better software encoding for hosts with no GPU.

**Latency, if you like measuring things.** A performance review traced a frame
from capture to glass and an input event from the tablet to the host. The
pipeline is already bounded and drop-aware in the right places — frame queues
discard prediction chains rather than block, NVENC runs with no B-frames and no
lookahead, the media worker is off the UI thread — so what is left is copies and
serialisation, ranked here by what a measurement would actually show at 4K60.

- **A bounded async decode window.** VideoToolbox is currently made synchronous
  — `wait_for_async_frames` immediately after every submit — which serialises the
  worker behind the decoder and turns any hiccup into a stall. Two frames in
  flight would overlap receive, decode and present without reordering.
- **GPU-side colour conversion on the host.** Windows maps BGRA8 or FP16 WGC
  staging and performs the negotiated conversion on the CPU. Linux Grading
  starts in host memory because XShm is the ten-bit source, converts there, then
  uploads once to CUDA. This is raw-frame traffic, not compressed, so it
  dominates the fidelity modes without affecting the separate NvFBC fast path.
- **Binary input encoding.** Every mouse, key and pen event becomes a
  `serde_json::Value` and travels on an unbounded channel. Fine for a mouse;
  questionable for a 1000 Hz pen, where the queue can also become a latency
  buffer rather than a backlog you drop.

### How to contribute

Fork, branch, open a pull request — [CONTRIBUTING.md](../CONTRIBUTING.md) walks
through it, including the [Contributor License Agreement](../CLA.md) you accept
once, with a statement in your first pull request, and how contributors are
credited. In short: put logic in `shared/` if it can live there, and say what
you actually built and ran.

---

## Building from source

The workspace does not build in one command, and that is deliberate: the client
is macOS-only, the Windows host is Windows-only, the Linux host is Linux-only.

**Prerequisites checklist:**

- [ ] **Rust, pinned.** Install [rustup](https://rustup.rs); it reads
      [`rust-toolchain.toml`](../rust-toolchain.toml) and installs the pinned
      channel with `clippy` and `rustfmt`. Do not build with a distribution
      or Homebrew `cargo` ahead of rustup's on `PATH`.
- [ ] **CMake** on every platform (the vendored Opus build needs it).
- [ ] **macOS:** Xcode or the Command Line Tools (an Intel Mac builds with
      Command Line Tools 14.2).
- [ ] **Linux:** a C toolchain, `libpam0g-dev` and `libpulse-dev`
      (`pam-devel`, `pulseaudio-libs-devel` on Rocky/RHEL); for the release
      Pier's `wayland-provider`, the PipeWire development headers and libclang
      (`pipewire-devel`, `clang-devel`). NVENC capture needs an NVIDIA GPU and
      driver on the test host.
- [ ] **Windows:** Visual Studio with the MSVC x64 toolchain, run from an x64
      Developer Command Prompt with CMake on `PATH`; the host triple must be
      `x86_64-pc-windows-msvc`.
- [ ] **Python 3.9+** for the gate scripts under `scripts/`.

**Anywhere** — the shared crates:

```sh
cargo test --locked -p arcen-identity -p arcen-input -p arcen-keel \
  -p arcen-media -p arcen-observability -p arcen-outputs -p arcen-protocol \
  -p arcen-session -p arcen-telemetry -p arcen-transport -p arcen-usb-bridge
cargo clippy --locked -p arcen-identity -p arcen-input -p arcen-keel \
  -p arcen-media -p arcen-observability -p arcen-outputs -p arcen-protocol \
  -p arcen-session -p arcen-telemetry -p arcen-transport -p arcen-usb-bridge \
  -- -D warnings
python3 -m unittest scripts/test_validate_observability.py
```

The complete gate list, including the feature builds, the shared-contract
check and the dependency-purity proofs, is in [`AGENTS.md`](../AGENTS.md#validating-a-change).

**Each platform, on that platform:**

```sh
# macOS
cargo build --release -p arcen-deck-macos
packaging/macos/build-deck-app.sh              # dist/macos/Arcen Deck.app on Apple silicon,
                                               # dist/macos/Arcen Deck Intel.app on Intel
packaging/macos/build-deck-app.sh --arch x86_64   # the Intel app from an Apple-silicon Mac
# Each app targets one architecture and macOS 12; see packaging/macos/README.md.
# On an Intel Mac, Command Line Tools 14.2 suffice.

# macOS Pier (macOS 14.2 or later)
cargo test --locked -p arcen-pier-macos
cargo build --locked --release -p arcen-pier-macos
packaging/macos/build-pier-pkg.sh               # dist/macos/ArcenPier-<version>.pkg; signing flags in the script header

# Linux — needs libpam0g-dev and libpulse-dev; release Piers add
# wayland-provider, which also needs the PipeWire development headers and
# libclang (without it the Pier serves X11 only)
cargo build --locked --release -p arcen-pier-linux --features wayland-provider
ARCEN_PIER_BINARY=target/release/arcen-pier \
  cargo build --locked --release -p arcen-pier-linux-installer

# Windows, MSVC
hosts\windows\build.cmd
```

Each platform's tests, and what else to validate there, are in that
directory's `AGENTS.md`: [macOS Deck](../clients/macos/AGENTS.md),
[macOS Pier](../hosts/macos/AGENTS.md), [Linux Pier](../hosts/linux/AGENTS.md)
and [Windows Pier](../hosts/windows/AGENTS.md).

Before proposing a change, run the publication check. It refuses private
addresses, key material, vendor SDK payloads, and any crate that forgets to
declare its licence:

```sh
scripts/ci/check-publication-hygiene.sh
```

Releasing the macOS client requires signing and notarisation. The procedure is
in [`docs/operations/macos-signing.md`](operations/macos-signing.md).

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
| [`docs/user-guide.md`](user-guide.md) | The Deck user guide for artists and editors, task by task |
| [`STRUCTURE.md`](../STRUCTURE.md) | What every top-level file and folder is, and who owns it |
| [`docs/architecture/`](architecture) | How each part works and why |
| [`docs/adr/`](adr) | Decisions, including withdrawn ones, with reasons |
| [`docs/operations/`](operations) | Running and releasing: the [administration guide](operations/pier-administration.md), [configuration reference](operations/config-reference.md), [error codes](operations/error-codes.md), [troubleshooting](operations/troubleshooting.md), the [colour check guide](operations/colour-check.md) and [pen and tablet input](operations/pen-tablet-input.md) |
| [`docs/testing/`](testing) | The [colour probe matrix](testing/README.md) and the [real-screen harness](testing/realscreen.md) for feel and latency |
| [`docs/security/`](security) | Trust boundaries, [why the macOS Pier runs as separate processes](security/macos-pier-process-model.md), and [the Linux Pier under SELinux](security/linux-selinux.md) |
| `*/ARCHITECTURE.md` | Component detail, beside the code |
| `*/AGENTS.md` | Who owns a directory and how to validate it |

Decisions that were reversed are kept and marked withdrawn. A record that only
shows the wins teaches nothing.

---

## Origins

Arcen is original work. Existing remote-desktop and pixel-streaming systems
informed it — the shape of the problem, the vocabulary, the performance bar —
but no third-party source, SDK payload, or vendor implementation was copied into
it. See [`legal/ORIGINS.md`](../legal/ORIGINS.md).

Third-party open-source components keep their own licences, recorded in full in
[`legal/THIRD_PARTY_NOTICES.md`](../legal/THIRD_PARTY_NOTICES.md).

---

## Licensing and official builds

**Arcen Deck and Arcen Pier are free software.** Anyone can download, build,
run, study, change and share them — a home user with one workstation, a small
studio, a large facility. That is intended, and it is welcome.

The licence is the **GNU Affero General Public License, version 3 only**
([LICENSE](../LICENSE)), with one additional permission, the
[Arcen Store Distribution Exception](../LICENSE-EXCEPTION.md):

```text
SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
```

The `Cargo.toml` files say `license = "AGPL-3.0-only"`, because Cargo's
licence field has no way to name a project-specific exception; the exception
applies to every first-party file that carries it, as each file's header (or
[`REUSE.toml`](../REUSE.toml) for files without one) states. Arcen's patches
and notices for vendored third-party code are offered under that code's own
upstream-compatible licence (MIT, Apache-2.0 or BSD), as `REUSE.toml` records.
The exception lets anyone offer Arcen free of charge through an app store, even
where the store's own terms would otherwise conflict with the AGPL, as long as
the matching source is public and the build does not use the Arcen names. It
only adds a permission; it takes nothing away from the AGPL, and you may remove
it from your own modified versions. It grants no rights in third-party
components (section 6): those keep their own licences. The macOS Deck and its
USB helper, for example, statically link libusb, which is LGPL-2.1-or-later,
so anyone distributing a Deck build, through a store or otherwise, must meet
the LGPL's terms for it as well.

The AGPL's network clause matters for remote-access software: if you run a
modified Arcen that other people connect to, you must offer those people your
source, under the same licence.

**Official builds** come from TUTTIFRUTTI AS, which owns Arcen and is its
official distributor ([NOTICE](../NOTICE)). They exist mainly to provide what a
download from a stranger cannot: code signing, Apple notarisation, the
platform trust chains that let an operating system run the software without
warnings, a verified publisher identity, and smooth installation and updates.
They are a trust and convenience service, not a gate on the software: the
source they are built from is all in the public repository,
https://github.com/Aanerud/arcen_public.

**Pre-installed cloud workstation images and support** are paid services from
TUTTIFRUTTI AS, and they are what fund the project.

**Every official release links the exact source tag it was built from**, and
every official binary carries a build ID naming that release and revision
([how to check one](operations/versioning-and-releases.md#telling-an-official-build-apart)).

The names **Arcen**, **Arcen Deck** and **Arcen Pier** and the Arcen logos are
trademarks of TUTTIFRUTTI AS. They are not covered by the AGPL. If you
distribute your own builds or a fork, give them your own name;
[TRADEMARKS.md](../TRADEMARKS.md) explains what is allowed, including plain
references such as "based on Arcen".

---

## Community

**Forks and derivative work are welcome**, and they will be recognised. A
port to another platform, an integration with a studio's pipeline, support for
a new device, an experiment that goes somewhere this project never planned —
all of it is the point of free software.

- **Notable forks, ports and integrations** can be linked from
  [CONTRIBUTORS.md](../CONTRIBUTORS.md#forks-ports-and-integrations) on request:
  open an issue with a link and a sentence about what it does.
- **Work that comes back upstream** is credited by name or handle, as its
  author chooses, in [CONTRIBUTORS.md](../CONTRIBUTORS.md) and in the release
  notes of the release that contains it ([how credit works](../CONTRIBUTING.md#how-credit-works)).
- Forks use their own name and branding ([TRADEMARKS.md](../TRADEMARKS.md)).
