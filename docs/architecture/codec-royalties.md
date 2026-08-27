# Codec royalties and AV1: what the public record actually says

**Status: research finding, 2026-08-15 (revised same day). Engineering
document.** It records publicly published facts with citations so that
engineering and commercial decisions start from evidence. It draws **no legal
conclusions**, and nothing here is advice. Any actual licensing position is for
counsel to determine.

This was written because the question was asked directly: *can Arcen get away
from codec royalties by moving to AV1?*

**The first version of this document answered "less cleanly than the popular
framing suggests". That answer was wrong, and section 1 has been rewritten.**
It noted that the Sisvel AV1 pool exists but did not check *whom it charges*.
Sisvel's published terms charge **consumer device manufacturers**; they contain
no software, service or streaming category. The AVC and HEVC programmes, by
contrast, both expressly license software, and AVC additionally licenses
cloud-gaming streaming. For a company that ships software and no hardware, that
asymmetry runs in AV1's favour, not against it.

---

## 1. Sisvel's AV1 pool charges **device makers**, not software or streaming

*(This section was rewritten on 2026-08-15 after the first version overstated
the risk. The correction matters, so the error is left visible rather than
quietly patched.)*

The AOMedia Patent License 1.0 is bounded: §2.9 defines a Licensor as a party
that itself distributes an AV1 implementation, or one with an obligation
"*as a result of its membership and/or participation in the Alliance for Open
Media working group*" ([text](https://aomedia.org/license/patent-license/)). It
does not bind third-party patent holders, and **Sisvel operates a
royalty-bearing AV1 pool on exactly that theory**
([programme](https://www.sisvel.com/licensing-programmes/audio-and-video-coding-decoding/video-coding-platform-av1/)).

But **what** that pool charges for is the decisive detail, and it is narrow:

> "*The licence offered covers any **consumer product** making use of the AV1
> specification.*"
> "*The royalty rates are offered **on a per product basis, applicable for
> consumer devices** making use of the AV1 specification.*"
> "*The licence offered by Sisvel **does not cover components or
> subassemblies** such as, without limitation, chipsets, semiconductor
> components or embedded modules.*"

Both rate categories are defined as "*any consumer product*", and every
enumerated example is a **physical device**:

| Category | Rate (std / compliant) | Sisvel's examples |
| --- | --- | --- |
| Consumer Display Device | EUR 0.32 / 0.24 | smartphones, tablets, notebooks, computers, convertibles, TVs, projectors, cameras with display |
| Consumer Non-Display Device | EUR 0.11 / 0.08 | set-top boxes, gaming consoles, VR/AR devices, dongles, decoders, players, home theatres, streaming media players, desktop PCs without display, **graphics cards** |

**There is no software category. There is no service, cloud or streaming
category. There is no per-stream, per-title or per-content fee anywhere in the
published terms.**

### Why that matters enormously for Arcen

Arcen ships **software** to **enterprises**. It manufactures no devices. Under
Sisvel's published terms the licensable unit is the finished consumer device —
the customer's PC, laptop, Mac or graphics card, made by Dell, Apple, NVIDIA
and so on. Arcen is not that party.

Now compare what the AVC and HEVC programmes charge for:

| Programme | Software licensed? | Streaming / cloud service licensed? |
| --- | --- | --- |
| **AVC** (Via LA) | **Yes** — "*media player and other personal computer **software***" is an enumerated covered product | **Yes** — a whole separate Section 2, with annual fees by service tier explicitly including **Cloud Gaming** |
| **HEVC** (Access Advance) | **Yes** — "**HEVC Software**" is a named rate category, and the caps *exclude* certain HEVC software | **Yes** — licenses encoders/decoders "*whether made available in devices, **software**, or through **Cloud Based Services***" |
| **AV1** (Sisvel) | **Not in the published terms** — consumer devices only | **Not in the published terms** — no service or content tier exists |

So the asymmetry runs the **opposite way** to the popular framing. For a
software vendor that ships no hardware:

- AVC and HEVC both name software as a licensable unit, and AVC additionally
  has a cloud-gaming streaming tier.
- Sisvel's AV1 terms, as published, reach the device maker instead.

**Streaming a desktop — including for gaming — is not a royalty base in
Sisvel's AV1 programme.** It plainly is one in Via LA's AVC programme.

### What still deserves caution

1. Sisvel is **one** pool. Another AV1 holder could assert on a different
   theory, and no pool guarantees completeness of coverage.
2. The published page is a **summary**; Sisvel links an executed
   `AV1_Sublicense_Agreement`, which is the definitive text. It could not be
   parsed in this session (binary PDF), so the operative definitions of
   "consumer product" and "Licensed Product" are **unread**.
3. Arcen sells to enterprises, not consumers. If anything that sits further
   outside "consumer product", but the agreement's own definition governs.
4. None of this is legal advice, and the conclusion is counsel's to draw.


---

## 2. Meanwhile, HEVC just got simpler, not more fragmented

The usual argument against HEVC is licensing fragmentation. That argument
weakened materially on **15 December 2025**, when Access Advance acquired Via
Licensing Alliance's HEVC/VVC programme
([announcement](https://accessadvance.com/2025/12/15/access-advance-and-via-licensing-alliance-announce-hevc-vvc-program-acquisition/)).
Access Advance describes the result as "*a one-stop shop for those seeking to
license HEVC and VVC technologies for virtually all implementations*", removing
"*a layer of complexity*".

Historic fragmentation was real — Access Advance still publishes a **Duplicate
Royalty Policy** precisely because licensees could owe two pools for the same
product. But an argument built on 2020's licensing landscape should not be
carried into 2026 unexamined.

---

## 3. Published rate structures, for scale

**AVC / H.264** — Via LA
([fee schedule](https://www.via-la.com/licensing-programs/avc-h-264/)):

- First **100,000 units/year: $0.00**
- 100,001–5,000,000: $0.20/unit; beyond: $0.10/unit
- **Enterprise cap: $9.75M/year** (2017 onward)
- Software is expressly enumerated as a covered product category

**HEVC** — Via LA programme (now Access Advance-owned)
([schedule](https://www.via-la.com/licensing-programs/hevc-vvc/)):

- First **100,000 units/year: $0.00**
- Beyond: $0.30/unit (Region 1) / $0.20 (Region 2)
- **Enterprise cap: $30M/year**

**HEVC** — Access Advance
([terms](https://accessadvance.com/hevc-advance-patent-pool-general-pool-terms/)):
"HEVC Software" is an explicit rate category; Region 2 gets 50% off; there is a
$25,000 annual enterprise credit and a 20% ceiling on renewal-term increases.
Two details matter disproportionately:

- the per-category and Single Enterprise caps **explicitly exclude "certain
  HEVC Software"**, and
- if a licensee is not "In-Compliance", **the caps do not apply at all**.

The **numeric** Access Advance per-unit rates are published only as PNG images
and could not be read, so the actual dollar figure for HEVC Software is
unknown from public sources.

Both pools state plainly that they cannot guarantee completeness. Via LA: "*no
assurance is or can be made that the License includes every essential
patent*". Access Advance "*reserves the right to seek licenses from any party
infringing HEVC Standard Essential Patents of our Licensors*". Neither pool
claims to make a licensee fully covered.

---

## 4. The decisive engineering constraint: AV1 4:4:4 has no hardware

This is the finding that most directly bounds Arcen, and it is not a licensing
question at all.

The AV1 specification puts 4:4:4 in **High** profile, and 12-bit and 4:2:2 in
**Professional**
([Annex A](https://github.com/AOMediaCodec/av1-spec/blob/master/annex.a.levels.md)):

> *"The Main profile supports YUV 4:2:0 or monochrome bitstreams with bit depth
> equal to 8 or 10. The High profile further adds support for 4:4:4 bitstreams
> with the same bit depth constraints. Finally, the Professional profile
> extends support over the High profile to also bitstreams with bit depth equal
> to 12, and also adds support for the 4:2:2 video format."*

NVIDIA's published support matrix has, on **every** table — GeForce, RTX Pro,
datacenter, DGX, Jetson — an AV1 column headed exactly **"AV1 YUV 4:2:0"**.
There is no AV1 4:2:2, 4:4:4 or lossless column anywhere on that page, whereas
H.264 and HEVC each have explicit 4:2:2, 4:4:4 and lossless columns
([matrix](https://developer.nvidia.com/video-encode-decode-support-matrix)).
The SDK 13.0 guide confirms Ada onward supports "*AV1 main profile with 8 or
10-bit input precision*" via `NV_ENC_AV1_PROFILE_MAIN_GUID`.

**No publicly documented hardware AV1 4:4:4 encoder or decoder was found
anywhere.** (Surveyed: NVIDIA and Apple. AMD, Intel, Qualcomm and others were
not checked, so this is a negative finding rather than proof of absence.)

### Consequence for Arcen

Arcen's grading tier is **defined** by 4:4:4 — it is the single property that
matters most to colourists, ahead of bit depth and range. Therefore:

- **The grading tier can never move to AV1** on current or announced hardware.
  It stays on HEVC 4:4:4, and its royalty exposure is unchanged.
- Only the **4:2:0 tier** could move, and only where both ends have hardware:
  NVENC from Ada onward, Apple silicon from M3 onward.

So the maximum realistic outcome is a *split*: mainline 4:2:0 sessions on AV1,
grading sessions on HEVC. That is a narrower prize than "get off royalties",
and it comes with the caveat in §1 that AV1 is not demonstrably free anyway.

---

## 5. The compression argument is not evidenced for our workload

The widely repeated "AV1 is ~30% better" figure is, in the primary source,
**AV1 versus VP9** — from AOM's own authors
([Han et al.](https://arxiv.org/abs/2008.06091)) — not versus HEVC. No clean,
primary-sourced AV1-vs-HEVC BD-rate figure was obtained.

More importantly, **nothing found evaluates AV1 against HEVC on screen
content**, which is Arcen's actual workload. Camera-video benchmarks do not
transfer: desktop content is dominated by text edges, flat regions and abrupt
cuts, and screen-content coding tools are in AV1's baseline whereas HEVC keeps
them in a separate SCC extension
([survey](https://arxiv.org/abs/2011.14068)). One study of 360° content even
found the ranking **inverts** between hardware and software implementations
([Sharma et al.](https://arxiv.org/abs/2311.00082)).

A useful licensing detail here: Access Advance's HEVC licence covers "*[a]ll
profiles in Versions 1-10 + Optional Features… in a single per-device
royalty*", so using HEVC's SCC extension does not increase the HEVC royalty.

**If a bandwidth case for AV1 is going to be made for Arcen, it has to be
measured on Arcen's own content.** That is exactly what the probe matrix and
round-trip harness are for.

---

## 6. What this means for the engineering work

The AV1 encode/decode work on `feat/av1-royalty-free-tier` is still worth
having, but **its justification should be restated**:

| Original rationale | Status after research |
| --- | --- |
| "AV1 is royalty-free, so we escape pools" | **Stronger than first assessed.** Sisvel's AV1 pool charges *consumer device makers*; its published terms contain no software, service or streaming category. AVC and HEVC both expressly license software, and AVC additionally licenses cloud-gaming streaming |
| "HEVC licensing is fragmented" | **Weakened.** Consolidated into one administrator in Dec 2025 — but it still names HEVC Software as a rate category, and its caps exclude certain software |
| "AV1 compresses ~30% better than HEVC" | **Unevidenced** for HEVC, and entirely unevidenced for screen content |
| "AV1 could serve the grading tier" | **False.** AV1 4:4:4 needs High profile; no hardware implements it |
| "AV1 gives codec optionality and a hardware-accelerated 4:2:0 path" | **True** |

The corrected picture is that AV1 is a **materially better royalty position for
a software vendor specifically** — precisely because Arcen ships no devices,
and the AV1 pool charges devices while the AVC and HEVC pools charge software
and cloud services.

That is a stronger case than "optionality", and it was missed on the first pass
by reading *that* a pool exists without reading *whom it charges*. The lesson
generalises: a licensing programme's royalty **base** matters as much as its
rate, and a product's shape determines which programmes reach it at all.

Two constraints still bound the outcome and are not affected by the above:

- **Grading cannot move.** AV1 4:4:4 requires High profile and no vendor
  documents hardware for it, so the 4:4:4 tier stays on HEVC regardless of
  licensing.
- **The compression case is unmeasured** for screen content.

The AV1 4:2:0 product path and auth-time host ranking are now wired on both
Piers. The remaining codec-science gate is an equal-quality comparison against
HEVC **on Arcen's own screen content**; the live numbers below prove
interactivity and bandwidth reduction versus the observed H.264 run, not a
matched-quality BD-rate result. Grading stays on HEVC regardless.

### 2026-08-15 engineering evidence

That hardware path is now proven for one representative stack: NVIDIA L40S
(Ada) NVENC produced AV1 Main 4:2:0 8-bit at 3008×1692, and M4 Pro
VideoToolbox hardware-decoded it as full-range `420f`. The same live desktop
and moving YouTube workload measured median bitrate of 25.4 Mb/s for AV1,
53.1 Mb/s for H.264 4:2:0 8-bit, and 20.8 Mb/s for HEVC 4:2:0 10-bit.

Those figures do **not** establish equal perceptual quality: the intervals had
different packet-loss and presentation conditions. They do establish that
hardware AV1 is interactive on Ada→M4 and materially reduces bandwidth versus
the H.264 baseline in this run. They also show that visible jitter is presently
dominated by Deck presentation cadence rather than hardware encode/decode.

Software AV1 remains outside the product claim. The current rav1e path measured
7.23 fps at 1080p and 1.84 fps at 4K for 4:4:4 10-bit on 24 EPYC vCPUs; it is
not wired as the ordinary 4:2:0 fallback, and Deck lacks software AV1 decode.

The resulting runtime policy is measured rather than generation-name based.
On L40S, Performance selected hardware AV1 and Grading Reference selected HEVC
4:4:4 10-bit. On the Windows service pinned to GRID V100D, the same Performance
request emitted a typed AV1 refusal and retried hardware HEVC on that same
approved adapter; Grading Reference still produced full-range `xf44`. Neither
host silently borrowed a different GPU or described a temporary pre-fallback
plan in `ServerHello`.
Virtualized/no-GPU hosts therefore retain the reviewed H.264 software fallback
until both halves meet an interactive target.

---

## 7. What could not be verified

Recorded so that nobody mistakes a gap for a finding:

1. The often-cited MPEG LA 2010 "free internet video" AVC exemption could not
   be retrieved, and **the current Via LA schedule contains no such
   exemption** — free ad-supported streaming is a fee-bearing tier.
2. Access Advance's numeric HEVC per-unit rates are PNG-only and unreadable,
   including the figure for HEVC Software.
3. No enumeration of HEVC patent holders outside all pools was found.
4. No formal AOMedia response to the Sisvel pool was found.
5. No public AV1 patent **litigation** was found — from a non-exhaustive search
   that was not a docket search.
6. **Apple M4 AV1 decode is not confirmed from Apple primary sources.** M3 is
   confirmed. Apple publishes no AV1 profile, chroma or bit-depth detail at
   all, so whether any Apple silicon decodes AV1 High profile is *unpublished*,
   not merely unfound.
7. AMD, Intel, Qualcomm and others were not surveyed for AV1 4:4:4 hardware.
8. No executed licence agreement was reviewed for any programme. Every pool
   page states the agreement itself is the only definitive statement of terms.

Item 6 is directly actionable: Arcen's own Deck capability probe can answer the
Apple half empirically on the next hardware run, which is more than the public
record currently offers.
