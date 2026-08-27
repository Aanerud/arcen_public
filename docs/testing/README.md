# Colour probe matrix: running it end to end

Operator runbook for the grader/VFX colour work on
`feat/color-fidelity-10bit-444` (4:4:4 chroma, 10-bit depth, full-range
colour, negotiated end to end). Read
[`../architecture/color-fidelity.md`](../architecture/color-fidelity.md)
first for the design and the rationale — this document only covers how to
run the matrix and record findings, and does not repeat the design doc. Host
config keys (`video.bit_depth`, `video.color_range`, `video.color_matrix`,
`video.variant`, `video.color_policy`) are documented in
[`../operations/pier-administration.md`](../operations/pier-administration.md)
under "Colour fidelity policy" — link there rather than duplicating.

## 1. Why the matrix exists

Arcen's defining method for this work is that uncertainty is resolved by
measurement, not by design. Several questions have no authoritative answer in
any vendor document:

- Does macOS VideoToolbox decode HEVC Rext at 10 bits, and in hardware, or
  only in software (or not at all)?
- Does the full-range VUI flag actually survive host encode → wire → Apple
  decode, or does a decoder quietly assume limited range?
- Does VideoToolbox decode H.264 High 4:4:4 Predictive at all? Apple
  publishes nothing on this.
- Can a GBR identity-matrix stream survive `CoreVideo`, which has no
  identity/GBR matrix constant to describe it with?

The product already ships HEVC 4:4:4 **8-bit** today, which proves Rext
decode works at 8 bits and narrows the real unknown to depth, range and
matrix specifically, not to Rext as a whole.

Each question is coded as a row in `PROBE_MATRIX`
(`shared/media/src/video/variant.rs`, 11 rows, one comment per row explaining
what it answers) and run on real hardware. A row that **fails** is a
recorded finding. A row left **untested** is an assumption, which is exactly
what this matrix exists to eliminate.

## 2. Current state of the loop (verified 2026-08-14)

Both matrix-walking tools are implemented and were run on real hardware at
commit `086edeeed0aac87ca7275892318fa81dac093453`:

- `arcen-capenc probe-matrix` walks all 11 rows, performs real encoder
  initialisation, emits host JSON, and can write one deterministic Annex-B
  round-trip source per encodable row.
- `arcen-deck probe-matrix` consumes those sources, creates real
  VideoToolbox sessions (including a hardware-required attempt), records the
  delivered `CVPixelBuffer` format/extensions, and computes end-to-end pattern
  error.
- `variant=<id>` reaches the real Windows and Linux encoder-construction
  paths. A live Windows session log showed
  `variant=hevc-444-10-full-bt709`, full-range VUI, and READY
  `bit_depth=10 range=full`.
- Both Piers compile and pass their target-platform tests. Shared
  `VideoConfig` now carries `bit_depth`, `color_range`, `color_matrix`,
  `color_policy`, and `variant`; the Windows service-to-session-agent IPC
  preserves the same fields.
- Windows NVENC produces 4:4:4 10-bit full-range and M4 Pro VideoToolbox
  hardware-decodes it as `xf44`. Linux now initialises the same 10-bit row;
  the live Rext `yuv444p10le` full-range stream reaches Deck as `xf44-full`
  and decodes in hardware.
- 4:2:2 remains blocked in Arcen before the Blackwell hardware question:
  the vendored NVENC bindings and BGRA conversion have no NV16/P210 path.
- AV1 rows remain unsupported by the host dispatcher and by Deck's current
  parameter-set harness, despite the standalone `rav1e` wrapper.
- Merging host and Deck JSON remains manual by design; keep local result files outside the published tree unless they have been sanitized for publication.

The former live-only MF blocker is also closed: a Windows Pier software-fallback Media
Foundation H.264 stream reaches Deck as `420v`, decodes in hardware and
displays. The retest additionally caught and fixed zeroed depth/range/matrix
bits in both Piers' per-frame headers.

### Secure smoke credentials

Automated Deck smokes accept `--credentials-stdin`. Standard input must contain
exactly two non-empty UTF-8 lines: username, then password. The flag cannot be
combined with `--username`, `--password`, or `--password-file`. This keeps both
values out of process arguments and evidence transcripts:

```sh
printf '%s\n%s\n' "$ARCEN_TEST_USERNAME" "$ARCEN_TEST_PASSWORD" |
  arcen-deck media-smoke HOST 18444 --credentials-stdin --pin-sha256 FINGERPRINT
```

Do not replace the explicit trust option with
`--insecure-skip-verify` outside an isolated lab.

`media-smoke --video-only` skips only the completion requirement for an audio
packet. Use it when validating video on a deliberately silent source; it does
not disable host audio and must not be used as evidence that audio passed.

