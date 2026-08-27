# arcen-session-probe

A minimal Deck stand-in that completes a full Arcen session handshake against a
Pier and reports whether media frames actually arrive. It exists because a
review harness that stopped short of the full negotiation produced a false S1
("the Linux Pier delivers no frames", PERF-368) that cost real time.

## Why it is in the repository

Nothing else in the tree can answer "does this host actually stream?" without a
GUI client on the right hardware. This can, from any machine with Python, in
about thirty seconds, and it prints a JSON summary suitable for pasting into a
finding.

## The negotiation, in the order the Pier requires it

This ordering is the whole point of the tool; getting any step wrong produces a
`1011` close with a message that does not obviously name the missing step.

1. The **host speaks first** with `auth_request`, carrying the challenge.
   Sending `client_hello` eagerly on connect makes the Pier parse it as the
   auth response and fail with ``missing field `method` ``.
2. Client sends `auth_response`. The `credential` field is the **plaintext
   password**: it is handed straight to PAM
   (`hosts/linux/src/net/server.rs:961`). It is not the
   `arcen_protocol::auth::hash_password` digest, which belongs to a different
   auth path. TLS is the confidentiality boundary.
3. Host sends `auth_result`, then `server_hello`.
4. Only *now* does the client send `client_hello`. It must include
   `device_capabilities`; the field has no serde default and its absence is
   reported as ``missing field `device_capabilities` ``.
5. Client sends `quality_settings`. Without it the Pier closes with
   `initial quality negotiation failed`.
6. Media frames begin arriving as binary WebSocket messages.

## Usage

```sh
PROBE_HOST=203.0.113.10 \
PROBE_USER=arcen-test \
PROBE_PASS="$SOME_PASSWORD" \
PROBE_SECONDS=30 \
python3 tools/session-probe/arcen-session-probe.py
```

Exit status is 0 when at least one media frame arrived, 1 otherwise, so it can
gate a script.

The password is read from the environment only and is never printed, never
written to disk, and never placed in argv.

## Interpreting the output

`first_frame_ms` is time from TCP connect to the first media frame, so it
includes TLS, PAM, session creation and encoder start. It is a useful
time-to-first-frame proxy but is not an input-to-photon latency measurement.

A low `binary_frames` count against a long `PROBE_SECONDS` is not by itself a
defect: an idle desktop with no motion legitimately produces few frames.
Compare like with like, and drive some screen motion if you want a rate.
