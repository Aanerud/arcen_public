#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS

"""Fast, dependency-free scanning of Rust source for the shared-adoption tools.

Used by ``platform_differences.py`` (the earned-differences ledger check) and
``shared_adoption_index.py`` (SAI v2). Everything here is regex-driven so a
scan of every product stays in the low seconds:

* ``blank_comments`` / ``blank_comments_and_strings`` replace comments (and
  string contents) with spaces, keeping every offset and newline, so a match
  in the blanked text is a match in code at the same line;
* ``strip_tests`` blanks ``#[cfg(test)]``/``#[test]`` items;
* ``production_files`` lists a product's production Rust (no ``tests/``,
  ``examples/``, ``benches/``, build output or generated bindings);
* ``items`` splits a file into top-level items, descending one level into
  ``impl``/``mod``/``trait`` blocks;
* ``clone_report`` finds contiguous cross-product clones of at least N
  normalised lines with a rolling window hash.
"""

from __future__ import annotations

import os
import re
from dataclasses import dataclass, field
from pathlib import Path

PRODUCTS = [
    "hosts/linux",
    "hosts/windows",
    "hosts/macos",
    "clients/macos",
    "clients/windows",
    "hosts/capenc",
]

SKIPPED_DIRS = frozenset({"target", "tests", "examples", "benches", ".git"})

_LEXEME = re.compile(
    r"//[^\n]*"
    r"|/\*.*?\*/"
    r"|(?<![A-Za-z0-9_])b?r(#*)\".*?\"\1"
    r"|(?<![A-Za-z0-9_])b?\"(?:\\.|[^\"\\])*\""
    r"|(?<![A-Za-z0-9_])b?'(?:\\u\{[0-9A-Fa-f]+\}|\\.|[^'\\\n])'",
    re.DOTALL,
)
_NOT_NEWLINE = re.compile(r"[^\n]")
GENERATED = re.compile(r"@generated|automatically generated|DO NOT EDIT", re.IGNORECASE)


def _spaces(text: str) -> str:
    return _NOT_NEWLINE.sub(" ", text)


def _blank_string(text: str) -> str:
    if text.lstrip("b").startswith("'"):
        opening, closing = text.index("'"), text.rindex("'")
    else:
        opening, closing = text.index('"'), text.rindex('"')
    return text[: opening + 1] + _spaces(text[opening + 1 : closing]) + text[closing:]


def blank_both(source: str) -> tuple[str, str]:
    """(comments blanked, comments and string contents blanked) in one pass;
    offsets and line breaks are unchanged in both."""
    comments: list[str] = []
    everything: list[str] = []
    previous = 0
    for match in _LEXEME.finditer(source):
        start, end = match.span()
        between = source[previous:start]
        comments.append(between)
        everything.append(between)
        text = match.group(0)
        if text.startswith("/"):
            blank = _spaces(text)
            comments.append(blank)
            everything.append(blank)
        else:
            comments.append(text)
            everything.append(_blank_string(text))
        previous = end
    comments.append(source[previous:])
    everything.append(source[previous:])
    return "".join(comments), "".join(everything)


def blank_comments(source: str) -> str:
    """Comments replaced by spaces; strings kept; offsets and lines unchanged."""
    return blank_both(source)[0]


def blank_comments_and_strings(source: str) -> str:
    """Comments and string/char contents replaced by spaces; offsets unchanged."""
    return blank_both(source)[1]


_TEST_ATTRIBUTE = re.compile(
    r"#\[\s*(?:cfg\s*\(\s*(?:all\s*\(\s*)?test\b[^\]]*\)|test|tokio::test[^\]]*|rstest)\s*\]"
)
_ATTRIBUTE = re.compile(r"\s*#\[[^\]]*\]")
_ITEM_DELIMITER = re.compile(r"[{}()\[\];]")
_FIELD_DELIMITER = re.compile(r"->|=>|[{}()\[\]<>,;]")
_ATTRIBUTED_ITEM = re.compile(
    r"\s*(?:pub(?:\s*\([^)]*\))?\s+)?(?:default\s+|const\s+|async\s+|unsafe\s+|extern\s+\"[^\"]*\"\s+)*"
    r"(?:fn|mod|impl|struct|enum|trait|static|const|type|use|union|macro_rules!|extern\s+crate)\b"
    r"|\s*[A-Za-z_][A-Za-z0-9_:]*!\s*[({\[]"
)


