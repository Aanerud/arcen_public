# Clipboard redirection privacy and trust boundary

Clipboard v1 is available only after exact subprotocol-version negotiation on
direct macOS Deck connections to the active Windows and dedicated-Xorg Linux
Piers. The host policy is authoritative in both directions and is rechecked
before offers, allocation growth, native injection, and host emission.

Clipboard payloads are ephemeral. Arcen retains at most one active transfer and
one replaceable pending item per sender, one reassembly, and one native latest
slot. Owned buffers are scrubbed on replacement, abort, expiry, disconnect, and
teardown. Arcen does not log or persist text, pixels, hashes, origin tokens,
paths, or native handles. Logs contain bounded direction/kind/size/reason
metadata only.

The private origin marker prevents loops; it is not a credential or integrity
control. QUIC/TLS 1.3 and authenticated host-session binding remain the security
boundary. Unnegotiated, disabled, old-peer, Linux no-auth/shared-display, and
Deck-local-off states start no OS watcher and send/accept no clipboard payload.

Text is UTF-8. Images are PNG on the wire; DIBV5 and TIFF exist only inside the
Windows and macOS adapters. Files, HTML, RTF, delayed rendering, Wayland, and
Gateway/Span transport are not implemented.

After a remote injection the endpoint OS clipboard may continue owning the
latest item after the Arcen connection ends. Windows stores the eager native
payload, macOS pasteboard retains its eager item, and the Linux child releases
its X11 selection on disconnect (so its remotely injected item becomes
unavailable). Users must overwrite or clear sensitive clipboard content when
residual endpoint ownership is unacceptable.

This implementation and its dependency/notice changes require explicit
Release/Security, Shared/Architecture, macOS Client, Windows Host, and Linux Host
review. Lab soak and release approval have not been performed.
