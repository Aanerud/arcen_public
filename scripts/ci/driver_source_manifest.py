#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS
"""Check or rewrite a driver source manifest on any operating system.

`hosts/windows/driver/arcen-iddcx/source-manifest.json` pins the SHA-256 of
every file in the IddCx driver source directory. `verify-driver-source.ps1`
enforces it on Windows before a driver build, so an edit anywhere in that
directory (even a comment or a licence header) that does not regenerate the
manifest only fails on a Windows build machine. This script applies the same
rules — every file except the manifest itself and `bin/`/`obj/` output, no
missing or extra files, upper-case hex SHA-256 of the bytes on disk — so the
publication hygiene gate catches drift on any OS.

Usage:
    scripts/ci/driver_source_manifest.py --check [DIR ...]
    scripts/ci/driver_source_manifest.py --write [DIR ...]

Without DIR, every directory under hosts/windows/driver/ that has a
source-manifest.json is processed.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
MANIFEST = "source-manifest.json"
SCHEMA_VERSION = 1


def source_files(directory: Path) -> list[str]:
    files = []
    for path in directory.rglob("*"):
        if not path.is_file() or path.name == MANIFEST and path.parent == directory:
            continue
        relative = path.relative_to(directory)
        if any(part in ("bin", "obj") for part in relative.parts[:-1]):
            continue
        files.append(relative.as_posix())
    return sorted(files)


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest().upper()


def problems(directory: Path) -> list[str]:
    manifest_path = directory / MANIFEST
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        return [f"{manifest_path}: unreadable ({error})"]
    if manifest.get("schema_version") != SCHEMA_VERSION:
        return [f"{manifest_path}: unsupported schema_version"]
    expected = {name.replace("\\", "/"): value.upper() for name, value in manifest.get("files", {}).items()}
    actual = source_files(directory)
    found = []
    for name in sorted(set(expected) - set(actual)):
        found.append(f"{directory}: manifest lists {name}, which does not exist")
    for name in sorted(set(actual) - set(expected)):
        found.append(f"{directory}: {name} is not in the manifest")
    for name in sorted(set(expected) & set(actual)):
        if digest(directory / name) != expected[name]:
            found.append(f"{directory}: {name} does not match its manifest hash")
    return found


def write(directory: Path) -> None:
    manifest = {
        "schema_version": SCHEMA_VERSION,
        "files": {name: digest(directory / name) for name in source_files(directory)},
    }
    (directory / MANIFEST).write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")


def default_directories() -> list[Path]:
    return sorted(path.parent for path in (REPO / "hosts" / "windows" / "driver").glob(f"*/{MANIFEST}"))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--check", action="store_true")
    mode.add_argument("--write", action="store_true")
    parser.add_argument("directories", nargs="*", type=Path)
    args = parser.parse_args(argv)
    directories = args.directories or default_directories()
    if args.write:
        for directory in directories:
            write(directory)
            print(f"wrote {directory / MANIFEST}")
        return 0
    found = [problem for directory in directories for problem in problems(directory)]
    for problem in found:
        print(problem, file=sys.stderr)
    if found:
        print("regenerate with scripts/ci/driver_source_manifest.py --write", file=sys.stderr)
        return 1
    print(f"driver source manifests: ok ({len(directories)})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
