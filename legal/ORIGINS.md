# Origins

Arcen is original work by its copyright holder, TUTTIFRUTTI AS. It grew out of
earlier first-party prototype work, also owned by TUTTIFRUTTI AS, which was
relicensed under the GNU Affero General Public License v3.0 when this repository
was opened. There is no third-party copyright in the first-party source tree;
vendored third-party code under `third_party/` and the data listed in
`REUSE.toml` keep their own copyright and licences.

## Inspiration

Arcen was informed by the published behaviour of existing remote-desktop and
pixel-streaming systems — the general shape of the problem, the vocabulary of
the field, and the performance bar those systems set. That influence is at the
level of ideas, protocol concepts, and prior art.

No third-party source code, proprietary SDK payload, decompiled binary,
reference corpus, or vendor implementation was copied, ported, or derived into
this repository.

## Protocol implementations

The USB/IP codec in `shared/usb-bridge/src/usbip.rs`, which the Linux Pier
speaks to the kernel's `vhci-hcd`, was written from the public protocol
description (the Linux kernel's `Documentation/usb/usbip_protocol.rst`). No
kernel or `usbip` tool source was copied.

## Third-party components

Arcen links and bundles third-party open-source components. Each retains its own
licence, and every one of them is recorded with its full notice in
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md). That file is exhaustive by
policy: if a dependency ships in a release artefact, its notice belongs there.

## Contributions

Contributions are accepted under the [Contributor License Agreement](../CLA.md),
which is a licence grant, not a copyright assignment: contributors keep their
copyright and grant TUTTIFRUTTI AS a licence that lets it include their work in
official builds and images. Every accepted contribution stays available to the
public under AGPL-3.0-only with the
[Arcen Store Distribution Exception](../LICENSE-EXCEPTION.md). See
[CONTRIBUTING.md](../CONTRIBUTING.md).
