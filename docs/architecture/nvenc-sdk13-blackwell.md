# NVENC on Blackwell: 4:2:2, Ultra High Quality, and the SDK 13.0 gate

**Status.** Analysis + contract correction. No Blackwell silicon has been
tested; none is available to the project yet.
**Branch.** `feat/nvenc-blackwell-422-uhq`, off hardware-golden `2c8e5c7`.

---

## 1. The short version

Blackwell (RTX 50-series / GB20x) is the first NVIDIA generation whose encoder
adds capability that matters to graders and VFX artists:

| Capability | Ada and earlier | Blackwell |
| --- | --- | --- |
| HEVC 4:2:2 8-bit and 10-bit | ❌ | ✅ |
| H.264 4:2:2 | ❌ | ✅ |
| H.264 10-bit | ❌ | ✅ |
| Ultra High Quality tuning | ❌ | ✅ |
| **Any 12-bit** | ❌ | **❌ still** |
| **AV1 4:4:4** | ❌ | **❌ still** |

**None of it is reachable from this codebase today, and the blocker is not
hardware — it is our vendored headers.** `hosts/capenc/src/nvenc_sys/` is
generated from **NVENCAPI 12.1**. Every constant the Blackwell features need
arrived in **Video Codec SDK 13.0**. There is no `#ifdef` to flip and no
runtime probe to write: the enum values do not exist in our tree, so the code
cannot name a 4:2:2 surface or the UHQ tuning at all.

Note the two persistent ceilings in the right-hand column. **12-bit is still
impossible on NVENC on Blackwell** — `NV_ENC_BIT_DEPTH` defines 8 and 10 and
nothing else, across every generation. And **AV1 4:4:4 is still not exposed**,
only `NV_ENC_AV1_PROFILE_MAIN_GUID`. Blackwell does not change either
conclusion from the main colour work: 12-bit remains software-only, and
grading at 4:4:4 remains HEVC.

---

## 2. What SDK 13.0 actually adds

Precise names, so the update is mechanical rather than exploratory.

| Symbol | Value | Purpose |
| --- | --- | --- |
| `NV_ENC_BUFFER_FORMAT_NV16` | `0x40000001` | 8-bit 4:2:2, semi-planar |
| `NV_ENC_BUFFER_FORMAT_P210` | `0x40000002` | 10-bit 4:2:2, semi-planar, **MSB-aligned in 16-bit words** |
| `NV_ENC_CAPS_SUPPORT_YUV422_ENCODE` | cap enum entry | Runtime 4:2:2 query |
| `NV_ENC_TUNING_INFO_ULTRA_HIGH_QUALITY` | tuning enum entry | UHQ |

Two details that will otherwise cost an afternoon each:

- **P210 is MSB-aligned**, exactly like the `YUV444_10BIT` path already proven
  in the colour work. 10-bit white is `0xFFC0`, not `0x03FF`. `ColorTransform::pack_p16`
  in `shared/media/src/video/convert.rs` already does this correctly and should
  be reused rather than reimplemented.
- **`chromaFormatIDC = 2` for 4:2:2 is unverified.** The 12.1 header documents
  only `1` (4:2:0) and `3` (4:4:4). `2` is the H.264/HEVC spec value for 4:2:2
  and is the obvious candidate, but it must be confirmed against the SDK 13
  header and a trial `NvEncInitializeEncoder` — do not assume it.

Also worth folding in during the same update: `inputBitDepth` / `outputBitDepth`
on `NV_ENC_CONFIG_HEVC` are SDK **12.2+**. Our working 10-bit implementation
uses the legacy `pixelBitDepthMinus8` because 12.1 is all it has. The modern
fields are clearer and should be adopted once available.

---

## 3. Governance: this is a third-party intake

The current bindings are pinned to an older NVIDIA header snapshot. Updating
them means bringing in a **newer NVIDIA header**, which is a
fresh third-party intake and therefore **Release/Security review**, not
something to do quietly in a feature commit.

The clean, well-precedented route is FFmpeg's **`nv-codec-headers`**
(`ffnvcodec`), which is an **MIT-licensed** redistributable mirror of
`nvEncodeAPI.h` and is how essentially every open-source project consumes
NVENC. That avoids the NVIDIA SDK EULA question entirely.

Required before merging any binding update:

1. Release/Security sign-off on the intake.
2. An entry in `legal/ORIGINS.md`: source repository path, full
   source commit, Arcen destination path, treatment, required notices.
3. MIT notice recorded in the third-party notices file.

**This is the gate. Everything in §4 is downstream of it.**

---

## 4. What this branch changes