def _item_end(blanked: str, start: int) -> int:
    """End offset (exclusive) of the item starting at ``start``: through its
    brace-matched body, or through a ``;`` reached outside any bracket before
    any ``{`` (so ``[u8; 4]`` in a signature does not end it). Stops before a
    ``}`` that closes an enclosing block."""
    braces = groups = 0
    for match in _ITEM_DELIMITER.finditer(blanked, start):
        char = match.group(0)
        if char in "([":
            groups += 1
        elif char in ")]":
            groups -= 1
        elif char == ";":
            if braces == 0 and groups <= 0:
                return match.end()
        elif char == "{":
            braces += 1
        elif braces == 0:
            return match.start()
        else:
            braces -= 1
            if braces == 0:
                return match.end()
    return len(blanked)


def _field_end(blanked: str, start: int) -> int:
    """End offset of an attributed field, variant, struct-literal field or
    statement: through the next ``,`` or ``;`` outside any bracket, or up to
    the ``}`` that closes the enclosing block."""
    depth = 0
    for match in _FIELD_DELIMITER.finditer(blanked, start):
        char = match.group(0)
        if char in ("->", "=>"):
            continue
        if char in "([{<":
            depth += 1
        elif char in ")]>":
            depth -= 1
        elif char == "}":
            if depth == 0:
                return match.start()
            depth -= 1
        elif depth == 0:
            return match.end()
    return len(blanked)


def test_spans(blanked: str) -> list[tuple[int, int]]:
    """Offsets of test-only items in comment- and string-blanked code."""
    spans = []
    index = 0
    while True:
        match = _TEST_ATTRIBUTE.search(blanked, index)
        if not match:
            break
        cursor = match.end()
        while True:
            attribute = _ATTRIBUTE.match(blanked, cursor)
            if not attribute:
                break
            cursor = attribute.end()
        if match.group(0).startswith("#[cfg") and not _ATTRIBUTED_ITEM.match(blanked, cursor):
            end = _field_end(blanked, cursor)
        else:
            end = _item_end(blanked, cursor)
        spans.append((match.start(), end))
        index = end
    return spans


def blank_spans(source: str, spans: list[tuple[int, int]]) -> str:
    if not spans:
        return source
    out = []
    previous = 0
    for start, end in spans:
        out.append(source[previous:start])
        out.append(_spaces(source[start:end]))
        previous = end
    out.append(source[previous:])
    return "".join(out)


def strip_tests(source: str, blanked: str | None = None) -> str:
    """``source`` with test-only items replaced by blank lines (offsets kept)."""
    if blanked is None:
        blanked = blank_comments_and_strings(source)
    return blank_spans(source, test_spans(blanked))


@dataclass
class SourceFile:
    path: str  # repository-relative, POSIX separators
    text: str  # production text: test items blanked, comments kept
    code: str  # production code: comments and string contents blanked
    code_with_strings: str  # production code: comments blanked, strings kept

    @property
    def lines(self) -> int:
        return sum(1 for line in self.code.splitlines() if line.strip())


_CACHE: dict[tuple[str, str], list[SourceFile]] = {}


def is_generated(text: str) -> bool:
    return bool(GENERATED.search(text[:2000]))


def production_files(root: Path, prefix: str) -> list[SourceFile]:
    """Production Rust under ``root/prefix`` (cached per root and prefix)."""
    key = (str(root.resolve()), prefix)
    if key in _CACHE:
        return _CACHE[key]
    files: list[SourceFile] = []
    base = root / prefix
    for directory, names, entries in os.walk(base):
        names[:] = sorted(name for name in names if name not in SKIPPED_DIRS)
        for entry in sorted(entries):
            if not entry.endswith(".rs"):
                continue
            path = Path(directory) / entry
            raw = path.read_text(errors="replace")
            if is_generated(raw):
                continue
            with_strings, blanked = blank_both(raw)
            spans = test_spans(blanked)
            files.append(
                SourceFile(
                    path=path.relative_to(root).as_posix(),
                    text=blank_spans(raw, spans),
                    code=blank_spans(blanked, spans),
                    code_with_strings=blank_spans(with_strings, spans),
                )
            )
    _CACHE[key] = files
    return files


def clear_cache() -> None:
    _CACHE.clear()


# --- Items ----------------------------------------------------------------------

_ITEM_START = re.compile(
    r"^[ \t]*(?:pub(?:\s*\([^)]*\))?\s+)?(?:default\s+)?(?:const\s+|async\s+|unsafe\s+|extern\s+\"[^\"]*\"\s+|extern\s+)*"
    r"(fn|impl|struct|enum|mod|trait|static|const|type|union|macro_rules!|use|extern\s+crate)\b",
    re.MULTILINE,
)
_CONTAINERS = {"impl", "mod", "trait"}


