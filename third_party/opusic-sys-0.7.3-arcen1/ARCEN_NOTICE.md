# Arcen local patch notice

This directory is the exact crates.io `opusic-sys` 0.7.3 source archive,
checksum
`2804e694ef0de3b4cbb254de565053b7cb48d3398df7fd60c6c62bed40c5372a`,
published from upstream commit
`a9cf9bb34ea9b155cb54095811f5db90bc1a22c4` (tree
`afc1f0465335e02976e84a3acafa8914aedcfae4`).

Arcen applies only `arcen-crt-static.patch`: on MSVC, `build.rs` sets libopus
`OPUS_STATIC_RUNTIME=ON` exactly when Cargo's target features contain
`crt-static`, and sets it `OFF` otherwise. Non-MSVC targets are unchanged.

The bundled libopus source is version 1.6.1 from signed tag object
`a5d6c1b6f4e582df97390f9ac5c6e7c51cbffffe`, commit
`22244de5a79bd1d6d623c32e72bf1954b56235be`, and upstream release tarball
SHA-256
`6ffcb593207be92584df15b32466ed64bbec99109f007c82205f0194572411a1`.
The preserved upstream `opus.patch` has SHA-256
`7f22820055847d438a2bd6df350f8b039cf936291873c36e6c9a7570797b5d5f`.
The Arcen patch has SHA-256
`29af076d6f2b88f63c28c93291ce4afa7d580c0ddaa4709cc6965b3a69767890`.

The source, existing patch, licenses, authorship records, and bundled libopus
tree must remain intact. Release/Security owner approval date: 2026-07-22.
See `legal/ORIGINS.md` and `legal/THIRD_PARTY_NOTICES.md`.
