# Encode intent: interactive versus grading

**Branch.** `feat/nvenc-grading-quality`, off hardware-golden `2c8e5c7`.
**Status.** Implemented on both hosts and the macOS Deck. Interactive and
Quality paths have been built on their target platforms; P6/HIGH_QUALITY is
hardware-proven on L40S and V100-class NVENC.

---

## 1. The problem

The colour work made Arcen's stream *correct*: 10-bit 4:4:4 full-range HEVC,
measured at exactly zero round-trip error. It did not make the encoder *try
hard*. Both knobs that decide how much effort NVENC spends were hardcoded:

```rust
let preset_guid = NV_ENC_PRESET_P4_GUID;              // balanced
let tuning = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;    // no lookahead, no B-frames
```

That is the right default and it is not a bug. Remote desktop is latency-bound,
and `ULTRA_LOW_LATENCY` exists precisely to strip out the lookahead and
B-frames that add delay.

But it is the wrong setting for the user this colour work was done for. A
colourist studying a held frame, or a VFX artist checking a matte edge, is not
driving the desktop — they are *looking* at it. Every millisecond of latency
they were paying for is worth nothing to them, and every bit of extra image
quality is worth a lot.

Arcen had no way to say that, and the encoder had no way to hear it.

## 2. Why this is worth more than 4:2:2

While investigating Blackwell's 4:2:2 support (see
`nvenc-sdk13-blackwell.md`), the comparison became hard to ignore:

| | Requires | Delivers |
| --- | --- | --- |
| Blackwell 4:2:2 | SDK 13.0 headers + Release/Security intake + new GPU | *Less* chroma than the 4:4:4 already golden |
| Grading intent | Nothing. Already in NVENCAPI 12.1 | More bits spent on the image the grader is judging |

`NV_ENC_TUNING_INFO_HIGH_QUALITY` and presets P5–P7 have been sitting in the
vendored bindings the whole time, unused.

## 3. Design

```rust
pub enum EncodeIntent { #[default] Interactive, Quality }
```

**Intent is deliberately not part of `VideoConfiguration`.** That struct is the
colour contract — it is negotiated, equality-checked, reported in READY and
stamped into every frame header. Intent is none of those things: it does not
change what the decoder must do, and two sessions with identical colour must
still compare equal regardless of how hard the encoder worked. Mixing them
would have broken the "negotiated exactly as requested" check that the colour
work relies on.

So intent travels beside the contract rather than inside it, and it never
enters plan resolution at all.

### What each intent selects

| | Interactive | Quality |
| --- | --- | --- |
| Preset | P4 | **P6** |
| Tuning | `ULTRA_LOW_LATENCY` | **`HIGH_QUALITY`** |
| VBV buffer | 2 frames | **8 frames** |
| Lookahead / B-frames | off | **off — forced, see below** |

### Output ordering is never intent-dependent

**This was got wrong once, in code that looked deliberate, and it produced a
bug that looked like anything but a configuration mistake.** It is worth
stating flatly:

> Both intents run IPPP with no B-frames and no lookahead. `Quality` buys
> better mode decision, finer RDO and a wider VBV. It does **not** buy
> reordering.

The original implementation set P6 + `HIGH_QUALITY` and let the driver fill in
the rest of the preset. For that tuning the driver enables B-frames and
lookahead — entirely correct when encoding a *file*, and a defect when encoding
a *session*:

1. A B-frame cannot be displayed until a later frame has arrived, so coding
   order stops matching display order.
2. Arcen stamps a frame's timestamp when the encoded access unit is read *out
   of the encoder* — coding order. No capture timestamp exists on the wire.
3. The client therefore cannot reorder, and the Deck sets `presentationTimeStamp`
   and `decodeTimeStamp` to that same value.
4. VideoToolbox then released several callbacks at once, and the Deck's drain
   kept only the newest, discarding the rest **without incrementing any
   counter**.

The visible result was grading playback that ran forward, jumped backward, then
ran forward again, with every packet, loss and supersede counter reading zero.

Reordering is also worth nothing here even when it is implemented correctly: a
live desktop always wants the newest frame, so a reorder buffer would trade
latency for compression a session cannot spend.

Both encoder backends now override the driver's defaults immediately after
`NvEncGetEncodePresetConfigEx`, for every intent, and
`EncodeIntent::REQUIRED_FRAME_INTERVAL_P` carries the reasoning at the one
place a future change would have to touch. The Deck counts any frame the drain
discards (`collapsed_decode_callbacks`) so the same failure can never again be
invisible.

**Before B-frames could ever be enabled**, the protocol must carry a real
capture timestamp and the client must reorder against it. That is a protocol
change, not a tuning change.

P6 rather than P7 on purpose: P7 is only marginally better than P6 and much
slower, and a grading session is still a *live* session, not an offline export.

The VBV change is not decoration. The existing code comment already recorded
that "a large VBV buffer is exactly what trades added latency for more
smoothing" — which is the reason it was kept at 2 frames. Quality wants exactly
that trade, so it gets 8. Without widening the buffer, `HIGH_QUALITY` would
still be clipped to a tight per-frame budget and much of the benefit would be
thrown away.

Bitrate is untouched by intent, and there is a test asserting that: how many
bits per second the format needs is a function of resolution, chroma and depth,
not of effort.

