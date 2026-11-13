#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS
"""The one personal-account reference publication allows: the canonical
public repository URL.

Arcen's public repository lives under a personal GitHub account, and the
source offers in every binary, the README, NOTICE and CHANGELOG link to it.
The publication checks forbid that account name everywhere else, so this
module defines exactly which text is the canonical URL and removes it before
those checks run:

    URL                           (CANONICAL_URL below)
    URL/releases                  optionally /latest or /tag/<tag>
    URL/compare/<tag>...<tag>
    URL/tree/main                 optionally /<path> and #<anchor>
    URL/blob/<release tag>/<path> optionally #<anchor>; the tag is vX.Y.Z[-pre]

The scheme is optional; nothing else under the account (another repository,
`/blob` at a branch, `/issues`, a bare owner/name slug, an email address, the bare account
name) is allowed. Only the fixed `[https://]github.com/<account>/<repository>`
prefix of a match is replaced, by spaces of the same length (so offsets stay
correct); the path after it is still checked.

Command-line use (scripts/ci/check-publication-hygiene.sh):

    git grep -nIEi PATTERN … | publication_allowlist.py --filter PATTERN

prints only the `file:line:text` hits whose text still matches PATTERN once
the canonical URL is removed, and exits 1 if any remain.
"""

from __future__ import annotations

import re
import sys

CANONICAL_URL = "https://github.com/Aanerud/arcen_public"

# Built from CANONICAL_URL so the account name appears in this file only
# inside the URL itself. Each allowed path has a strict shape:
#
#   /releases[/latest | /tag/<tag>]
#   /compare/<ref>...<ref>            refs are tag-like: letters, digits, . _ -
#   /tree/main[/<path>][#<anchor>]    path: letters, digits, . _ / -
#   /blob/vX.Y.Z[-pre]/<path>[#<anchor>]  an immutable release-tag link
#
# and the URL must end at whitespace, closing punctuation or a sentence-ending
# ".". Anything else (a query, "@", ":", ".git", another path) is not the
# canonical URL at all, so the account name in it stays visible to the checks.
_PREFIX = r"(?P<prefix>(?:https?://)?" + re.escape(CANONICAL_URL.removeprefix("https://")) + r")"
_REF = r"[A-Za-z0-9._-]+"
_TAIL = (
    r"(?:/releases(?:/latest|/tag/" + _REF + r")?"
    r"|/compare/" + _REF + r"\.\.\." + _REF +
    r"|/tree/main(?:/[A-Za-z0-9._/-]*)?(?:#[A-Za-z0-9_-]+)?"
    r"|/blob/v[0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.]+)?/[A-Za-z0-9._/-]+(?:#[A-Za-z0-9_-]+)?"
    r")?"
)
_END = r"(?=$|[\s)\]\"'<>`,;|*]|\.(?=$|[\s)\]\"'<>`]))"
CANONICAL_URL_PATTERN = re.compile(
    r"(?<![A-Za-z0-9_./-])" + _PREFIX + _TAIL + _END,
    re.IGNORECASE,
)


def strip_canonical(text: str) -> str:
    """`text` with the fixed prefix of every canonical URL blanked out.

    Only `[https://]github.com/<account>/<repository>` is replaced, by spaces
    of the same length; the path after it stays, so a denylist rule still sees
    any name or address written into it.
    """

    def blank(match: re.Match) -> str:
        start, end = match.span("prefix")
        whole = match.group(0)
        offset = start - match.start()
        return whole[:offset] + " " * (end - start) + whole[offset + end - start:]

    return CANONICAL_URL_PATTERN.sub(blank, text)


def remaining_hits(lines, pattern: str) -> list[str]:
    """`git grep -n` lines that still match `pattern` without the canonical URL."""
    rule = re.compile(pattern, re.IGNORECASE)
    kept = []
    for line in lines:
        line = line.rstrip("\n")
        parts = line.split(":", 2)
        content = parts[2] if len(parts) == 3 else line
        if rule.search(strip_canonical(content)):
            kept.append(line)
    return kept


def main(argv: list[str]) -> int:
    if len(argv) != 3 or argv[1] != "--filter":
        print(__doc__, file=sys.stderr)
        return 2
    kept = remaining_hits(sys.stdin, argv[2])
    for line in kept:
        print(line)
    return 1 if kept else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
