#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS

"""Shared Adoption Index (SAI): a trend of how much each product builds on shared/.

Informational only. It counts dependencies and references, not correctness,
so it is never a gate: the enforced rules live in check_shared_contracts.py
(and the earned-differences ledger it checks).

Components (0-100 each):
  breadth   (20%) declared arcen-* shared dependencies / shared crates that
                  apply to the product (hosts are not scored on Deck-only crates)
  reach     (25%) share of the product that references an arcen_* shared path
  depth     (25%) shared references per KLOC, capped at 25/KLOC = 100
  retention (30%) v2: 100 - portable % (the share of the product's code in
                  items that touch an OS API); v1: 100 - 3 x (% LOC in files
                  with no OS marker)

v2 (the default) measures production code only: no tests/, examples/,
benches/, generated bindings or #[cfg(test)] items, and no comments or string
contents. Portability is judged per item (fn, struct, impl member...) rather
than per file (an item is portable when it has no OS marker and names nothing
imported from an OS crate), so one OS call no longer hides a file of portable
logic and a
doc comment naming "NVENC" no longer makes a file OS-bound. Reach is weighted
by lines, so a sub-crate of many small OS-domain files no longer outweighs the
product's core. The `dup%` column is the share of the product's normalised
production lines that sit in contiguous clones (12 lines or more) of another
product's code; it is a drift signal and is not part of the score.

v1 (``--v1``) is the original whole-file, raw-text index, kept so earlier
numbers stay reproducible.

Usage: shared_adoption_index.py [ROOT] [--v1] [--record]
  --record appends a dated v2 row to docs/architecture/shared-adoption-history.md
"""

from __future__ import annotations

import argparse
import datetime
import os
import re
import tomllib
from pathlib import Path

import rust_scan

SHARED = {
    "arcen_" + name
    for name in (
        "deck_core identity input keel media observability outputs protocol "
        "session telemetry transport usb_bridge"
    ).split()
}
PRODUCTS = [
    "hosts/linux",
    "hosts/windows",
    "hosts/macos",
    "clients/macos",
    "clients/windows",
    "hosts/capenc",
]
# Shared crates only a Deck can use; a Pier is not scored on them.
DECK_ONLY = {"arcen_deck_core"}


def applicable(product: str) -> set[str]:
    return SHARED if product.startswith("clients/") else SHARED - DECK_ONLY


REFERENCE = re.compile(r"\b(arcen_[a-z_]+)::")
OS_MARKER = re.compile(
    r"\bunsafe\b|extern\s+\"|\bwindows(_sys)?::|\bobjc2|core_foundation|core_graphics|core_video"
    r"|core_media|\blibc::|\bx11|\bxcb|\bnix::|\bpam|cuda|nvenc|nvfbc|screencapturekit|\bmetal\b"
    r"|videotoolbox|video_toolbox|\bcocoa\b|block2|dispatch2|iokit|io_kit|pulse|alsa|wasapi"
    r"|cfg\(\s*(target_os|windows|unix)|std::os::(unix|windows)|\bCommand::new|/proc/|/sys/"
    r"|\bdbus|\bsystemd|\bxpc\b",
    re.IGNORECASE,
)


def declared_dependencies(root: Path) -> set[str]:
    found: set[str] = set()
    for directory, names, files in os.walk(root):
        names[:] = [name for name in names if name != "target"]
        if "Cargo.toml" not in files:
            continue
        manifest = tomllib.loads((Path(directory) / "Cargo.toml").read_text())

        def walk(node: object) -> None:
            if isinstance(node, dict):
                for key, value in node.items():
                    if key == "dependencies" and isinstance(value, dict):
                        found.update(
                            name.replace("-", "_")
                            for name in value
                            if name.replace("-", "_") in SHARED
                        )
                    walk(value)

        walk(manifest)
    return found


def measure(root: Path, product: str) -> dict[str, float]:
    lines = files = reaching = references = portable = 0
    for directory, names, entries in os.walk(root / product):
        names[:] = [name for name in names if name != "target"]
        for entry in entries:
            if not entry.endswith(".rs"):
                continue
            text = (Path(directory) / entry).read_text(errors="replace")
            count = text.count("\n")
            lines += count
            files += 1
            found = [name for name in REFERENCE.findall(text) if name in SHARED]
            references += len(found)
            reaching += bool(found)
            if not OS_MARKER.search(text):
                portable += count
    kloc = max(lines, 1) / 1000
    scope = applicable(product)
    dependencies = len(declared_dependencies(root / product) & scope)
    reach = 100 * reaching / max(files, 1)
    depth = min(100.0, 100 * (references / kloc) / 25)
    portable_share = 100 * portable / max(lines, 1)
    score = (
        0.2 * 100 * dependencies / len(scope)
        + 0.25 * reach
        + 0.25 * depth
        + 0.3 * max(0.0, 100 - 3 * portable_share)
    )
    return {
        "kloc": kloc,
        "dependencies": dependencies,
        "applicable": len(scope),
        "reach": reach,
        "references_per_kloc": references / kloc,
        "portable_share": portable_share,
        "score": score,
    }