Deliberately small, because the interesting work is gated. What is here is the
part that is correct to do *now*.

### 4.1 The contract no longer over-claims 4:2:2

`EncoderBackend::NativeNvenc.contract()` advertised
`ChromaSubsampling::Yuv422`. The encoder has always rejected it — there is a
typed `PixelFormatRejection::Yuv422Unsupported` — but the rejection happens at
**encoder init**, long after the plan resolved and after READY promised the
client 4:2:2.

That is the same class of silent over-claim the whole colour workstream exists
to remove: advertising a capability the build cannot deliver, and discovering
it late. The contract now omits 4:2:2, so a 4:2:2 request is degraded at plan
time with a visible reason instead of failing at init.

A regression test, `nvenc_contract_withholds_422_until_sdk13_bindings_land`,
pins this **together with the reason**. It is written to fail loudly the moment
someone updates the bindings, and its message tells them what to implement and
to invert the assertion. The nudge is the point.

### 4.2 Forward-looking probe rows

The matrix gains `hevc-422-8-full-bt709` and `h264-420-10-full-bt709`,
alongside the existing `hevc-422-10-full-bt709`. All three are expected to
report `unsupported` on current hardware and on Blackwell until the bindings
land. They stay in the matrix because they still exercise the **Deck's decode
side** — VideoToolbox `x422`/`xf22` handling is already implemented and
untested — and because once the bindings land, the answer is one probe run
away rather than a fresh round of test authoring.

---

## 5. The finding worth acting on before any of this

While confirming the tuning enum, a more immediately useful gap turned up.

`hosts/capenc/src/nvenc.rs` hardcodes **both** the preset and the tuning:

```rust
let preset_guid = NV_ENC_PRESET_P4_GUID;              // balanced low-latency
let tuning = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;    // no lookahead, no B-frames
```

That is the correct default for interactive remote desktop, where latency
dominates. **It is the wrong choice for the user this colour work is for.** A
colourist studying a held frame, or a VFX artist checking a matte edge, is not
latency-bound — they are staring at a mostly-static image and want every bit
NVENC can spend on it.

The important part: **`NV_ENC_TUNING_INFO_HIGH_QUALITY` and presets P5–P7
already exist in NVENCAPI 12.1.** They need no binding update, no Blackwell,
and no SDK 13.0. A "grading quality" mode is available on the hardware the
project already has, and it plausibly delivers more visible benefit to the
stated audience than 4:2:2 ever will — 4:2:2 is, after all, strictly *less*
chroma than the 4:4:4 that is already hardware-golden.

Its value is mainly for interchange with 4:2:2-native broadcast and camera
workflows, and for bandwidth: 4:2:2 sits between 4:2:0 and 4:4:4, so it is a
meaningful middle rung when 4:4:4 is too expensive for the link. Useful, but
not the headline.

Plumbing required (not done on this branch — it crosses owners):

`ClientSettings` quality preference → `QualitySettings` on the wire →
host colour/quality policy config → capenc argument → preset/tuning selection
at `nvenc.rs:853`. The Linux host already respawns capenc on a codec/chroma
change, so the respawn model this needs is proven and in place.

**Recommendation: do this before the SDK 13.0 update.** It is unblocked,
cheap, testable on existing hardware, and aimed squarely at the audience the
colour work targets.

---

## 6. Verification

Run on this branch:

```sh
cargo test --locked -p arcen-media
cargo clippy --locked -p arcen-media -- -D warnings
cargo fmt --all --check
```

`arcen-media` is 189 tests here (188 on golden, plus the new contract
regression). The added probe rows are covered by the existing variant
round-trip and id-stability tests.

No hardware validation is possible for §2 until Blackwell silicon is
available. The current test GPU correctly reports `hevc-422-10` as
`unsupported`, and after this change that answer arrives at plan resolution
with a reason rather than at encoder init.

---

## 7. Summary

- Blackwell's encoder additions are real and relevant, but **unreachable
  because our bindings are NVENCAPI 12.1**.
- The update is a **governed third-party intake**; the MIT `nv-codec-headers`
  mirror is the clean route.
- Blackwell does **not** lift the two ceilings that matter most: no 12-bit
  anywhere on NVENC, and no AV1 4:4:4. Grading stays on HEVC 4:4:4; 12-bit
  stays software-only.
- This branch removes a live over-claim, pins the reason with a test that
  nudges the next person, and stages the probe rows.
- **The highest-value next step is not Blackwell at all** — it is a grading
  quality mode using the HIGH_QUALITY tuning and P5–P7 presets that NVENCAPI
  12.1 already exposes and the code currently ignores.
