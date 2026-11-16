# Bitstream test vectors

Small Annex B streams for `tests/bitstream_vectors.rs`. Every file is
Arcen's own output: Arcen encoders encoding Arcen's synthetic test pictures.
Recordings that show anything else (an operating system's desktop,
wallpaper or third-party applications) stay out of the repository.

| File | Encoder | Picture | How it was made |
| --- | --- | --- | --- |
| `nvenc-h264-high-640x360.264` | NVENC (Linux X11 Pier, Auto, `--codec h264`) | Arcen's moving test picture (`motion.py`: blocks, ramps, text, frame counter), 40 pictures | Deck CLI `media-smoke` with `ARCEN_MEDIA_DUMP` and `ARCEN_MEDIA_DUMP_AFTER_SECONDS=6` (the recording starts at a keyframe after the test picture covers the desktop) |
| `nvenc-hevc-main-640x360.265` | NVENC (Linux X11 Pier, `--codec h265`) | as above | as above |
| `nvenc-hevc-main10-640x360.265` | NVENC (Linux X11 Pier, `--codec h265 --bit-depth 10`) | as above | as above |
| `nvenc-hevc-rext444-10-640x360.265` | NVENC (Linux X11 Pier, Grading: HEVC Main 4:4:4 10, BT.709 sRGB, full range) | as above | as above |
| `nvenc-hevc-rext444-10-pq-640x360.265` | NVENC (Linux Wayland Pier, HDR: HEVC Main 4:4:4 10, BT.2020 PQ) | Arcen's exact-still-image ramps picture (`wlpat.py ramps`) with its moving block | as above |
| `openh264-160x96.264` | OpenH264 through `arcen_media::video::SoftwareH264Encoder` (Constrained Baseline, POC type 2) | Moving checkerboard and ramps, 24 frames, forced IDR at frame 12 | `cargo run -p arcen-media --features software-h264-source --example software_h264_vector -- openh264-160x96.264 160 96 24` |

Larger lab recordings (NVENC from the Linux Piers, VideoToolbox from the
macOS Pier) are replayed with `ARCEN_DECODER_VECTORS=<dir> cargo test -p
arcen-media --test bitstream_vectors`.