# --- v2 ---------------------------------------------------------------------------

# OS markers applied to code only (comments and string contents blanked).
# Identifier-like markers need a word boundary, so `NativeNvenc` or
# `impulse` no longer count.
OS_MARKER_V2 = re.compile(
    r"\bunsafe\b|extern\s+\"|\bwindows(_sys)?::|\bwinapi\b|\bobjc2|\bcore_foundation|\bcore_graphics"
    r"|\bcore_video|\bcore_media|\blibc::|\bx11|\bxcb|\bnix::|\bpam\b|\bpam_|\bcuda|\bnvenc|\bnvfbc"
    r"|\bscreencapturekit|\bmetal\b|\bvideotoolbox|\bvideo_toolbox|\bcocoa\b|\bblock2|\bdispatch2"
    r"|\biokit|\bio_kit|\blibpulse|\bpulse\b|\balsa\b|\bwasapi|\bzbus\b|\breis\b|\bevdev\b|\bpipewire"
    r"|\bwayland_client|cfg!?\s*\(\s*(target_os|windows|unix)|std::os::(unix|windows)|\bCommand::new"
    r"|\bdbus|\bsystemd|\bxpc\b",
    re.IGNORECASE,
)
# Paths into the file system's OS views, matched in string literals.
OS_PATH = re.compile(r'"/(?:proc|sys|dev)/')
USE_ITEM = re.compile(r"^[ \t]*(?:pub(?:\([^)]*\))?\s+)?use\s+([^;]*);", re.MULTILINE)
IDENTIFIER = re.compile(r"[A-Za-z_][A-Za-z0-9_]{3,}")
WORD = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
# Names too generic to mark an item as OS-bound when imported from an OS crate.
GENERIC = frozenset(
    "Result Error Interface Option BOOL HANDLE PCWSTR PWSTR GUID HRESULT Self self super crate core "
    "windows Win32 Foundation System Graphics Common Ref Owned Free Retained".split()
)
HISTORY_PATH = "docs/architecture/shared-adoption-history.md"
HISTORY_PRODUCTS = {
    "hosts/linux": "Linux Pier",
    "hosts/windows": "Windows Pier",
    "hosts/macos": "macOS Pier",
    "clients/macos": "macOS Deck",
    "clients/windows": "Windows Deck",
    "hosts/capenc": "capenc",
}


def _os_identifiers(code: str) -> set[str]:
    """Identifiers a file imports from an OS crate (``use windows::...::LogonUserW``)."""
    names: set[str] = set()
    for match in USE_ITEM.finditer(code):
        body = match.group(1)
        if OS_MARKER_V2.search(body):
            names.update(n for n in IDENTIFIER.findall(body.split("::", 1)[-1]) if n not in GENERIC)
    return names


def _code_lines(text: str) -> int:
    return sum(1 for line in text.splitlines() if line.strip())


def portable_lines(source: rust_scan.SourceFile) -> int:
    """Non-blank code lines in items that touch no OS API."""
    code = source.code
    imported = _os_identifiers(code)
    portable = 0
    for start, end in rust_scan.leaf_spans(code):
        body = code[start:end]
        if USE_ITEM.match(body):
            continue
        if OS_MARKER_V2.search(body) or (imported and not imported.isdisjoint(WORD.findall(body))):
            continue
        if OS_PATH.search(source.code_with_strings[start:end]):
            continue
        portable += _code_lines(body)
    return portable


def measure_v2(root: Path, product: str, duplicated: int = 0, normalised: int = 0) -> dict[str, float]:
    lines = reaching = references = portable = 0
    for source in rust_scan.production_files(root, product):
        count = _code_lines(source.code)
        if not count:
            continue
        lines += count
        found = [name for name in REFERENCE.findall(source.code) if name in SHARED]
        references += len(found)
        if found:
            reaching += count
        portable += portable_lines(source)
    kloc = max(lines, 1) / 1000
    scope = applicable(product)
    dependencies = len(declared_dependencies(root / product) & scope)
    reach = 100 * reaching / max(lines, 1)
    depth = min(100.0, 100 * (references / kloc) / 25)
    portable_share = 100 * portable / max(lines, 1)
    score = (
        0.2 * 100 * dependencies / len(scope)
        + 0.25 * reach
        + 0.25 * depth
        + 0.3 * (100 - portable_share)
    )
    return {
        "kloc": kloc,
        "dependencies": dependencies,
        "applicable": len(scope),
        "reach": reach,
        "references_per_kloc": references / kloc,
        "portable_share": portable_share,
        "duplicated_share": 100 * duplicated / max(normalised, 1),
        "score": score,
    }


