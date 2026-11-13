#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS
"""Add or check the Arcen licence header on first-party source files.

Every first-party source file starts with two comment lines:

    SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
    Copyright (C) 2026 TUTTIFRUTTI AS

in the comment syntax of its language, after any line that must stay first
(a shebang, an XML declaration, a `<!doctype>`, `@echo off`). Files that
cannot carry a comment (JSON, images, fixtures, plists) and vendored
third-party code are covered by REUSE.toml instead, so this script never
touches them.

Usage:
    scripts/licensing/spdx-headers.py --check   # exit 1 if a header is missing
    scripts/licensing/spdx-headers.py --write   # add missing headers in place

Only tracked files (`git ls-files`) are considered. An existing bare
`AGPL-3.0-only` identifier is upgraded to the full expression.
"""

from __future__ import annotations

import argparse
import fnmatch
import subprocess
import sys
from pathlib import Path

# Assembled so this file does not itself look like a header to REUSE tools.
TAG = "SPDX-" + "License-Identifier:"
EXPRESSION = "AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception"
COPYRIGHT = "Copyright (C) 2026 TUTTIFRUTTI AS"

# Paths whose licensing is declared in REUSE.toml, never by a header here.
SKIP = [
    "third_party/*",
    "LICENSES/*",
    "LICENSE",
    "LICENSE-*",
    "NOTICE",
    "REUSE.toml",
    "Cargo.lock",
    "*/Cargo.lock",
    "VERSION",
    "hosts/windows/data/*",
    "scripts/lab/usb-vhci/*.patch",
    "scripts/lab/usb-vhci/usb-vhci.config.h",
    # Arcen-authored binary USB descriptor fixtures; annotated in REUSE.toml.
    # Captured vendor descriptors are deliberately not listed.
    "shared/usb-bridge/fixtures/wacom-intuos5-touch-l/device.bin",
    "shared/usb-bridge/fixtures/wacom-intuos5-touch-l/configuration.bin",
    "shared/usb-bridge/fixtures/wacom-intuos5-touch-l/report-interface0.bin",
    "shared/usb-bridge/fixtures/wacom-intuos5-touch-l/report-interface1.bin",
    "shared/usb-bridge/fixtures/wacom-intuos5-touch-l/report-interface2.bin",
    # Derived from the vendored MIT nv-codec-headers header; MIT in-file.
    "hosts/capenc/src/nvenc_sys/guid.rs",
    "hosts/capenc/src/nvenc_sys/version.rs",
    "hosts/capenc/src/nvenc_sys/windows_sys/nvEncodeAPI.rs",
    "*.md",
    "*.json",
    "*.jsonl",
    "*.txt",
    "*.png",
    "*.icns",
    "*.svg",
    "*.plist",
    "*.entitlements",
    "*.sha256",
    "*.inf",
    "*.vcxproj",
    "*.xml",
    "*.wxs",
    "*.orig",
    "*.patch",
]

LINE = "line"
BLOCK = "block"
XML = "xml"

# extension or exact basename -> (style, prefix, suffix)
STYLES: dict[str, tuple[str, str, str]] = {}
for _ext in ("rs", "swift", "metal", "c", "h", "cpp", "m", "mm", "hlsl", "wgsl", "js", "ts", "modulemap"):
    STYLES["." + _ext] = (LINE, "// ", "")
for _ext in ("py", "sh", "ps1", "psm1", "toml", "yml", "yaml", "cmake", "conf", "service",
             "tmpfiles", "logrotate", "pl", "rb"):
    STYLES["." + _ext] = (LINE, "# ", "")
STYLES[".cil"] = (LINE, "; ", "")
STYLES[".cmd"] = (LINE, "REM ", "")
STYLES[".bat"] = (LINE, "REM ", "")
STYLES[".css"] = (BLOCK, "/* ", " */")
STYLES[".html"] = (XML, "<!-- ", " -->")
for _name in ("Containerfile", "Dockerfile", "Makefile", "CMakeLists.txt", "CODEOWNERS",
              ".gitignore", ".gitattributes", "pre-commit", "postinstall", "preinstall", "arcen"):
    STYLES[_name] = (LINE, "# ", "")


def style_for(path: str) -> tuple[str, str, str] | None:
    name = path.rsplit("/", 1)[-1]
    if name in STYLES:
        return STYLES[name]
    if "." in name:
        return STYLES.get("." + name.rsplit(".", 1)[-1])
    return None


def skipped(path: str) -> bool:
    return any(fnmatch.fnmatchcase(path, pattern) for pattern in SKIP)


def header_lines(style: tuple[str, str, str]) -> list[str]:
    _, prefix, suffix = style
    return [f"{prefix}{TAG} {EXPRESSION}{suffix}", f"{prefix}{COPYRIGHT}{suffix}"]


def keep_first(line: str, path: str) -> bool:
    """Lines that must stay above the header."""
    stripped = line.lstrip("\ufeff").strip().lower()
    if line.startswith("#!") and not line.startswith("#!["):
        return True
    if stripped.startswith("<?xml") or stripped.startswith("<!doctype"):
        return True
    if path.endswith((".cmd", ".bat")) and stripped in ("@echo off", "@setlocal"):
        return True
    return False


def process(path: Path, rel: str, write: bool) -> str | None:
    """Returns a problem description, or None when the file is fine."""
    style = style_for(rel)
    if style is None:
        return f"{rel}: no comment syntax known; add it to REUSE.toml or to this script"
    raw = path.read_bytes()
    if b"\0" in raw[:4096]:
        return f"{rel}: binary file; declare it in REUSE.toml"
    text = raw.decode("utf-8")
    newline = "\r\n" if "\r\n" in text else "\n"
    lines = text.split(newline)
    head = lines[:12]
    has_full = any(TAG in line and EXPRESSION in line for line in head)
    has_copyright = any(COPYRIGHT in line for line in head)
    if has_full and has_copyright:
        return None
    if not write:
        return f"{rel}: missing or incomplete licence header"
    # Drop a bare, older identifier line; the full one replaces it.
    lines = [
        line for index, line in enumerate(lines)
        if not (index < 12 and TAG in line and EXPRESSION not in line and "AGPL-3.0-only" in line)
    ]
    insert_at = 0
    while insert_at < len(lines) and insert_at < 2 and keep_first(lines[insert_at], rel):
        insert_at += 1
    new = header_lines(style)
    if style[0] == LINE and insert_at < len(lines) and lines[insert_at].strip() and not (
        lines[insert_at].lstrip().startswith(style[1].strip())
    ):
        new.append("")
    lines[insert_at:insert_at] = new
    path.write_bytes(newline.join(lines).encode("utf-8"))
    return None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--check", action="store_true")
    mode.add_argument("--write", action="store_true")
    parser.add_argument("--root", default=".", help="repository root (default: current directory)")
    args = parser.parse_args()
    root = Path(args.root).resolve()
    files = subprocess.run(
        ["git", "-C", str(root), "ls-files", "-z"],
        check=True, capture_output=True,
    ).stdout.decode().split("\0")
    problems = []
    for rel in files:
        if not rel or skipped(rel):
            continue
        path = root / rel
        if not path.is_file() or path.is_symlink():
            continue
        problem = process(path, rel, args.write)
        if problem:
            problems.append(problem)
    for problem in problems:
        print(problem, file=sys.stderr)
    if problems:
        print(f"{len(problems)} file(s) need attention", file=sys.stderr)
        return 1
    print("licence headers: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
