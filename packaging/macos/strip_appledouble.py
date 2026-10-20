#!/usr/bin/env python3
"""Remove AppleDouble ("._") companions from a flat component package.

macOS 26 tags every file a provenance-tracked process writes with the
`com.apple.provenance` extended attribute, and neither `xattr -c` nor
`xattr -d` can remove it. `pkgbuild` encodes extended attributes it finds as
an AppleDouble "._" entry beside every file and directory, in the payload and
in the scripts archive, and neither `COPYFILE_DISABLE` nor `xattr -cr` stops
it. Installer folds the companions back into attributes, but the payload
lists twice as many entries as the product has files.

The provenance attribute is local bookkeeping and means nothing on the Mac
the package installs, so this drops those companions. It refuses a companion
that carries anything else (a resource fork, Finder info or another
attribute): that would be content, and silently dropping it would change what
installs.

Usage:
  strip_appledouble.py COMPONENT_PKG   rewrite a flat component package in place
  strip_appledouble.py --check PKG     fail if a payload or scripts archive of
                                       PKG (component or product) has a "._" entry
"""

from __future__ import annotations

import argparse
import gzip
import re
import shutil
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

DROPPABLE_ATTRIBUTES = frozenset({"com.apple.provenance"})

APPLEDOUBLE_MAGIC = 0x00051607
APPLEDOUBLE_VERSION = 0x00020000
ENTRY_RESOURCE_FORK = 2
ENTRY_FINDER_INFO = 9
FINDER_INFO_SIZE = 32
ATTR_MAGIC = b"ATTR"
ATTR_HEADER_SIZE = 36

ODC_MAGIC = b"070707"
ODC_HEADER_SIZE = 76
ODC_TRAILER = "TRAILER!!!"


class StripError(Exception):
    pass


def is_appledouble_name(path: str) -> bool:
    return path.rpartition("/")[2].startswith("._")


def appledouble_attributes(data: bytes) -> list[str]:
    """Return the extended attribute names an AppleDouble file carries.

    Raises StripError if it carries a resource fork or Finder info, which are
    content rather than local bookkeeping."""
    if len(data) < 26:
        raise StripError("truncated AppleDouble header")
    magic, version = struct.unpack_from(">II", data, 0)
    if magic != APPLEDOUBLE_MAGIC or version != APPLEDOUBLE_VERSION:
        raise StripError("not an AppleDouble file")
    (count,) = struct.unpack_from(">H", data, 24)
    finder = None
    for index in range(count):
        kind, offset, length = struct.unpack_from(">III", data, 26 + 12 * index)
        if offset + length > len(data):
            raise StripError("AppleDouble entry runs past the end of the file")
        if kind == ENTRY_FINDER_INFO:
            finder = (offset, length)
        elif kind == ENTRY_RESOURCE_FORK:
            if length:
                raise StripError("carries a resource fork")
        else:
            raise StripError(f"carries AppleDouble entry type {kind}")
    if finder is None:
        return []
    offset, length = finder
    if any(data[offset : offset + FINDER_INFO_SIZE]):
        raise StripError("carries Finder info")
    if length <= FINDER_INFO_SIZE:
        return []
    # copyfile(3) pads the attribute header to a 4-byte boundary after the
    # Finder info.
    header = offset + FINDER_INFO_SIZE + 2
    if data[header : header + 4] != ATTR_MAGIC:
        raise StripError("malformed extended-attribute header")
    (attribute_count,) = struct.unpack_from(">H", data, header + ATTR_HEADER_SIZE - 2)
    names = []
    cursor = header + ATTR_HEADER_SIZE
    for _ in range(attribute_count):
        _, _, _, name_length = struct.unpack_from(">IIHB", data, cursor)
        raw = data[cursor + 11 : cursor + 11 + name_length]
        names.append(raw.rstrip(b"\0").decode("utf-8"))
        cursor = (cursor + 11 + name_length + 3) & ~3
    return names


def check_droppable(path: str, data: bytes) -> None:
    try:
        names = appledouble_attributes(data)
    except StripError as error:
        raise StripError(f"{path}: {error}; refusing to drop it") from None
    kept = sorted(set(names) - DROPPABLE_ATTRIBUTES)
    if kept:
        raise StripError(f"{path}: carries {', '.join(kept)}; refusing to drop it")


def read_odc(data: bytes) -> list[tuple[str, bytes, bytes]]:
    """Split an odc cpio archive into (name, raw entry bytes, file body)."""
    entries = []
    position = 0
    while True:
        header = data[position : position + ODC_HEADER_SIZE]
        if len(header) < ODC_HEADER_SIZE or header[:6] != ODC_MAGIC:
            raise StripError(f"not an odc cpio entry at offset {position}")
        name_size = int(header[59:65], 8)
        file_size = int(header[65:76], 8)
        name_start = position + ODC_HEADER_SIZE
        body_start = name_start + name_size
        end = body_start + file_size
        if end > len(data):
            raise StripError("cpio entry runs past the end of the archive")
        name = data[name_start : body_start - 1].decode("utf-8")
        entries.append((name, data[position:end], data[body_start:end]))
        position = end
        if name == ODC_TRAILER:
            return entries