def measure_all_v2(root: Path) -> tuple[dict[str, dict[str, float]], rust_scan.CloneReport]:
    present = [product for product in PRODUCTS if (root / product).exists()]
    clones = rust_scan.clone_report(root, [*present, "packaging"])
    rows = {
        product: measure_v2(
            root, product, clones.duplicated.get(product, 0), clones.lines.get(product, 0)
        )
        for product in present
    }
    return rows, clones


def print_table_v1(root: Path) -> None:
    print(f"{'product':16}{'KLOC':>6}{'deps':>7}{'reach%':>8}{'ref/KLOC':>10}{'portable%':>11}{'SAI':>6}")
    for product in PRODUCTS:
        if not (root / product).exists():
            continue
        row = measure(root, product)
        print(
            f"{product:16}{row['kloc']:6.1f}{row['dependencies']:>4}/{row['applicable']}{row['reach']:8.0f}"
            f"{row['references_per_kloc']:10.1f}{row['portable_share']:11.1f}{row['score']:6.0f}"
        )


def print_table(root: Path, v1: bool = False) -> None:
    if v1:
        print_table_v1(root)
        return
    rows, clones = measure_all_v2(root)
    print(
        f"{'product':16}{'KLOC':>6}{'deps':>7}{'reach%':>8}{'ref/KLOC':>10}{'portable%':>11}"
        f"{'dup%':>6}{'SAI v2':>8}{'v1':>5}"
    )
    for product, row in rows.items():
        old = measure(root, product)["score"]
        print(
            f"{product:16}{row['kloc']:6.1f}{row['dependencies']:>4}/{row['applicable']}{row['reach']:8.0f}"
            f"{row['references_per_kloc']:10.1f}{row['portable_share']:11.1f}"
            f"{row['duplicated_share']:6.1f}{row['score']:8.0f}{old:5.0f}"
        )
    print(f"Linux↔Windows Pier clones: {clones.pair_lines('hosts/linux', 'hosts/windows')} lines")


HISTORY_HEADER = """# Shared adoption history

<!-- One row per `python3 scripts/shared_adoption_index.py --record`. -->

Each product cell is `SAI v2 · dup%`: the Shared Adoption Index (0–100,
higher means more of the product is built on `shared/`) and the share of the
product's production lines that are clones of another product's code (lower
means less logic that can drift). The last column is the cloned production
lines shared by the Linux and Windows Piers. How each number is computed is documented in
`scripts/shared_adoption_index.py`; the gate on differences is
[platform-differences.md](platform-differences.md).

| Date | Version | {products} | Linux↔Windows clones |
| --- | --- | {aligns} | ---: |
"""


def record(root: Path) -> str:
    rows, clones = measure_all_v2(root)
    # The workspace version, not a commit: the history is published, and an
    # internal commit hash means nothing outside this repository.
    version_file = root / "VERSION"
    version = version_file.read_text().strip() if version_file.is_file() else "unknown"
    cells = [
        f"{rows[p]['score']:.0f} · {rows[p]['duplicated_share']:.1f} %" if p in rows else "—"
        for p in HISTORY_PRODUCTS
    ]
    line = (
        f"| {datetime.date.today().isoformat()} | {version} | "
        + " | ".join(cells)
        + f" | {clones.pair_lines('hosts/linux', 'hosts/windows'):,} |\n"
    )
    path = root / HISTORY_PATH
    if not path.is_file():
        path.write_text(
            HISTORY_HEADER.format(
                products=" | ".join(HISTORY_PRODUCTS.values()),
                aligns=" | ".join("---:" for _ in HISTORY_PRODUCTS),
            )
        )
    with path.open("a") as handle:
        handle.write(line)
    return line


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("root", nargs="?", type=Path, default=Path(__file__).resolve().parent.parent)
    parser.add_argument("--v1", action="store_true", help="the original whole-file index")
    parser.add_argument("--record", action="store_true", help=f"append a dated v2 row to {HISTORY_PATH}")
    args = parser.parse_args()
    print_table(args.root, v1=args.v1)
    if args.record:
        print(f"recorded: {record(args.root)}", end="")


if __name__ == "__main__":
    main()
