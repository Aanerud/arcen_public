# Clipboard redirection privacy and trust boundary

Clipboard v1 is available only after exact subprotocol-version negotiation on
direct macOS Deck connections to the active Windows Pier and to Linux Piers
serving a dedicated-Xorg or headless Wayland (GNOME/Mutter) desktop. The host policy is authoritative in both directions and is rechecked
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
A headless Wayland desktop whose agent did not prove the compositor clipboard
at start advertises disabled. Linux helpers receive the negotiated directions
as well as the host policy, so a helper never reads a kind the Deck did not
ask for. A Linux host copy larger than the policy is refused (helper diagnostic
`clipboard_host_item_refused`, reason `too_large`), never truncated, and the
session goes on.

Text is UTF-8. Images are PNG on the wire; DIBV5 and TIFF exist only inside the
Windows and macOS adapters. Files, HTML, RTF, delayed rendering, and
Gateway/Span transport are not implemented.

On a headless Wayland desktop the Pier reaches the clipboard only through the
session agent's `arcen-clipboard.sock`. It proves that socket the way it
proves the input socket: the socket and directory are private to the session
user, and the peer is the agent's uid and pid. The agent drives Mutter's
`RemoteDesktop` clipboard on a session of its own. It subscribes to selection
changes, and reads the selection, only while a Pier is connected. The owner
present when a Deck attaches is not sent.

After a remote injection the endpoint OS clipboard may continue owning the
latest item after the Arcen connection ends. Windows stores the eager native
payload, macOS pasteboard retains its eager item, and the Linux child releases
its X11 selection on disconnect (so its remotely injected item becomes
unavailable). On a Linux Wayland desktop the bridge releases its selection on
disconnect. Mutter's clipboard manager has already copied it, though (text up
to 4 MiB, images up to 200 MiB), and reinstalls that copy. The item therefore
stays on the session's clipboard, as on Windows and macOS. Users must overwrite or clear sensitive clipboard content when
residual endpoint ownership is unacceptable.

This implementation and its dependency/notice changes require explicit
Release/Security, Shared/Architecture, macOS Client, Windows Host, and Linux Host
review. Lab soak and release approval have not been performed.