## 3. Prerequisites

**Host** (produces the encoded stream):

- Windows or Linux with an NVIDIA GPU. HEVC 4:4:4 10-bit (the target format,
  `hevc-444-10-full-bt709`) is expected to encode from Turing onward.
  `hevc-422-10-full-bt709` needs a Blackwell (RTX 50-series) GPU; expect
  `NvEncInitializeEncoder` to fail below that (§7).
- Build the capture/encode engine with the features that match the codec
  paths you intend to exercise. The real feature names, read from
  `hosts/capenc/Cargo.toml`:
  - `nvenc` — native NVENC (Windows and Linux). Required for every non-AV1
    row.
  - `mf` — standalone Windows Media Foundation comparison path; not compiled
    into the shipped Pier.
  - `software-h264` — shipped portable OpenH264 path (enables
    `arcen-media/software-h264-source` in the shared crate).
  - There is **no** `software-av1`/`software-av1-source` feature on
    `arcen-capenc`. `arcen-media`'s own `software-av1-source` feature (which
    gates the real `rav1e` wrapper) exists but is not exposed through
    capenc, so it cannot currently be built into a host binary (§2).
  - Build the engine directly (works standalone today):
    `cargo build --release -p arcen-capenc --features nvenc,mf` (Windows) or
    `cargo build --release -p arcen-capenc --features nvenc,software-h264`
    (Linux).
  - The Piers (`arcen-pier-windows`, `arcen-pier-linux`) bake `nvenc`
    (+`mf`/`software-h264`) into their own `arcen-capenc` dependency
    unconditionally — no separate feature flag to pass.

**Client** (decodes and reports):

- An Apple Silicon Mac (the Deck client is arm64-only; see
  `pier-administration.md`'s BUILD-376). Recent macOS with VideoToolbox.
  Which macOS versions/chips actually hardware-decode which rows is
  precisely what this matrix measures — do not assume, run it.
- Build with the default-off `dev-tools` feature (`clients/macos/Cargo.toml`)
  to get the `probe-matrix` subcommand and the settings variant picker's
  underlying support:
  `cargo build --release -p arcen-deck-macos --features dev-tools`
  (`rust-version = "1.89"` in that crate's `Cargo.toml`; build on macOS with
  Xcode command line tools for the `objc2`/`videotoolbox`/`core-graphics`
  bindings).

## 4. Running the matrix end to end

1. **Build the host engine** on the NVENC machine, as above.
2. **Run the host matrix**, producing both the JSON report and deterministic
   round-trip files:
   ```
   arcen-capenc probe-matrix --output host-findings.json \
     --roundtrip-pattern grey_ramp --roundtrip-output-dir parameter-sets/
   ```
3. **Copy the captured files to the Mac** into one directory, for example
   `parameter-sets/`, one file per variant id (`hevc-444-10-full-bt709.hevc`,
   `h264-444-8-full-bt709.h264`, ...).
4. **Run the Deck side** against that directory:
   ```
   arcen-deck probe-matrix --parameter-sets parameter-sets/ --output deck-findings.json
   ```
   (macOS, `dev-tools` build; see §5 for what this does and does not cover.)
5. **Merge the two halves by hand** into
   [`color-matrix-results.json`](color-matrix-results.json):
   - Copy `deck-findings.json`'s `environments[0]` block into the tracked
     file's `environments` array (its own `_comment` says to copy one block
     per machine that runs the matrix). Fill in that entry's `host` object
     (`os`, `gpu`, `driver_version`, `nvenc_generation`) by hand — the Deck
     always emits `host: null` there, because it has no way to observe the
     host machine (§2).
   - For each row, copy the Deck's `decode`/`hardware_decode`/
     `delivered_pixel_format`/`color_extensions_attached`/`notes` fields
     into the matching row of a sanitized results file.
   - Copy `encoder_init`/`encoder_error` and measured burst rates from
     `host-findings.json`.
   - Add free-text visual observations; numeric round-trip fields are produced
     automatically when the host metadata and Deck reference pattern agree.
6. **Commit only a sanitized results file back to the branch.** This is the
   whole point of the loop: findings travel with the code that produced
   them, and the next round of work starts from what was actually measured.

## 5. Driving a single row interactively

Two independent surfaces exist. Neither requires the Pier.

### Host: `arcen-capenc variant=<id>`

The engine's full argv contract, from `hosts/capenc/src/lib.rs`:

```
arcen-capenc <output_index> <codec> [fps] [yuv444] [framed-v1] [selftest [WxH]]
```

`<codec>` is exactly `h264` or `h265` (not `hevc`) — it selects which NVENC
GUID gets initialised and must match the row's codec component. `variant=`
and `cursor=<local|host>` may appear anywhere in the argument list; they are
matched by prefix, not by position. `output_index` selects which display
output to capture (`0` = first/primary).

