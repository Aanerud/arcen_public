# Wayland Grading/HDR evidence: Mutter 50.6 and 51.0 in a container

**Status:** host-side capture, import, conversion and encode evidence,
2026-10-18. Not a support claim and not a product qualification. The Pier's
logind session cannot run in a container, so a full `arcen-pier wayland
qualify` and a Pier→Deck run need a GNOME ≥ 50 host (see
[the provider document](../linux-wayland-provider.md#wayland-grading-and-hdr-wp-fidelity)).

## Stack

| | |
| --- | --- |
| Host | Rocky Linux 10.2 lab host, GRID V100D-16Q, driver 580.105.08, `nvidia-drm modeset=1`, SELinux enforcing |
| Containers | podman with NVIDIA CDI. Fedora 44: `mutter-50.6-1.fc44`, `pipewire-1.6.9`. Fedora 45: `mutter-51.0-1.fc45`, `pipewire-1.6.9` |
| Compositor | `gnome-shell --headless` on the NVIDIA render node, one 1920×1080 virtual monitor, `MUTTER_DEBUG_FORCE_HDR=1` (the `force-hdr` route) |
| Content | capenc's `wayland-ramp-client`: a full-screen `XRGB2101010` ramp over a white patch, tagged PQ/BT.2020 or untagged. The runs below predate the channel patch; see the re-proof note |
| Consumer | capenc (`wayland` feature, release), running on the host against the container's PipeWire through a shared runtime directory |

## Results

| Measurement | Mutter 50.6 | Mutter 51.0 |
| --- | --- | --- |
| HDR offer, PQ ramp | `xBGR_210LE` DMA-BUF, 1024/1024 codes, max error 0, import 7.0 ms | same, import 7.8 ms |
| Grading offer, PQ ramp | `RGBA_F16` DMA-BUF, 986/1024, max error 1 | same |
| SDR white (untagged picture) | PQ code 594 = 202.9 nits | same |
| HDR epoch (animated) | admitted `rgb10-direct`; SPS `rext 4:4:4 10-bit bt2020/pq/bt2020ncl full`; 30–31 fps; import 9.9–10.3 ms; stage 3.4 ms; NVENC 5.0 ms | same; import 10.5–10.7 ms |
| Grading epoch (animated) | admitted `f16-pq-to-sdr`; SPS `rext 4:4:4 10-bit bt709/bt709/bt709 full`; 30–31 fps; import 5.8–6.3 ms | same; import 5.7–6.0 ms |

Shared-memory wide formats, measured on 50.6 earlier the same day:

- `xBGR_210LE` carried 256 levels in the wrong channel order.
- `RGBA_F16` carried invalid values.

They are excluded by the per-memory precision proof.

## Product stack for contrast

On Rocky Linux 10.2 / GNOME 49.4 (same host, a development record beside the
installed service), `arcen-pier wayland qualify` records
`compositor_lacks_hdr_screencast`. Auto and Speed qualify as before.

## Re-proof with the channel patch (Mutter 50.6, 2026-10-18)

After the per-format proof and the channel patch were added, both offers
were measured again with the same hardware and container:

- **HDR offer:** `xBGR_210LE` DMA-BUF, 1024/1024 codes, maximum error 0,
  channel patch read back as 900/520/141 (`channels=ok`).
- **Grading offer:** `RGBA_F16` DMA-BUF, 986/1024 codes, maximum error 1,
  `channels=ok`.
- **Grading epoch given only `wide-proven=xbgr2101010@dmabuf`:** it offered
  only `xbgr2101010` and was admitted as `rgb10-pq-to-sdr`. SPS truth:
  `rext 4:4:4 10-bit bt709/bt709/bt709 full`.
- **HDR epoch given both pairs:** admitted as `rgb10-direct`. SPS truth:
  `bt2020/pq/bt2020ncl full`.
