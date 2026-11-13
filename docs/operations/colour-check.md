# Colour check

The Deck's **Colour check** answers, item by item, the question a colourist
asks before judging a picture remotely: *is what I see what the host has?* It
reads the host's own truth, this Mac's display and presentation state, and —
on demand — measures a known test pattern code value for code value. Every
item has a verdict, a stable code (listed in
[Error codes › Colour check](error-codes.md#colour-check)), the facts behind
it and, where the Deck can act, a one-click fix.

The Deck never changes a host setting. Where the host needs something —
a depth-30 Xorg desktop, Mutter 50, a proven Windows HDR display — the check
names it and links the page an administrator needs.

## Opening it

- **Session › Colour Check…** in the menu bar, during a session.
- **Settings › Streaming › Colour check…**, at any time. Without a session it
  checks this Mac only.

**Copy report** puts the whole checklist on the clipboard as text. **Save
report** writes it to `~/Documents/Arcen/Colour Checks/` as
`arcen-colour-check-YYYYMMDD-HHMMSS.txt` (UTC), ready to attach to a review.
The report names the Deck build, the host (name, platform, Pier version) and
the time, and repeats every item with its code, facts and fixes.

## Verdicts

| Mark | Verdict | Meaning |
| --- | --- | --- |
| ✓ | PASS | Right for colour-critical work. |
| ℹ | INFO | A fact worth knowing; nothing is wrong. |
| ⚠ | WARN | Usable, with a caveat a colourist must know. |
| ✗ | FAIL | What you see is not what the host has. |

The summary line takes the worst PASS/WARN/FAIL verdict; INFO never lowers it.

## What it checks

### What the host is serving

Read from the host's hello (`active_*` colour fields, codec, encoder class),
its `served_pipeline` message (the pipeline it actually serves and, for a
downgrade, its reason and advice), and the headers of the frames the Deck
decoded — compared with what this Deck asked for:

- **Preset** — requested Auto/Speed/Grading/HDR against served, with the
  host's reason when it served less.
- **Codec and profile** — for example *HEVC Main 4:4:4 10 (range
  extensions), 10-bit, 4:4:4*.
- **Chroma**, **Bit depth**, **Transfer and primaries**, **Range and matrix**
  — each requested against served. Grading is ten-bit 4:4:4 full-range
  BT.709 **SDR**; only a PQ transfer is HDR.
- **Frame headers** — the decoded frames' own codec, chroma, depth, range and
  matrix agree with the hello. A disagreement is a defect: send the report
  with the Pier's log.
- **Capture source** — the host does not report it, so the check states the
  source the served pipeline uses on that platform and labels it *implied*.
- **Encoder** — hardware or software.

### What this Mac is doing

- **Display** — macOS's reference-rendering headroom
  (`NSScreen.maximumReferenceExtendedDynamicRangeColorComponentValue`). It is
  non-zero only while a reference preset is active (System Settings ›
  Displays › Preset on a Liquid Retina XDR or Pro Display XDR); otherwise
  brightness, True Tone and ambient adaptation change what you see.
- **Colour profile** — the display's profile name and the colour space the
  Deck tags the picture with (read back from the presentation layer). macOS
  converts the tagged picture into the profile: colours match by appearance,
  and the codes reaching the panel equal the stream's only when the profile is
  the tag.
- **HDR** — current and potential EDR headroom. A PQ stream on a display with
  no headroom fails: highlights clip at SDR white.
- **Presentation** — a ten-bit stream must be on the dedicated ten-bit Metal
  layer; the eight-bit fallback fails, with its reason.
- **EDR** — read back from the layer: on exactly when the stream is PQ.
- **Scale** — stream pixels against the backing pixels they occupy on screen,
  to within one pixel over the whole picture. 1:1 passes; a whole-number
  enlargement (Retina-resolution streaming off) is a caveat; anything else
  fails, because resampling blends neighbouring pixels — including a host
  desktop two rows taller than the screen, which the Deck fits 0.1% smaller
  (found on a Windows host: 3600×2262 shown at 3596×2260 on a notched Mac;
  the Deck now requests the exact fullscreen viewport).

### Exactness

1. Choose a pattern: `grey_ramp` (banding and luma scaling),
   `shadow_highlight_wedge` (range clipping), `saturated_primaries` (matrix
   error), `chroma_detail` (subsampling) or `full_gamut_noise`.
2. **Save pattern…** writes it as a PNG at this stream's exact size, with no
   colour profile, to `~/Documents/Arcen/Colour Checks/`.
3. Show that PNG full-screen on the host at 100%: one image pixel per screen
   pixel, no border, no viewer scaling, no colour management. A browser in
   kiosk mode works; size the image by `devicePixelRatio` on a scaled
   desktop.
4. Press **Measure**. The Deck reads the frame its presentation layer shows
   (a still screen sends no new frames) straight from the decoder's planes — before any display conversion or scaling on this Mac —
   and compares every pixel with the pattern, in two ways:
   - **RGB as the desktop drew it**: the decoded Y′CbCr turned back into
     eight-bit RGB with the stream's own matrix and range.
   - **Coded Y′CbCr samples** (4:4:4 only): the decoded codes against the codes
     the pattern should have at the stream's depth, for example `max 1/1023`.

   Both report the maximum and mean error, the worst pixel's position, and how
   many pixels are more than two codes off and the rectangle that holds them.
   **Exact** means zero everywhere; **near** means within two eight-bit codes
   (a lossy codec's quantisation); anything more fails, with two named
   exceptions:
   - **Re-encoded as BT.709** (`colour.exact.bt709_reencoded`): the picture
     matches the desktop's sRGB values decoded to light and encoded again with
     the BT.709 transfer. That is what a Windows Pier before 0.18.0 serves for
     a Grading stream (it captures linear light, FP16 scRGB): code values are
     not the desktop's (up to 16/255 in the shadows), and shown as SDR the dark
     tones come out darker. A 0.18.0 Windows Pier serves a 0.18.0 Deck the
     desktop's own codes with the sRGB transfer (`srgb`), which the
     **Transfer and primaries** item accepts as the BT.709 request kept;
     from 0.19.0 Linux (Xorg and Wayland) and macOS Piers state `srgb` the
     same way. Every
     measurement is also compared this way, so the report says which of the
     two arrived.
   - **Localised** (`colour.exact.localised`): within two codes except a few
     pixels or one small area — something on the host is drawn over the
     pattern there, such as a notification or Windows' activation watermark.

HDR streams are not compared code for code: HDR maps the SDR desktop to PQ at
203-nit white by design. Measure in Grading.

A lossy stream is close, not bit-exact. Bit-exact still frames need lossless
refinement ([ADR 0016](../adr/0016-lossless-refinement.md)), which is
proposed and not in this release; the check says so rather than implying it.

### Host prerequisites

When the host served less than asked for, the check names why and links the
page for the host's administrator:

| Host | What Grading and HDR need | Read |
| --- | --- | --- |
| Linux, Xorg | Grading: a depth-30 desktop (`video.x11_desktop_depth`: `auto` starts one for a Grading sign-in, or `30`). HDR: a declared Rec.2100 PQ desktop (`video.desktop_encoding`), otherwise served as Grading. | [Linux desktop depth](pier-administration.md#linux-desktop-depth), [HDR gate](../architecture/color-fidelity.md#linux-depth-30-capture-and-the-hdr-gate) |
| Linux, Wayland | Mutter 50 or later with HDR screen sharing; GNOME 49 serves eight-bit. | [Wayland Grading and HDR](../architecture/linux-wayland-provider.md#wayland-grading-and-hdr) |
| Windows | Grading on any NVENC host. HDR only once the captured display is proven in HDR mode. | [Windows ten-bit and HDR](../architecture/color-fidelity.md#windows-fp16-scrgb-to-ten-bit-sdr-and-hdr10) |
| macOS (preview) | HDR needs the virtual display to report headroom above SDR white. | [Presets and pipelines](../features.md#streaming-presets-and-independent-pipelines) |

## One-click fixes

**Apply** at the top offers the first Deck-side fix of the first failing or
cautioned item; every item also shows its own.

| Fix | What the Deck does |
| --- | --- |
| Switch to Grading / HDR and reconnect | Selects the preset (the same setting as Settings › Streaming), saves it, and reconnects. You sign in again; the remote desktop and its applications stay open, and the host chooses its pipeline for the new connection. HDR is offered only when a display of this Mac can show HDR; on a Grading session against a Windows or macOS host the HDR item offers it as a choice, never as the recommended fix. The panel steps aside for sign-in and comes back with the new session — or, when the host refuses the new connection (for example a host without a ten-bit 4:4:4 hardware encoder refusing Grading), with the host's reason under Session. On a Linux Xorg host a desktop keeps the depth it started with, so a desktop started for Auto or Speed is still served as Auto (`eight_bit_source`) until you sign out of it. |
| Stream at Retina resolution | Turns on Retina-resolution streaming. A window-following stream resizes live on hosts that support it; a pinned stream reconnects. |
| Enter fullscreen | Shows a pinned stream at 1:1. |
| Measure / Save pattern… | See Exactness. |
| Read: … | Opens the linked documentation. |

## Where the rules live

The items, rules, wording and codes are shared code
(`shared/media/src/colour_check.rs`, `arcen_media::colour_check`), unit-tested
on every platform; the pattern measurement is
`arcen_media::test_pattern::measure_biplanar`. The macOS Deck only gathers
what macOS knows (`clients/macos/src/ui/colour_check.rs`) and draws the panel,
so a future Deck on another platform reuses every rule.
