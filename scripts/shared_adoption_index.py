#!/usr/bin/env python3
"""Shared Adoption Index (SAI): a trend of how much each product builds on shared/.

Informational only. It counts dependencies and references, not correctness,
so it is never a gate: the enforced rules live in check_shared_contracts.py.

Components (0-100 each):
  breadth   (20%) declared arcen-* shared dependencies / 11
  reach     (25%) % of .rs files that reference an arcen_* shared path
  depth     (25%) shared references per KLOC, capped at 25/KLOC = 100
  retention (30%) 100 - 3 x (% LOC in files with no OS marker)
"""

from __future__ import annotations

import os
import re
import sys
import tomllib
from pathlib import Path

SHARED = {
    "arcen_" + name
    for name in (
        "identity input keel media observability outputs protocol session "
        "telemetry transport usb_bridge"
    ).split()
}
PRODUCTS = ["hosts/linux", "hosts/windows", "hosts/macos", "clients/macos", "hosts/capenc"]
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
    dependencies = len(declared_dependencies(root / product))
    reach = 100 * reaching / max(files, 1)
    depth = min(100.0, 100 * (references / kloc) / 25)
    portable_share = 100 * portable / max(lines, 1)
    score = (
        0.2 * 100 * dependencies / 11
        + 0.25 * reach
        + 0.25 * depth
        + 0.3 * max(0.0, 100 - 3 * portable_share)
    )
    return {
        "kloc": kloc,
        "dependencies": dependencies,
        "reach": reach,
        "references_per_kloc": references / kloc,
        "portable_share": portable_share,
        "score": score,
    }


def print_table(root: Path) -> None:
    print(f"{'product':16}{'KLOC':>6}{'deps':>7}{'reach%':>8}{'ref/KLOC':>10}{'portable%':>11}{'SAI':>6}")
    for product in PRODUCTS:
        if not (root / product).exists():
            continue
        row = measure(root, product)
        print(
            f"{product:16}{row['kloc']:6.1f}{row['dependencies']:>4}/11{row['reach']:8.0f}"
            f"{row['references_per_kloc']:10.1f}{row['portable_share']:11.1f}{row['score']:6.0f}"
        )


if __name__ == "__main__":
    print_table(Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).resolve().parent.parent)
