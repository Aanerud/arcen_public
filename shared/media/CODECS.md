# Codecs: adding, changing, and updating

This is the runbook for changing what Arcen can encode. It is written so a
competent engineer who has never read this crate can ship a new codec safely in
an afternoon.

Read the whole page before starting. It is short on purpose.

## The model, in one paragraph

Capability is a **set**, not a boolean per codec. A backend declares once what
it can encode; a codec declares once what Arcen offers for it; everything else —
plan resolution, fallback, READY validation, the hosts, the client — reads those
two tables. That is why adding a codec is a small, bounded change rather than a
field threaded through every layer. If you find yourself writing
`if codec == VideoCodec::Something` outside those tables, stop: the design has
somewhere to put that fact, and it is not there.

The two tables are:

| Table | Location | States |
| --- | --- | --- |
| `EncoderBackend::contract()` | `src/video/plan.rs` | what a **backend** can encode, and its geometry/rate ceiling |
| `VideoCodec::offered_chroma()` | `src/lib.rs` | which chroma Arcen **offers** for a codec |

The second is a product decision, not a capability. H.264 High 4:4:4 exists;
Arcen does not offer it, because clients hardware-decode 4:4:4 only via HEVC.
Stating that once is why no validation path has to re-derive it.

---

## Adding a new codec

Worked example: adding AV1.

### 1. Extend the vocabulary — `src/lib.rs`

```rust
pub enum VideoCodec {
    // ...
    Av1,
}
```

Then three small edits in the same file, all of which the compiler forces:

- add it to `VideoCodec::ALL`
- give it a token in `VideoCodec::token()`
- give it a bit in `VideoCodec::bit_index()`
- give it a row in `VideoCodec::offered_chroma()`

`bit_index` values must be unique and stable. **Never renumber an existing
codec**: the bits are not on the wire today, but the tokens are, and renumbering
invites a mismatch the type system cannot see. Append.

The test `every_codec_has_a_unique_bit_and_round_trips_through_its_token` fails
if you forget the bit or the token, which is the point.

### 2. Say which backends can encode it — `src/video/plan.rs`

Add it to the `codecs:` set of every backend whose contract now includes it:

```rust
Self::NativeNvenc => BackendLimits {
    codecs: CodecSet::from_slice(&[VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1]),
    // ...
},
```

Be honest here. This table is the source of truth for fallback and for READY
validation, so claiming a codec a backend cannot actually produce turns a clean
negotiation failure into a session that dies mid-handshake.

### 3. Decide preference order — `src/video/plan.rs`

`CODEC_PREFERENCE` is the order tried when the requested codec cannot be served.
Insert the new codec where it should rank. Leaving it out is legal and means
"never fall back *to* this codec", which is often right for a codec that is
newer or less widely decodable.

### 4. Implement or bind the encoder

Portable, CPU-side codecs live in `shared/media` behind a Cargo feature, next to
`software_h264.rs`. Platform or vendor encoders live in `hosts/capenc`, because
they need FFI and native resource ownership, and `shared/media` is
`#![forbid(unsafe_code)]` and platform-free. **Do not** relax that to make a
binding fit.

If the backend is new rather than just the codec, see the next section.

### 5. Client decode

A codec the host can encode and the client cannot decode is worse than not
having it. Update the Deck decoder and its advertised capabilities together with
the host, in the same change.

### 6. Verify

```sh
cargo test  --locked -p arcen-media
cargo clippy --locked -p arcen-media -- -D warnings
cargo test  --locked -p arcen-pier-linux
```

Then prove it on hardware, because a plan that resolves is not a session that
streams:

```sh
PROBE_HOST=<host> PROBE_USER=<user> PROBE_PASS="$PW" \
  python3 tools/session-probe/arcen-session-probe.py
```

Assert on `encoder_backend` and `codec` in the output. A test that passes
because it silently fell back to H.264 has told you nothing.

---

## Adding a new backend

A backend is an encoder implementation: NVENC, Media Foundation, OpenH264, and
in future AMF, QuickSync or VA-API.