@dataclass
class Item:
    kind: str
    start: int  # offset in the code text
    end: int
    children: list["Item"] = field(default_factory=list)


def _items_in(code: str, start: int, end: int, depth: int) -> list[Item]:
    found: list[Item] = []
    cursor = start
    while True:
        match = _ITEM_START.search(code, cursor, end)
        if not match:
            return found
        kind = match.group(1).split()[0]
        item_end = min(_item_end(code, match.end()), end)
        item = Item(kind, match.start(), item_end)
        if kind in _CONTAINERS and depth == 0:
            body = code.find("{", match.end(), item_end)
            if body != -1:
                item.children = _items_in(code, body + 1, item_end - 1, depth + 1)
        found.append(item)
        cursor = max(item_end, match.end())


def items(code: str) -> list[Item]:
    """Top-level items of blanked ``code``; ``impl``/``mod``/``trait`` blocks
    carry their member items as ``children``."""
    return _items_in(code, 0, len(code), 0)


def leaf_spans(code: str) -> list[tuple[int, int]]:
    """Item spans to classify one by one: container members replace their
    container; the container's own header lines are folded into the first."""
    spans = []
    for item in items(code):
        if item.children:
            spans.extend((child.start, child.end) for child in item.children)
        else:
            spans.append((item.start, item.end))
    return spans


# --- Clone detection ------------------------------------------------------------

_TRIVIAL = re.compile(r"^[\s{}()\[\];,.]*$|^(?:use|pub use|mod|pub mod)\b|^#\[")


def normalised_lines(code: str) -> list[tuple[int, str]]:
    """(1-based line number, whitespace-collapsed text) of non-trivial lines."""
    out = []
    for number, line in enumerate(code.splitlines(), 1):
        text = " ".join(line.split())
        if text and not _TRIVIAL.match(text):
            out.append((number, text))
    return out


@dataclass
class CloneReport:
    window: int
    # (product a, product b) -> lines of a covered by windows also in b
    covered: dict[tuple[str, str], int]
    # product -> lines covered by a window found in any other product
    duplicated: dict[str, int]
    # product -> non-trivial production lines
    lines: dict[str, int]
    # (product a, product b) -> [(file a, file b, lines)] largest first
    files: dict[tuple[str, str], list[tuple[str, str, int]]]

    def pair_lines(self, a: str, b: str) -> int:
        """Duplicated lines between two products (the larger side)."""
        return max(self.covered.get((a, b), 0), self.covered.get((b, a), 0))


def clone_report(root: Path, products: list[str], window: int = 12) -> CloneReport:
    """Contiguous clones of at least ``window`` normalised lines between the
    production code of different products (rolling window hash)."""
    occurrences: dict[int, set[tuple[str, str]]] = {}
    per_file: list[tuple[str, str, list[int], list[int]]] = []
    lines: dict[str, int] = {}
    for product in products:
        total = 0
        for source in production_files(root, product):
            normal = normalised_lines(source.code)
            total += len(normal)
            if len(normal) < window:
                continue
            hashes = [hash(text) for _, text in normal]
            numbers = [number for number, _ in normal]
            keys = [hash(tuple(hashes[i : i + window])) for i in range(len(hashes) - window + 1)]
            per_file.append((product, source.path, numbers, keys))
            for key in keys:
                occurrences.setdefault(key, set()).add((product, source.path))
        lines[product] = total
    covered: dict[tuple[str, str], int] = {}
    duplicated: dict[str, int] = {}
    file_pairs: dict[tuple[str, str, str, str], set[int]] = {}
    for product, path, numbers, keys in per_file:
        by_other: dict[str, set[int]] = {}
        for index, key in enumerate(keys):
            owners = occurrences[key]
            if len(owners) == 1:
                continue
            for other_product, other_path in owners:
                if other_product == product:
                    continue
                span = range(index, index + window)
                by_other.setdefault(other_product, set()).update(span)
                file_pairs.setdefault((product, path, other_product, other_path), set()).update(span)
        union: set[int] = set()
        for other, indexes in by_other.items():
            covered[(product, other)] = covered.get((product, other), 0) + len(indexes)
            union |= indexes
        duplicated[product] = duplicated.get(product, 0) + len(union)
    files: dict[tuple[str, str], list[tuple[str, str, int]]] = {}
    for (product, path, other, other_path), indexes in file_pairs.items():
        files.setdefault((product, other), []).append((path, other_path, len(indexes)))
    for value in files.values():
        value.sort(key=lambda row: -row[2])
    for product in products:
        duplicated.setdefault(product, 0)
    return CloneReport(window, covered, duplicated, lines, files)
