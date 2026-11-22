# Host presence

<!-- source: shared/deck-core/src/presence.rs (HostPresence, PresenceSource, NoPresence, DemoPresence, DEMO_ENV); shared/deck-core/src/screens/home.rs (render_home_with, row_second_line, detail_connection) -->

Home can show one status line under each saved machine and under its name
in the machine's column: **Ready**, **You're signed in · resume**, **In use
by someone else** (or **In use by ‹who›**) and **Off**. When the user's own
session is signed in, the primary button reads **Resume** and asks for no
password. This note records what feeds that line today and what is
expected to feed it later.

## Today: nothing is known

No Pier broadcasts who is signed in, and the Deck asks no Pier for it: there
is no protocol message for presence, and this work adds none. The Deck's
source is `NoPresence`, which answers `HostPresence::Unknown` for every
machine. With `Unknown`, Home shows the machine's address as the list's
second line and no status line in the column, and Connect asks for a
username and password as it always has. The Deck never guesses a status.

## The seam

`arcen_deck_core::presence` is shared and OS-free:

- `HostPresence`: `Unknown`, `Ready`, `SignedIn { by_you, who, since }`,
  `InUse { who }` and `Off`, with the phrase Home shows and the primary's
  verb (`Connect` or `Resume`).
- `PresenceSource`: one method, `presence(id, name)`, answered at once from
  what the source already knows; it never blocks the UI thread.
- `presence::current()`: the source Home uses (`render_home` calls
  `render_home_with` with it), always `NoPresence` in a release build.

Home is already wired: a source that returns a known status changes the
list's second line, the column's status line and the primary's label with
no other change.

## Later: a presence service

A future service that reports which sessions exist on the machines it
knows about can implement `PresenceSource` (fed, for example, by a
subscription the Deck keeps off the UI thread). That needs its own protocol
design, review by Shared/Architecture and Release/Security, and an ADR; it
is not implemented.

**Privacy rule.** `who` is shown only when that service's policy allows it.
Without that permission the service reports `InUse { who: None }` or
`SignedIn { by_you: false, who: None, .. }`, which Home shows as **In use by
someone else**. The Deck never infers a name from anything else.

## Screenshots only: the demo source

A developer build (the `dev-tools` feature, `arcen-deck-core/dev-tools`)
also compiles `DemoPresence`: with `ARCEN_DECK_PRESENCE_DEMO=1` in the
Deck's environment, it invents a status from a hash of each machine's name,
the same for the same name, so screenshots can show every state. A release
build has neither the source nor the variable: it always uses
`NoPresence`.
