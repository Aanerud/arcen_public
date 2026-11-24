# ADR 0027: Forward-looking graphics APIs, promoted by measurement

**Status:** Accepted 2026-11-18 for the principle and decision 2. Decision 3
(a D3D12 decode backend) is deferred: the Windows Deck stays on D3D11 for
now. Product-wide principle; first applied to the Windows Deck's decoder. Extends to graphics the forward-only rule that
[ADR 0017](0017-forward-only-pen-input.md) set for pen input.

## Context

Where two platform interfaces can do the same job, Arcen chooses the newer
one. On Windows, D3D11 is the older graphics API and D3D12 the newer.

The Windows Deck decodes with the D3D11 video decoder API (`CreateVideoDecoder`
in `clients/windows/src/decode/session.rs:300`). It decodes on the same D3D11
device as the presenter (`VideoDevice::from_gpu`,
`clients/windows/src/decode/device.rs:253`). On a vGPU, `Present` holds that
device for 15–16 ms, which blocks decode. On a weak GPU this costs frame
rate, for example on an Intel UHD 630 driving two 2560x1600 monitors.

What was known on 2026-11-18:

- **D3D11 with a separate decode device is the common design:** a decode
  device of its own, synchronised with paired fences, with one device as the
  fallback on drivers with known bugs.
- **D3D12 decode is not yet the default anywhere.** Chromium has a D3D12
  decoder but leaves it off by default. FFmpeg's D3D12 HEVC path handles only
  Main and Main 10.
- **No published like-for-like measurement shows D3D12 decoding faster than
  D3D11.** D3D12 does give decode its own `VIDEO_DECODE` queue with no lock
  shared with presentation. Its support has to be probed per profile
  (`D3D12_FEATURE_VIDEO_DECODE_SUPPORT`), not inferred from the GPU
  generation.

## Decision

1. **Principle.** When two platform interfaces can do a job, Arcen builds
   toward the newer one. The older one remains only as a fallback. Which one
   runs on a given machine is decided by a capability probe and a
   measurement, never by the product's age or a GPU name.
2. **Windows Deck decode, now.** Decode gets its own D3D11 device and thread
   on the presenter's adapter. Textures are shared between the two devices
   and ordered by GPU fences in both directions. Each window presents on its
   own thread. Where the driver cannot share, the Deck keeps one device and
   says so in its stats and log. This removes the contention without waiting
   for D3D12.
3. **Windows Deck decode, target (deferred 2026-11-18; D3D11 stays).** A D3D12 video decode backend runs on its
   own queue behind the existing decoder seam (`VideoDecoderBackend`,
   `clients/windows/src/decode/backend.rs`). It is probed per profile and
   becomes the default for a given adapter and driver once it matches or
   beats the D3D11 backend there. The measure is p95 decode-to-display frame
   age and throughput, with Grading 4:4:4 and HDR PQ shown correctly. D3D11
   stays as the automatic fallback.
4. **Not chosen for the Windows Deck:** Media Foundation, vendor SDKs, and
   Vulkan Video. Vulkan Video's decode extensions are absent on the UHD 630
   Windows driver. These stay out unless a probe shows a gap that only
   they fill.

## Consequences

- The Windows Deck's one-device rule (`clients/windows/AGENTS.md`) is
  replaced by step 2.
- Every decode backend must report the same evidence: profile probe,
  presented and unpresented frames, and frame age. That way promoting one is
  a measured result, not an opinion.
- The macOS Deck already uses the forward platform path (VideoToolbox and
  Metal). The principle applies to any future Deck and to Pier encode
  choices too: NVENC, Media Foundation and Vulkan Video on non-NVIDIA hosts.
- Decision 2 shipped in 0.20.3: on a vGPU, two 2560x1600 windows now run at
  60 fps each. The presenters consume only a texture handle, a fence handle
  and a value, so a D3D12 decoder can be added later without changing them.
