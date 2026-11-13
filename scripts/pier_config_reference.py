#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS

"""Keep docs/operations/config-reference.md in step with the Pier's config types.

The Pier configuration is strict JSON (`deny_unknown_fields`) read into Rust
types: the shared `arcen_session::pier_config::PierConfig<P>` plus one
platform section per host (`LinuxPlatformConfig`, `WindowsPlatformConfig`,
`MacOsPlatformConfig`). This script reads those types from source — the same
fields serde accepts, with `rename`/`alias`/`skip` honoured — and checks that
the reference lists exactly those keys, each with a type, default, meaning and
the release it first appeared in.

  python3 scripts/pier_config_reference.py            # check (CI, tests)
  python3 scripts/pier_config_reference.py --write    # add rows for new keys
  python3 scripts/pier_config_reference.py --since    # print each key's first release (needs git history)

`--write` appends a stub row for every key the document lacks, with its
first release computed from git history, and fails (rather than deleting
prose) on rows whose key no longer exists.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, Iterable

REPO = Path(__file__).resolve().parent.parent
DOCUMENT = REPO / "docs" / "operations" / "config-reference.md"

# Section heading -> (root struct, its crate, key prefix).
SECTIONS: dict[str, tuple[str, str, str]] = {
    "## Common keys": ("PierConfig", "shared/session", ""),
    "## Linux `platform.*` keys": ("LinuxPlatformConfig", "hosts/linux", "platform."),
    "## Windows `platform.*` keys": ("WindowsPlatformConfig", "hosts/windows", "platform."),
    "## macOS `platform.*` keys": ("MacOsPlatformConfig", "hosts/macos", "platform."),
}

# Types whose serde shape is another struct (`try_from`/custom Deserialize).
SERDE_PROXIES = {"QosTargets": "QosTargetsWire", "LoggingConfig": "LoggingConfigRaw"}

# The generic platform parameter is documented in its own sections.
COMMON_EXCLUDED = {"platform"}

SOURCE_DIRS = ("shared", "hosts")

# What each release shipped, oldest first, by release tag. A key's "since" is
# the first release whose source has it; anything already in 0.13.0 is "≤0.13"
# because earlier history is not release-tagged. A tag missing from this
# checkout contributes nothing, so a key it would have dated falls to a later
# release; every key published so far is already dated in the document.
RELEASES = [
    ("0.13.0", "v0.13.0"),
    ("0.14.0", "v0.14.0"),
    ("0.15.0", "v0.15.0"),
    ("0.16.0", "v0.16.0"),
    ("0.16.1", "v0.16.1"),
    ("0.17.0", "v0.17.0"),
    ("0.18.0", "v0.18.0"),
]
BASELINE = "≤0.13"
SINCE_PATTERN = re.compile(r"^(≤0\.13|\d+\.\d+\.\d+)$")

COLUMNS = ["Key", "Type", "Default", "Since", "Meaning", "Takes effect"]

STRUCT_RE = re.compile(r"^(?:pub(?:\([^)]*\))?\s+)?struct\s+(\w+)(?:<[^>]*>)?\s*\{\s*$")
FIELD_RE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(r#)?(\w+)\s*:\s*(.+?),?\s*$")
SERDE_RE = re.compile(r"#\[serde\((.*)\)\]")


@dataclass
class Field:
    name: str
    rust_type: str
    aliases: list[str] = field(default_factory=list)


@dataclass
class Struct:
    name: str
    crate: str
    fields: list[Field]


def crate_of(path: str) -> str:
    parts = path.replace("\\", "/").split("/")
    return "/".join(parts[:2])


def parse_structs(path: str, text: str) -> list[Struct]:
    """Named-field structs in one Rust file, with serde field names."""
    structs = []
    lines = text.splitlines()
    index = 0
    while index < len(lines):
        match = STRUCT_RE.match(lines[index].strip())
        index += 1
        if not match:
            continue
        name = match.group(1)
        fields: list[Field] = []
        attributes: list[str] = []
        depth = 1
        while index < len(lines) and depth > 0:
            line = lines[index].strip()
            index += 1
            if line.startswith("}"):
                depth -= 1
                continue
            if depth != 1 or not line or line.startswith("//"):
                continue
            if line.startswith("#["):
                attributes.append(line)
                continue
            found = FIELD_RE.match(line)
            if not found:
                attributes = []
                continue
            rust_name, rust_type = found.group(2), found.group(3)
            serde = " ".join(
                serde.group(1) for serde in map(SERDE_RE.search, attributes) if serde
            )
            attributes = []
            if re.search(r"\bskip(_deserializing)?\b", serde) or "flatten" in serde:
                continue
            rename = re.search(r'rename\s*=\s*"([^"]+)"', serde)
            aliases = re.findall(r'alias\s*=\s*"([^"]+)"', serde)
            fields.append(
                Field(rename.group(1) if rename else rust_name, rust_type, aliases)
            )
        structs.append(Struct(name, crate_of(path), fields))
    return structs


class Registry:
    def __init__(self, files: Iterable[tuple[str, str]]):
        self.structs: dict[tuple[str, str], Struct] = {}
        for path, text in files:
            for struct in parse_structs(path, text):
                self.structs.setdefault((struct.crate, struct.name), struct)

    def resolve(self, rust_type: str, crate: str) -> Struct | None:
        path = rust_type.split("::")
        name = SERDE_PROXIES.get(path[-1], path[-1])
        if path[0].startswith("arcen_"):
            crate = "shared/" + path[0][len("arcen_") :].replace("_", "-")
        if (crate, name) in self.structs:
            return self.structs[(crate, name)]
        matches = [struct for (_, other), struct in self.structs.items() if other == name]
        return matches[0] if len(matches) == 1 else None


def unwrap(rust_type: str) -> tuple[str, bool]:
    """Strips Option/Box; reports whether a Vec was crossed."""
    rust_type = rust_type.strip()
    sequence = False
    while True:
        match = re.match(r"^(?:std::\w+::)?(Option|Box|Vec)<(.+)>$", rust_type)
        if not match:
            return rust_type, sequence
        sequence |= match.group(1) == "Vec"
        rust_type = match.group(2).strip()


def keys_of(registry: Registry, root: str, crate: str, prefix: str) -> dict[str, list[str]]:
    """Every accepted dotted key under `root` -> its aliases."""
    struct = registry.structs.get((crate, root))
    if struct is None:
        return {}
    keys: dict[str, list[str]] = {}

    def walk(struct: Struct, prefix: str, seen: tuple[str, ...]) -> None:
        for item in struct.fields:
            if not prefix and root == "PierConfig" and item.name in COMMON_EXCLUDED:
                continue
            key = prefix + item.name
            inner, sequence = unwrap(item.rust_type)
            nested = registry.resolve(inner, struct.crate)
            if nested is not None and nested.name not in seen and nested.fields:
                walk(nested, key + ("[]." if sequence else "."), seen + (nested.name,))
            else:
                keys[key] = item.aliases

    walk(struct, prefix, (root,))
    return keys


def working_tree_files(root: Path = REPO) -> list[tuple[str, str]]:
    files = []
    for directory in SOURCE_DIRS:
        for path in sorted((root / directory).rglob("*.rs")):
            relative = path.relative_to(root).as_posix()
            if "/target/" in relative or "/tests/" in relative:
                continue
            text = path.read_text(encoding="utf-8")
            if "Deserialize" in text:
                files.append((relative, text))
    return files


def revision_files(revision: str) -> list[tuple[str, str]]:
    listing = subprocess.run(
        ["git", "grep", "-l", "Deserialize", revision, "--", *SOURCE_DIRS],
        cwd=REPO,
        check=False,
        capture_output=True,
        text=True,
    ).stdout.split()
    files = []
    for entry in listing:
        path = entry.split(":", 1)[1]
        if not path.endswith(".rs") or "/tests/" in path:
            continue
        text = subprocess.run(
            ["git", "show", f"{revision}:{path}"],
            cwd=REPO,
            check=True,
            capture_output=True,
            text=True,
        ).stdout
        files.append((path, text))
    return files


def expected_sections(files: list[tuple[str, str]]) -> dict[str, dict[str, list[str]]]:
    registry = Registry(files)
    return {
        heading: keys_of(registry, root, crate, prefix)
        for heading, (root, crate, prefix) in SECTIONS.items()
    }


def first_releases(current: dict[str, dict[str, list[str]]]) -> dict[str, str]:
    """Key -> first release that accepted it, or the next release if none."""
    since: dict[str, str] = {}
    for version, revision in RELEASES:
        for heading, keys in expected_sections(revision_files(revision)).items():
            for key in keys:
                since.setdefault(
                    f"{heading}\0{key}", BASELINE if version == RELEASES[0][0] else version
                )
    upcoming = (REPO / "VERSION").read_text(encoding="utf-8").strip()
    released = {version for version, _ in RELEASES}
    if upcoming in released:
        major, minor, _ = upcoming.split(".")
        upcoming = f"{major}.{int(minor) + 1}.0"
    for heading, keys in current.items():
        for key in keys:
            since.setdefault(f"{heading}\0{key}", upcoming)
    return since


@dataclass
class Row:
    key: str
    cells: list[str]
    line: int


def read_document(text: str) -> dict[str, list[Row]]:
    sections: dict[str, list[Row]] = {}
    current = None
    for number, line in enumerate(text.splitlines(), start=1):
        if line.startswith("## "):
            current = line.strip() if line.strip() in SECTIONS else None
            if current is not None:
                sections.setdefault(current, [])
            continue
        if current is None or not line.startswith("|"):
            continue
        cells = [cell.strip() for cell in line.strip().strip("|").split("|")]
        if cells[0] in ("Key", "") or set(cells[0]) <= set("-: "):
            continue
        key = re.fullmatch(r"`([^`]+)`", cells[0])
        sections[current].append(Row(key.group(1) if key else cells[0], cells, number))
    return sections


def check(text: str, expected: dict[str, dict[str, list[str]]]) -> list[str]:
    problems = []
    documented = read_document(text)
    for heading, keys in expected.items():
        rows = documented.get(heading)
        if rows is None:
            problems.append(f"missing section {heading!r}")
            continue
        seen = set()
        for row in rows:
            if row.key in seen:
                problems.append(f"line {row.line}: {row.key} is listed twice")
            seen.add(row.key)
            if row.key not in keys:
                problems.append(
                    f"line {row.line}: {row.key} is not a key the Pier accepts (removed or renamed?)"
                )
            if len(row.cells) != len(COLUMNS):
                problems.append(f"line {row.line}: expected {len(COLUMNS)} columns")
                continue
            for column, cell in zip(COLUMNS[1:], row.cells[1:]):
                if not cell or "TODO" in cell:
                    problems.append(f"line {row.line}: {row.key} has no {column}")
            if not SINCE_PATTERN.match(row.cells[3]):
                problems.append(
                    f"line {row.line}: {row.key} Since must be {BASELINE} or a release like 0.18.0"
                )
        for key in sorted(set(keys) - seen):
            problems.append(
                f"{heading}: {key} is accepted by the Pier but undocumented "
                "(run scripts/pier_config_reference.py --write and fill in the row)"
            )
    return problems


def stub_rows(
    text: str,
    expected: dict[str, dict[str, list[str]]],
    since: Callable[[str, str], str],
) -> str:
    documented = read_document(text)
    lines = text.splitlines()
    insertions: dict[int, list[str]] = {}
    for heading, keys in expected.items():
        rows = documented.get(heading, [])
        missing = sorted(set(keys) - {row.key for row in rows})
        if not missing or not rows:
            continue
        insertions[rows[-1].line] = [
            f"| `{key}` | TODO | TODO | {since(heading, key)} | TODO | TODO |" for key in missing
        ]
    output = []
    for number, line in enumerate(lines, start=1):
        output.append(line)
        output.extend(insertions.get(number, []))
    return "\n".join(output) + "\n"


CONFIG_SOURCE = "shared/session/src/pier_config.rs"


def sources_present(root: Path = REPO) -> bool:
    """Whether `root` holds the configuration types (not a fixture)."""
    return (root / CONFIG_SOURCE).exists()


def check_repository(root: Path = REPO) -> list[str]:
    """Problems with the reference in `root`, for other checkers."""
    document = root / "docs" / "operations" / "config-reference.md"
    if not document.exists():
        return [f"{document.relative_to(root)} is missing"]
    expected = expected_sections(working_tree_files(root))
    problems = [
        f"{heading}: no keys found in source; the {SECTIONS[heading][0]} type was not parsed"
        for heading, keys in expected.items()
        if not keys
    ]
    return problems + check(document.read_text(encoding="utf-8"), expected)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--write", action="store_true", help="add stub rows for new keys")
    mode.add_argument("--since", action="store_true", help="print each key's first release")
    arguments = parser.parse_args(argv)

    expected = expected_sections(working_tree_files())
    if arguments.since:
        releases = first_releases(expected)
        for heading, keys in expected.items():
            print(heading)
            for key in sorted(keys):
                print(f"  {key}\t{releases[heading + chr(0) + key]}")
        return 0
    text = DOCUMENT.read_text(encoding="utf-8")
    if arguments.write:
        releases = first_releases(expected)
        DOCUMENT.write_text(
            stub_rows(text, expected, lambda heading, key: releases[heading + "\0" + key]),
            encoding="utf-8",
        )
        text = DOCUMENT.read_text(encoding="utf-8")
    problems = check(text, expected)
    for problem in problems:
        print(f"config-reference: {problem}", file=sys.stderr)
    if problems:
        return 1
    total = sum(len(keys) for keys in expected.values())
    print(f"config-reference: {total} keys documented and in sync")
    return 0


if __name__ == "__main__":
    sys.exit(main())
