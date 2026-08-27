# Common Release Contracts

`stage-component.sh` and `promote-component.sh` are fail-closed adapters between
the release workflows and platform-owned packaging hooks. They never fabricate
an installer, signature, or notarization result.

Platform migration work must provide the executable hook named by each adapter.
Build hooks receive `--package`, `--version`, `--commit`, and `--output`.
Signing hooks receive `--input` and `--output`. Production promotion also
requires an executable `packaging/common/sign-manifest.sh` that writes a
non-empty detached signature next to the supplied release manifest.

The promotion adapter verifies staged checksums and records `PROMOTION.json`,
binding the staged checksum manifest to the signed checksum manifest. Signing
hooks run in a sparse checkout without product source and must only sign,
notarize, or wrap the supplied staged files; they must never compile, download,
or substitute product payloads.
