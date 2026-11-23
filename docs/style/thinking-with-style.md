<!-- SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception -->
<!-- Copyright (C) 2026 TUTTIFRUTTI AS -->
# Thinking with Style

The house style for text a user reads: release notes, `CHANGELOG.md` entries,
public GitHub release bodies and Deck UI strings. It follows William Strunk
Jr., *The Elements of Style* (Ithaca, N.Y., 1918; public domain,
[Project Gutenberg #37134](https://www.gutenberg.org/ebooks/37134)).

## Strunk's rules we keep

1. **Omit needless words.** "Vigorous writing is concise." Every word must
   earn its place; cut what the reader would not miss.
2. **Use the active voice.** "The Deck reconnects", not "a reconnection is
   performed".
3. **Put statements in positive form.** Say what happens: "keeps the
   session", not "does not fail to keep the session".
4. **Use definite, specific, concrete language.** "Alt+G reaches the host",
   not "improved shortcut handling".
5. **Place the emphatic words at the end.** End the sentence on what the user
   gets.
6. **One paragraph per topic.** A paragraph, or a bullet, says one thing.

## Arcen's rules for release notes

7. **One sentence, from the user's side.** A note says what a user can now do,
   or what no longer goes wrong. Add a second sentence only for a limit the
   user must know.
8. **No archaeology.** Internal causes, file and function names, PR numbers,
   ADRs, test counts and the story of the bug stay in commits and pull
   requests.
9. **Group by product:** Linux Pier, Windows Pier, macOS Pier, macOS Deck,
   Windows Deck. List a shared change under each product it reaches.
10. **At most about five items per product per release.** Fold the rest into
    one line, or leave it to the commit log.
11. **Open each release with a headline** of one or two sentences.
12. **Deck UI strings follow rules 1–7:** short, active, and they tell the
    user what to do next.

## Before and after (from 0.21.0)

> **Before:** The 0.20.7 belief that a Mac cannot report its pointer's image
> was a naming bug: the cursor was read correctly, but compared byte for byte
> with AppKit's own copies, which never match the window server's (ADR 0029
> amendment 1).
>
> **After:** The Deck draws the Mac's pointer in the Mac's own shapes, so it
> moves with your hand.

> **Before:** A moment in which the displays cannot be read whole (a failed
> display or device read, nothing lit, an output already proved dark that
> cannot be described for a moment, or one of them dropping out during a mode
> set …) no longer ends a Deck's session at once.
>
> **After:** With desk privacy on, a brief moment when the displays cannot be
> read no longer ends the session.

> **Before:** With the system shortcuts going to the remote computer (the
> default on a PC), the Windows Deck's keyboard hook now takes every chord
> another app may claim … before Windows matches hot keys.
>
> **After:** Alt+G and other shortcuts that another app claims now reach the
> host.

## Where it applies

Write each entry under `[Unreleased]` in `CHANGELOG.md` in this style as the
change lands; the release procedure is in
[Versioning and release metadata](../operations/versioning-and-releases.md).
Detail a developer needs belongs in the commit message, the pull request, an
ADR or an architecture document, not in the release note.
