# Arcen local patch notice

This directory is the exact crates.io `videotoolbox` 0.18.1 source archive,
checksum
`7c2c3d936f5f1bd6f9d706c68149ea5277e11142af9e580eb08607645d635cca`,
published from upstream commit
`59e024ffcbfa73ab54996901faacdb5a31261ae9` of
https://github.com/doom-fish/videotoolbox-rs.

Arcen applies only `arcen-macos12.patch`, so a binary using
`DecompressionSession` still loads on macOS releases older than
`VTDecompressionSessionDecodeFrameWithOptions`:

- `DecompressionSession::decode` and `decode_with_options(None)` call
  `VTDecompressionSessionDecodeFrame`, which VideoToolbox has provided since
  macOS 10.8. Upstream routed both through the newer options entry point. The
  behaviour is the same when no per-frame options are given.
- `decode_with_options(Some(..))` resolves
  `VTDecompressionSessionDecodeFrameWithOptions` at runtime with `dlsym`, and
  returns `unimpErr` (-4) where the running OS lacks it.
- The FFI no longer declares `VTDecompressionSessionDecodeFrameWithOptions` or
  `VTDecompressionSessionDecodeFrameWithOptionsAndOutputHandler`, so neither
  can be strongly linked by accident.

The Arcen patch has SHA-256
`f54c13a96f17f2c4248b61bff4cf15301849678c3e6754a880953bcebddcea4c`.

The source, licenses and authorship records must remain intact. Release/Security
owner approval is required before release. See `legal/ORIGINS.md` and
`legal/THIRD_PARTY_NOTICES.md`.