def strip_cpio(data: bytes, label: str) -> tuple[bytes, list[str], int]:
    """Return the archive without droppable "._" entries, the dropped names and
    the number of entries kept (not counting the trailer)."""
    entries = read_odc(data)
    names = {name for name, _, _ in entries}
    kept, dropped = [], []
    for name, raw, body in entries:
        if name != ODC_TRAILER and is_appledouble_name(name):
            head, _, base = name.rpartition("/")
            sibling = f"{head}/{base[2:]}" if head else base[2:]
            if sibling not in names:
                raise StripError(f"{label}: {name} has no companion file; refusing to drop it")
            check_droppable(f"{label}: {name}", body)
            dropped.append(name)
            continue
        kept.append(raw)
    return b"".join(kept), dropped, len(kept) - 1


def read_gzip_cpio(path: Path) -> bytes:
    raw = path.read_bytes()
    if raw[:2] != b"\x1f\x8b":
        raise StripError(f"{path.name} is not a gzip cpio archive")
    return gzip.decompress(raw)


def write_gzip(path: Path, data: bytes) -> None:
    path.write_bytes(gzip.compress(data, compresslevel=9, mtime=0))


def strip_scripts_directory(scripts: Path) -> list[str]:
    dropped = []
    for path in sorted(scripts.rglob("._*")):
        sibling = path.with_name(path.name[2:])
        if not sibling.exists():
            raise StripError(f"Scripts/{path.relative_to(scripts)} has no companion file")
        check_droppable(f"Scripts/{path.relative_to(scripts)}", path.read_bytes())
        dropped.append(str(path.relative_to(scripts)))
        path.unlink()
    return dropped


def rewrite_bom(bom: Path) -> None:
    listing = subprocess.run(
        ["lsbom", str(bom)], check=True, capture_output=True, text=True
    ).stdout.splitlines()
    kept = [line for line in listing if not is_appledouble_name(line.split("\t", 1)[0])]
    file_list = bom.with_name("Bom.list")
    file_list.write_text("".join(f"{line}\n" for line in kept), encoding="utf-8")
    subprocess.run(["mkbom", "-i", str(file_list), str(bom)], check=True)
    file_list.unlink()
    rebuilt = subprocess.run(
        ["lsbom", str(bom)], check=True, capture_output=True, text=True
    ).stdout.splitlines()
    if rebuilt != kept:
        raise StripError("the rebuilt Bom does not match the filtered listing")


def rewrite_package_info(package_info: Path, file_count: int) -> None:
    text = package_info.read_text(encoding="utf-8")
    updated, replaced = re.subn(
        r'numberOfFiles="\d+"', f'numberOfFiles="{file_count}"', text, count=1
    )
    if replaced != 1:
        raise StripError("PackageInfo has no payload numberOfFiles")
    package_info.write_text(updated, encoding="utf-8")


def strip_component(package: Path) -> int:
    expanded = package.with_name(package.name + ".expanded")
    if expanded.exists():
        shutil.rmtree(expanded)
    subprocess.run(["pkgutil", "--expand", str(package), str(expanded)], check=True)
    try:
        payload = expanded / "Payload"
        if not payload.is_file():
            raise StripError(f"{package.name} is not a flat component package")
        data, dropped, kept = strip_cpio(read_gzip_cpio(payload), "Payload")
        write_gzip(payload, data)
        rewrite_bom(expanded / "Bom")
        rewrite_package_info(expanded / "PackageInfo", kept)

        scripts = expanded / "Scripts"
        if scripts.is_dir():
            dropped += [f"Scripts/{name}" for name in strip_scripts_directory(scripts)]
        elif scripts.is_file():
            data, dropped_scripts, _ = strip_cpio(read_gzip_cpio(scripts), "Scripts")
            write_gzip(scripts, data)
            dropped += [f"Scripts/{name}" for name in dropped_scripts]

        package.unlink()
        subprocess.run(["pkgutil", "--flatten", str(expanded), str(package)], check=True)
    finally:
        shutil.rmtree(expanded, ignore_errors=True)
    left = appledouble_entries(package)
    if left:
        raise StripError(f"{package.name} still has AppleDouble entries: {left[:5]}")
    return len(dropped)


def appledouble_entries(package: Path) -> list[str]:
    """Every "._" entry in the payloads and scripts archives of PACKAGE."""
    payload = subprocess.run(
        ["pkgutil", "--payload-files", str(package)],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.splitlines()
    found = [line for line in payload if is_appledouble_name(line)]
    with tempfile.TemporaryDirectory(prefix=".appledouble.", dir=package.parent) as scratch:
        expanded = Path(scratch) / "expanded"
        subprocess.run(
            ["pkgutil", "--expand", str(package), str(expanded)],
            check=True,
            capture_output=True,
        )
        found += [
            str(path.relative_to(expanded))
            for path in sorted(expanded.rglob("._*"))
            if "Scripts" in path.relative_to(expanded).parts
        ]
    return found


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--check", action="store_true")
    parser.add_argument("package", type=Path)
    args = parser.parse_args(argv)
    package = args.package.resolve()
    try:
        if args.check:
            found = appledouble_entries(package)
            if found:
                print(f"error: {package.name} has {len(found)} AppleDouble entries:", file=sys.stderr)
                for name in found[:10]:
                    print(f"  {name}", file=sys.stderr)
                return 1
            return 0
        count = strip_component(package)
        print(f"==> removed {count} AppleDouble entries from {package.name}")
        return 0
    except (StripError, subprocess.CalledProcessError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
