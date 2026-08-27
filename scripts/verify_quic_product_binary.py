#!/usr/bin/env python3
"""Reject shipped product binaries that contain dormant WSS wire identifiers."""

from __future__ import annotations

import argparse
from pathlib import Path


FORBIDDEN = (
    b"transport:wss-v1",
    b"wss://",
    b"arcen-direct-wss",
)
CHUNK_BYTES = 1024 * 1024


def verify(path: Path) -> None:
    overlap = max(len(value) for value in FORBIDDEN) - 1
    previous = b""
    with path.open("rb") as stream:
        while chunk := stream.read(CHUNK_BYTES):
            window = previous + chunk
            for value in FORBIDDEN:
                if value in window:
                    raise ValueError(
                        f"{path}: release binary contains dormant WSS marker "
                        f"{value.decode('ascii')!r}"
                    )
            previous = window[-overlap:]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path, nargs="+")
    args = parser.parse_args()
    for path in args.binary:
        if not path.is_file():
            parser.error(f"binary does not exist: {path}")
        try:
            verify(path)
        except ValueError as error:
            parser.exit(1, f"error: {error}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