To attempt one row against a synthetic test pattern (no live desktop needed,
useful for a quick pass since it does not depend on what is currently on
screen) and capture it to a file the Deck can read:

```
arcen-capenc 0 h265 30 yuv444 variant=hevc-444-10-full-bt709 selftest 1920x1080 > hevc-444-10-full-bt709.hevc
```

Let it run for a couple of seconds (it loops forever, like the live path,
until interrupted) then stop it (Ctrl+C) once you have several access
units. **Do not add `framed-v1`** to a capture you intend to feed to
`arcen-deck probe-matrix` — that switches stdout to length-prefixed records
for the Pier's internal pipe, not the raw Annex-B the Deck's parser expects.
For an H.264 row, drop `yuv444` when the row's chroma is not 4:4:4 and set
`<codec>` to `h264`, for example:

```
arcen-capenc 0 h264 30 variant=h264-444-8-full-bt709 selftest 1920x1080 > h264-444-8-full-bt709.h264
```

To attempt the row against the real desktop instead of `selftest`, drop the
`selftest 1920x1080` tokens; `output_index` then selects a real attached
output and capture requires an actual desktop session.

`variant=` is authoritative over the legacy codec/chroma tokens and now reaches
the real Windows and Linux encoder constructors. Watch stderr: encoder
construction failure logs a line containing `NVENC init failed: <reason>`
and the process exits with status `4` when `nvenc` is the effective encoder
choice; that exit code and log line together are `encoder_init: failed` plus
`encoder_error` for the results file.

Add `encoder=nvenc` to force NVENC specifically (rejecting a software
fallback) when you want a clean pass/fail signal for one row rather than a
silent fallback to another backend.

### Deck: the settings variant picker