## 4. Path

```
Streaming preset -> quality_settings.encode_intent ("interactive"/"quality")
          ->  host resolves, spawns capenc with  intent=<token>
          ->  requested_intent()  ->  preset + tuning + VBV
```

The `intent=` argv token follows the established `key=value` style of
`variant=`, `encoder=` and `cursor=`. It is emitted **only when non-default**,
so every existing interactive session's argv is byte-identical to before.

Production Deck exposes no separate intent control. Auto and Speed derive
`Interactive`; Grading and HDR derive `Quality`. The developer-only Full Colour
contract remains `Interactive`. Legacy `settings.json` values are accepted for
migration but ignored. The derived token is threaded into
`ConnectOptions.profile.encode_intent` and copied into the request by
`rust_viewer_quality_settings`. Engineering CLI smoke tooling retains
`--encode-intent interactive|quality`.

An absent token means `Interactive`. An *unknown* token is a hard error, not a
fallback — deliberately. A session that asked for grading quality and silently
received the interactive encoder would look like a codec regression and be
debugged as one, which is the same class of silent degradation the colour work
exists to remove.

### The middle arrow is not the same on both hosts

That is a limitation of where the two hosts already stood, not a design choice.

**Linux** resolves the token in `net::server` beside the colour axes and adds
it to the same respawn that already honours a codec/chroma/colour request:
capenc is spawned before `server_hello` so the hello can tell the truth, the
client's `quality_settings` arrives after it, and a difference costs one
respawn. Intent had to join the *trigger* for that respawn, not just the
config: a grading client usually asks for exactly the colour contract the host
already resolved, so an intent-only request is the common case, and without
that term it would have been the one case that changed nothing.

**Windows** receives the same authenticated initial-video request before
display mutation or agent launch. The resolved request is carried through the
broker/agent IPC and both single- and multi-monitor capenc configurations, so
the first encoder is created with the derived intent.

Mid-session `quality_settings` changes intent on neither host, matching the
existing mid-session codec/colour behaviour. Both hosts log the requested
token; neither can diff it against the active session, because
`ResolvedMediaPlan` deliberately carries the format the encoder announces and
intent is not part of that contract.

An unrecognised authenticated `encode_intent` token rejects the initial video
request. A host must never silently turn a requested grading session into an
interactive one. Capenc likewise rejects an unknown argv token.

## 5. Tests

| Test | Asserts |
| --- | --- |
| `every_intent_round_trips_through_its_token` | Wire tokens survive the round trip |
| `default_intent_is_interactive` | The default cannot drift into buying latency |
| `intent_parses_defaults_to_interactive_and_refuses_to_guess` | Absent ⇒ interactive; unknown and repeated ⇒ error |
| `quality_intent_widens_the_vbv_buffer_without_moving_bitrate` | Intent buys buffer, not bitrate |
| `interactive_vbv_stays_well_under_a_second_of_bits` | The latency buffer stays a latency buffer |
| `argv_emits_the_intent_token_only_when_it_is_not_the_default` (Linux host) | Default intent leaves the shipped argv byte-identical |
| `build_args_emits_the_intent_token_only_when_it_is_not_the_default` (Windows host) | Same, for the Windows argv builder |

Counts on this branch: `arcen-media` 190 (golden 188), `arcen-capenc` 177 with
`nvenc,mf` (golden 174). `cargo clippy -p arcen-media -p arcen-protocol -D warnings`
is clean.

## 6. Validation status

The active Linux and Windows host crates and the macOS Deck are built and
tested on their target platforms. Grading Reference has encoded live
HEVC 4:4:4 10-bit sessions with P6/HIGH_QUALITY on L40S and V100-class GPUs.
Current multi-monitor clients carry the derived Quality intent into every
pipeline before spawn; legacy late changes are rejected visibly rather than
being claimed as applied.

### What the test machine should check

**Test this on the Linux host.** That is not a preference — Windows resolves
and reports intent but cannot yet apply it (§4), so a Windows session will
always encode interactive no matter what the Deck asks for. The colour work
was proven on a Windows NVENC host, so the natural instinct is to retest here;
for this feature that instinct produces a false negative.

1. It builds — on Linux, on Windows, and on macOS.
2. A session with intent `quality` actually initialises. **P6 + HIGH_QUALITY is
   a combination that has never been trialled in this codebase**, and NVENC
   caps are independent booleans: only a real `NvEncInitializeEncoder` proves a
   combination works. Confirm it does not fall back or fail, especially at
   4:4:4 10-bit.
3. **Measure both halves of the trade**, because both are the point:
   - Latency: interactive vs quality, same content. Expect quality to be
     worse; find out by how much and whether it is tolerable for a grader.
   - Image: at a fixed bitrate on hard content (film grain, fine text, a
     gradient), does quality visibly improve? The round-trip error metric in
     the probe matrix will read 0 for both on synthetic patterns — that metric
     answers colour correctness, not compression quality, so judge this one on
     real content and real bitrates.
4. Whether P7 is worth offering as a third option, or whether it is as slow as
   expected for no visible gain.

If the latency cost turns out to be small, the more interesting question opens
up: whether `Quality` should engage automatically when the desktop is static,
rather than being a mode the user has to remember to select.
