# Security Policy

Arcen is remote-access software. A Pier accepts network connections, injects
input into a live desktop session, and on Windows ships a credential provider
that participates in the logon path. Bugs here matter more than in most
software, so please report them properly rather than publicly.

## Reporting a vulnerability

Use **GitHub's private vulnerability reporting** on this repository:
*Security* → *Report a vulnerability*. That opens a private advisory visible
only to the maintainer.

Please do not open a public issue for a security problem.

Include what you have: affected component (Pier, Deck, transport, credential
provider), version or commit, what an attacker can achieve, and reproduction
steps if you have them.

## What to expect

This project has **no support commitment and no response-time guarantee** — see
[SUPPORT.md](SUPPORT.md). Security reports are read and taken seriously, but
they are handled by one person on a best-effort basis. There is no bug bounty.

If you need a guaranteed response, Arcen is not a suitable dependency for you.

## Scope

In scope: authentication and session admission, the QUIC/TLS transport and its
certificate trust logic, input injection, the Windows credential provider,
privilege boundaries in the host helpers, and anything that lets an
unauthenticated peer reach a session.

Out of scope: findings that require an already-root/administrator attacker on
the host, denial of service through sheer bandwidth, and the deliberate
`InsecureSkipVerify` development mode (it is double-gated and documented as
unsafe).

## Deployment note

Arcen is designed for **direct machine-to-machine connections on a trusted
network**. It is not hardened for exposure directly to the public internet, and
nothing in this repository should be read as a claim that it is. Put it behind a
VPN or a network you control.