Deck → Settings → Color Fidelity → **"Variant (testing)"** combo box lists
every `PROBE_MATRIX` row (`clients/macos/src/ui/app.rs`) and pins the exact
variant the Deck requests when it next connects, bypassing the preset and
Advanced overrides. This is the live, negotiated path (as opposed to
`probe-matrix`'s offline decode of a pre-captured file). The active host plan
still starts before the initial `quality_settings` exchange, so the client
request alone does not recreate a mismatched encoder: configure the host
`video.variant`/colour policy to the row being tested, restart the service,
and verify the PID changed before trusting a live result.

## 5.5 Measuring colour accuracy end to end

The probe halves can additionally carry a known test pattern through a **real**
encode and decode, so the recorded error is the one a colourist would actually
see rather than a theoretical figure.

Host — encode the pattern per row and write the bitstreams out:

```
capenc probe-matrix --roundtrip-pattern grey_ramp \
                    --roundtrip-output-dir roundtrip/
```

That writes one bitstream per variant id into `roundtrip/`, plus a shared
`roundtrip-meta.json` recording the pattern and geometry. Copy the directory to
the Mac alongside the parameter sets.

Deck — decode and compare against the locally regenerated reference:

```
arcen-deck probe-matrix --parameter-sets parameter-sets/ \
                        --reference-pattern grey_ramp \
                        --output deck-findings.json
```

The pattern is **generated from a pure function** in `arcen-media`, so the Deck
reproduces the reference pixels exactly without the host transferring an image.
If the `--reference-pattern` flag disagrees with `roundtrip-meta.json`, the
measurement is disabled with an explicit note rather than reporting a
meaningless number.

Valid pattern tokens: `grey_ramp`, `shadow_highlight_wedge`,
`saturated_primaries`, `chroma_detail`, `full_gamut_noise`. Each targets a
different failure mode — see `TestPattern::exposes()` in
`shared/media/src/test_pattern.rs`. Run more than one: `chroma_detail` is the
only one that meaningfully exercises subsampling, and
`shadow_highlight_wedge` is the only one that exposes range clipping.

### Read the two error figures correctly

`roundtrip_max_error` and `roundtrip_mean_error` in the sanitized results file are
**end-to-end**: they include codec quantisation loss. They are *not* the same
as the pure colour-conversion error asserted in `arcen-media`'s unit tests,
which is exactly **0** for 10-bit full range and never 0 for the 8-bit limited
format the product shipped previously.

So a non-zero end-to-end error at 10-bit full range does **not** contradict the
"numerically lossless" claim — it means the codec quantised, which any lossy
encode does. What would contradict the claim is the *pure* figure regressing,
and that is guarded by CI rather than by a hardware run.

The mean is reported alongside the max because a mean hides a single wrong
pixel on a matte edge, which is precisely the defect that matters here.

## 6. Filling in a sanitized results file

Every row starts `untested`. Use the file's own `field_reference` block as
the source of truth for the value vocabulary; the summary below just orients
you:

| Field | Meaning |
| --- | --- |
| `encoder_init` | `ok \| failed \| unsupported \| untested`. Whether a real `NvEncInitializeEncoder` (or equivalent) attempt succeeded for this exact combination. |
| `encoder_error` | The exact error string when `encoder_init` is `failed`, else `null`. |
| `decode` | `ok \| failed \| unsupported \| untested`. Whether a real `VTDecompressionSession` accepted and decoded the stream. |
| `hardware_decode` | `true \| false \| null`, from `kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder`. |
| `delivered_pixel_format` | The `CVPixelBuffer` FourCC actually delivered, e.g. `xf44`. |
| `color_extensions_attached` | What `CMFormatDescriptionGetExtensions()` reported, so inferred-vs-set colour can be compared. |
| `roundtrip_max_error` | Per-channel max error of the injected deterministic test pattern, 0-255 scale; 0 means exact. Deck computes it automatically when `--reference-pattern` agrees with `roundtrip-meta.json` and the decoded geometry. |
| `sustained_fps` / `bitrate_mbps` | Measured, not requested. When `arcen-deck probe-matrix` reports these, they measure feeding a static file's access units back-to-back as fast as possible, not real-time playback — the tool's own `notes` says so on every row where it reports a rate. |
| `notes` | Free text. |

Two rules matter more than the mechanics:

- **A row that FAILS is a finding to record, not an error to hide.** Leave
  it in the file with `encoder_init`/`decode` set to `failed` and the exact
  error text. Deleting or skipping a failing row throws away the answer to
  the question that row existed to ask.
- **A row left `untested` is an assumption.** That is exactly what this
  matrix exists to eliminate, so an `untested` row after a matrix run is
  itself worth a note explaining why (no hardware for it, blocked on a
  feature, ran out of time).

Free-text observations — banding, crushed blacks, a colour cast, chroma
fringing on text — are as valuable as the numeric fields. A colourist's eye
catches a one-code drift a numeric round-trip check might miss if the
comparison methodology is not exact.

## 7. What to expect

So a real failure is distinguishable from an already-known one:

- `hevc-422-10-full-bt709` needs a Blackwell (RTX 50-series) GPU. Expect
  `encoder_init: failed` on Turing/Ampere/Ada — that is the row doing its
  job, not a bug.
- The two AV1 rows need `rav1e` wired into a host binary, which is not the
  case anywhere yet (§2/§3): expect `untested` until that lands. Once it
  does, do not expect interactive performance from them: `rav1e` measures
  **~3.1 fps at 1080p and ~0.67 fps at 4K** (4:4:4 10-bit, fastest preset,
  low latency, 6 threads — see `color-fidelity.md`'s "The software tier is
  not interactive"). These two rows are correctness checks, never
  performance ones.
- `h264-444-8-full-bt709` is **expected** to fail on the client — Apple
  documents nothing about whether VideoToolbox decodes H.264 High 4:4:4
  Predictive at all. A `decode: failed` result here is the finding the row
  exists to produce, not a bug to chase.
- `hevc-444-12-*` is deliberately **not a row** in `PROBE_MATRIX`. It is
  rejected as incoherent (`VideoVariant::from_id` returns
  `VariantIdError::Incoherent`) because no encoder Arcen has can produce
  12-bit HEVC — NVENC's `NV_ENC_BIT_DEPTH` defines only 8 and 10 at any
  chroma subsampling. Do not add it back expecting it to work; 12-bit exists
  only through the AV1/`rav1e` software tier.
- `hevc-444-10-full-identity` (the GBR/identity row): the more likely
  outcome is that it decodes but with **wrong colour**, since `CoreVideo`
  has no identity matrix constant to describe it with truthfully. Wrong
  colour here is itself the finding — record exactly what colour it
  produced instead of only pass/fail.

## 8. Known constraints that will otherwise look like bugs

- **NVENC has no 12-bit mode at any chroma subsampling.** This is a hardware
  ceiling (`NV_ENC_BIT_DEPTH` defines only 8 and 10), not a missing code
  path — 12-bit is AV1/`rav1e`-only, everywhere in the product.
- **NVENC does not expose AV1 4:4:4.** It advertises only
  `NV_ENC_AV1_PROFILE_MAIN_GUID`, which is 4:2:0. AV1 4:4:4 is software-only
  for the same hardware-ceiling reason.
- **`CoreVideo` has no identity/GBR matrix constant.** There is no Apple API
  call that can correctly describe an identity-matrix stream; the identity
  row exists specifically to measure what happens anyway, not because a
  correct path is expected to exist.