1. Add the variant to `EncoderBackend` in `src/video/plan.rs`.
2. Give it a `ready_token()`. This is on the wire; choose it once and do not
   change it.
3. Give it an `accelerator_class()` — `Hardware` or `Software`. Clients use this
   to tell the user which path they are on. **Do not** rely on the name: a
   client that guesses from the token will mislabel your backend.
4. Give it a `contract()` row: the codecs and chroma it can encode, and its
   geometry and rate ceiling. Make the ceiling the widest the backend could ever
   do; a runtime probe narrows it per machine via `BackendLimits::narrowed_to`.
5. Add the spawn/selection path in `hosts/capenc` for the platform it runs on.
6. Add it to the host's candidate ordering so `auto` can select it.

You should not need to touch `parse_ready_v1`, the resolver, or either host's
plan logic. If you do, that is a signal the contract table is missing something —
extend the table rather than adding a special case.

---

## Updating an existing codec dependency

This is deliberately cheap. OpenH264 is the worked example.

The version is pinned in exactly **one** place, the workspace manifest:

```toml
# Cargo.toml
openh264      = { version = "=0.9.7", default-features = false }
openh264-sys2 = { version = "=0.9.7", default-features = false }
```

and the API is called from exactly **one** file,
`src/video/software_h264.rs`. Nothing in `hosts/` or `clients/` touches it.

So an update is:

1. Bump both pins. Keep them equal and keep the `=` — an exact pin is what makes
   the build reproducible across the four machines.
2. `cargo update -p openh264 -p openh264-sys2` and review the lockfile delta.
3. Fix `software_h264.rs` if the wrapper API moved. It is the only caller.
4. Update the entries in `legal/THIRD_PARTY_NOTICES.md`, including the Cisco
   source commit if it changed. The Linux installer embeds that file, so a stale
   notice ships to every host.
5. Record the intake in `legal/ORIGINS.md`.
6. Rebuild on Linux **on the target machine**, not on macOS: the software
   encoder is behind `target_os` gates and a macOS build proves nothing about
   it. Needs a C++ toolchain and NASM.
7. Re-run the session probe against the software path and assert
   `encoder_backend: openh264-sw-h264`.

Steps 4 and 5 are not optional. Arcen statically incorporates the Cisco source,
and BSD-2-Clause requires a binary distribution to reproduce the notice.

---

## What the compiler catches, and what it does not

**Caught.** Adding a `VideoCodec` variant without a token, a bit index, or an
`offered_chroma` row — those are exhaustive matches. Omitting it from
`VideoCodec::ALL` is *not* a compile error, because `ALL` is a hand-written
list; the test `codec_all_lists_every_variant` closes that gap instead, and will
fail until you add it. Adding an `EncoderBackend` variant without a
`ready_token`, an `accelerator_class`, or a `contract`. Every struct literal
that needs the new capability. This is deliberate: the tables are exhaustive
matches so that forgetting one is a build failure, not a runtime surprise.

**Not caught, and therefore your job.**

- **Claiming capability a backend does not have.** The contract is trusted. If
  you say NVENC does AV1 on hardware that does not, sessions fail at READY with
  a capability conflict rather than falling back cleanly.
- **Client decode.** Nothing forces the Deck decoder to keep up with the host.
- **Wire compatibility.** `server_hello` still carries `supports_h264`,
  `supports_h265` and `supports_yuv444` as named booleans for older peers. They
  are derived from the set by accessor, so they stay consistent, but a codec
  outside those three is invisible to a peer that only reads them. If a new
  codec must be negotiable with older clients, that needs a protocol change, not
  just a table row.
- **Geometry and rate ceilings.** These are asserted, not discovered. A ceiling
  that is too generous produces runtime failures the resolver believed were
  impossible.

---

## The rule that matters most

Measure on the target platform. A codec path behind `#[cfg(target_os = ...)]` or
a Cargo feature is not exercised by a build on your laptop. This has bitten this
codebase more than once: a Windows binary shipped a capture-only encoder because
the crate was embedded without its `nvenc` feature, built cleanly, passed its
tests, and failed every session on real hardware.

Build it where it will run, and prove it with a session that reports which
encoder actually served it.
