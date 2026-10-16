# Arcen local patch notice

This directory is the exact crates.io `apple-cf` 0.9.3 source archive,
checksum
`e7fd680cd0f3f02ee717b2014b26d18985c57b784384ba47213fcf8791e8c250`,
published from upstream commit
`ea8a6c96c10647f46215ac7076295e998f05677d` of
https://github.com/doom-fish/apple-cf-rs.

Arcen applies only `arcen-macos12.patch`, so the macOS Deck can be built with
the Swift 5.7 toolchain and run on macOS 12, the oldest release
`LSMinimumSystemVersion` promises:

- `build.rs` compiles the Swift bridge with `swiftc` directly instead of
  SwiftPM. It targets `MACOSX_DEPLOYMENT_TARGET` (default 12.0) and links the
  SDK's Swift runtime stubs. Upstream required Swift tools 5.9 and macOS 13 for
  the bridge. The Swift target triple follows the architecture Cargo builds
  for, so the bridge also cross-compiles (for example
  `x86_64-apple-macosx12.0` on an Apple-silicon Mac). Source discovery uses
  each directory entry's own file type, which never follows a symbolic link:
  it recurses only into real directories, compiles only regular `.swift`
  files, and refuses a symbolic link anywhere in the bridge sources.
- Three Swift sources replace Swift 5.9 language features and macOS 13 APIs with
  equivalent Swift 5.7 code that also runs on macOS 12: a `switch` expression
  becomes a statement, `asyncAndWait` takes an explicit `DispatchWorkItem`, and
  `UnsafeMutableBufferPointer.update(from:count:)` becomes
  `initialize(from:count:)` on freshly allocated memory.
- `swift-bridge/Package.swift` declares tools 5.7 and macOS 12. This is
  metadata; the build no longer runs SwiftPM.

The Arcen patch has SHA-256
`95b05e3b1bea74f5a775c7e3a520c71bea2833b3c317128d208f7f0bafa29877`.

The source, licenses and authorship records must remain intact. Release/Security
owner approval is required before release. See `legal/ORIGINS.md` and
`legal/THIRD_PARTY_NOTICES.md`.
