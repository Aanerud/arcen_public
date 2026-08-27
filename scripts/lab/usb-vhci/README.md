# usb-vhci lab dependency

This directory integrates the public SourceForge `usb-vhci` v1.15 source for
the **Hard USB bridge lab only**.

- Upstream: `https://git.code.sf.net/p/usb-vhci/vhci_hcd`
- Commit: `79af411960bf229bd82797f0d42b654da8aae597`
- Tree: `2b2657101bd42ad0f75cd59eec85c09dc997dfa1`
- License: GPL-2.0-or-later at the source-file level (the bundled `COPYING`
  contains GPL-2.0)

No source, patch, binary, or payload is taken from commercial remote-desktop
products or a local reference corpus. The public upstream is fetched directly and the small Arcen
compatibility patch series is applied independently.

`install.sh` builds and installs the two modules through DKMS:

- `usb-vhci-hcd`
- `usb-vhci-iocifc`

The current patch set is validated only on pier-linux.example.internal's
`5.14.0-503.14.1.el9_5.x86_64` kernel. It is not a production support claim.
Run `uninstall.sh` to remove the exact lab module and modules-load entry.

The installer deliberately exposes no USB/IP daemon or TCP listener. Pier's
privileged helper consumes `/dev/usb-vhci`; all remote traffic remains inside
Arcen's authenticated QUIC session.
