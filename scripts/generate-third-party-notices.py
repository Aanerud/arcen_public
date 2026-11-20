#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS

"""Generate the third-party dependency inventory from the locked dependency graph.

`legal/THIRD_PARTY_NOTICES.md` is exhaustive by policy, and the hand-maintained
table had drifted to covering 85 of the ~574 resolved packages. Hand-maintaining
an inventory of that size does not work; deriving it from `Cargo.lock` does.

What ships is not "every package cargo knows about". It is, per release
artefact, the packages reachable from that artefact's root through `normal` and
`build` dependency edges on that artefact's target triple. Dev-dependencies do
not ship. A package pulled in only for a platform we do not build does not ship
there. So the inventory is the union of three per-target resolutions rather than
one `--all-features` sweep, which would over-claim.

Each artefact is also resolved on its own, with only its own release features,
as its release build is (`cargo build -p <root>`): Cargo unifies features only
across the packages one build selects. A whole-workspace resolution would turn
on an optional dependency that one product enables in every product that shares
the crate (the certificate issuer in `arcen-transport`, for example, is the
macOS Pier's and the Windows installer's, not the Linux Pier's).

Build scripts and proc-macros run on the machine that builds the release, so
their own dependencies are resolved for that machine (`BUILD_HOST`), not for
the one running this script: `walkdir`, a build-dependency of `openh264-sys2`,
pulls `winapi-util` only when the Windows Pier is built on Windows. Cargo
takes its host from `rustc -vV`, so when the two differ this script runs
`cargo tree` with itself as `RUSTC`, reporting the build host and answering
host `--print` queries for it; everything else goes to the real compiler.
This needs a POSIX host (macOS or Linux, as CI); on Windows it works for the
Windows artefacts only, which is what `--host-only` checks.

Usage:
    scripts/generate-third-party-notices.py [--check [--host-only]] [--output PATH]

    --check      exit non-zero if the generated inventory differs from the file
                 on disk, without writing. This is what CI runs.
    --host-only  with --check, resolve only the artefacts built on this machine
                 and check exactly their part of the file: every package they
                 ship is listed for them under its declared licence, and no
                 package is listed for them that they no longer ship. The
                 Windows release builds run this; the full --check stays the
                 gate on macOS and Linux.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# Release artefact roots, by the target triple they are built for. A package is
# in the inventory if some artefact reaches it.
ARTEFACTS: dict[str, list[str]] = {
    "aarch64-apple-darwin": [
        "arcen-deck-macos",
        "arcen-usb-helper",
        "arcen-pier-macos",
        "arcen-microphone-driver",
    ],
    # The release zip also carries an Intel Deck (with its USB helper).
    "x86_64-apple-darwin": [
        "arcen-deck-macos",
        "arcen-usb-helper",
    ],
    "x86_64-unknown-linux-gnu": [
        "arcen-pier-linux",
        # Embedded in the installer (`ARCEN_PAM_GREETER`, built on its own
        # with `cargo build -p arcen-pam-greeter`).
        "arcen-pam-greeter",
        "arcen-pier-linux-installer",
    ],
    "x86_64-pc-windows-msvc": [
        "arcen-pier-windows",
        "arcen-credential-provider",
        "arcen-pier-windows-installer",
        # The Windows Deck MSI and the optional USB component MSI
        # (packaging/windows-deck/build.ps1).
        "arcen-deck-windows",
        "arcen-usb-service",
        "arcen-usb-prepare",
    ],
}

# Cargo features the release build of an artefact enables, by target triple.
# Optional dependencies are only in the resolution when their feature is on, so
# a release feature missing here silently drops its packages from the notices.
# Keep in step with the release build commands (the Linux Pier is built with
# `--features wayland-provider` and its default `native-tablet`; see
# packaging/linux/installer/README.md; the Deck embeds the USB helper by
# default, which turns on `native-tablet`, and builds the helper with its
# IOUSBHost backend; see packaging/macos/build-deck-app.sh). The Windows Deck,
# its USB service and prepare tool are built with their default features
# (packaging/windows-deck/build.ps1); add them here if that changes.
RELEASE_FEATURES: dict[str, list[str]] = {
    "aarch64-apple-darwin": [
        "arcen-deck-macos/native-tablet",
        "arcen-usb-helper/iousbhost",
    ],
    "x86_64-apple-darwin": [
        "arcen-deck-macos/native-tablet",
        "arcen-usb-helper/iousbhost",
    ],
    "x86_64-unknown-linux-gnu": ["arcen-pier-linux/wayland-provider"],
}

# The machine each target's release is built on, whose platform decides the
# dependencies of build scripts and proc-macros. The Intel Deck is
# cross-compiled on the Apple-silicon Mac.
BUILD_HOST: dict[str, str] = {
    "aarch64-apple-darwin": "aarch64-apple-darwin",
    "x86_64-apple-darwin": "aarch64-apple-darwin",
    "x86_64-unknown-linux-gnu": "x86_64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc": "x86_64-pc-windows-msvc",
}

# Environment that turns this script into the host-reporting `rustc` shim.
SHIM_HOST = "ARCEN_NOTICES_BUILD_HOST"
SHIM_REAL_RUSTC = "ARCEN_NOTICES_REAL_RUSTC"

# Roots one release command builds together, so Cargo unifies their features
# (`hosts/windows/build.cmd`: `cargo build -p arcen-pier-windows -p
# arcen-credential-provider`). Every other root is built on its own.
BUILT_TOGETHER: list[tuple[str, ...]] = [
    ("arcen-pier-windows", "arcen-credential-provider"),
]

GENERATED_HEADING = "## Generated dependency inventory"

LICENCE_FILENAMES = (
    "LICENSE",
    "LICENSE.md",
    "LICENSE.txt",
    "LICENSE-MIT",
    "LICENSE-APACHE",
    "LICENCE",
    "COPYING",
    "UNLICENSE",
)


def rustc_shim(arguments: list[str]) -> int:
    """Act as `rustc` for `cargo tree`, reporting `SHIM_HOST` as the host.

    Cargo reads the host from `rustc -vV` and the host's `cfg` from a
    `--print` query without `--target`; both are answered for the build host.
    Any other invocation goes to the real compiler unchanged.
    """
    import os

    real = os.environ[SHIM_REAL_RUSTC]
    host = os.environ[SHIM_HOST]
    if "-vV" in arguments:
        result = subprocess.run(
            [real, *arguments], capture_output=True, text=True, check=False
        )
        lines = [
            f"host: {host}" if line.startswith("host: ") else line
            for line in result.stdout.splitlines()
        ]
        sys.stdout.write("\n".join(lines) + "\n")
        sys.stderr.write(result.stderr)
        return result.returncode
    targeted = any(
        argument == "--target" or argument.startswith("--target=")
        for argument in arguments
    )
    if not targeted and any(argument.startswith("--print") for argument in arguments):
        arguments = [*arguments, "--target", host]
    return subprocess.run([real, *arguments], check=False).returncode


def real_rustc() -> tuple[str, str]:
    """The toolchain's own `rustc` and the host it reports."""
    sysroot = subprocess.run(
        ["rustc", "--print", "sysroot"],
        cwd=REPO,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    path = str(Path(sysroot) / "bin" / "rustc")
    version = subprocess.run(
        [path, "-vV"], capture_output=True, text=True, check=True
    ).stdout
    host = next(
        line.split(":", 1)[1].strip()
        for line in version.splitlines()
        if line.startswith("host:")
    )
    return path, host


def tree_environment(target: str) -> dict[str, str] | None:
    """The environment that makes `cargo tree` resolve for `target`'s
    build host, or `None` when this machine is that host."""
    import os

    path, host = real_rustc()
    build_host = BUILD_HOST[target]
    if host == build_host:
        return None
    if os.name == "nt":
        raise SystemExit(
            f"resolving {target} for its build host {build_host} needs a POSIX "
            "machine; run this script on macOS or Linux"
        )
    environment = dict(os.environ)
    environment.update(
        {
            "RUSTC": str(Path(__file__).resolve()),
            SHIM_REAL_RUSTC: path,
            SHIM_HOST: build_host,
            # Cargo caches what rustc reported; a cache from another host
            # would answer for the wrong one.
            "CARGO_CACHE_RUSTC_INFO": "0",
        }
    )
    return environment


def cargo_metadata(target: str) -> dict:
    """Resolve the dependency graph for one target triple."""
    result = subprocess.run(
        [
            "cargo",
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--filter-platform",
            target,
            *(
                ["--features", ",".join(RELEASE_FEATURES[target])]
                if target in RELEASE_FEATURES
                else []
            ),
        ],
        cwd=REPO,
        capture_output=True,
        text=True,
        # cargo writes UTF-8; Windows' default code page cannot decode it.
        encoding="utf-8",
        check=False,
    )
    if result.returncode != 0:
        raise SystemExit(
            f"cargo metadata failed for {target}:\n{result.stderr.strip()}"
        )
    return json.loads(result.stdout)


def release_builds(roots: list[str]) -> list[list[str]]:
    """The roots grouped as their release commands build them."""
    builds: list[list[str]] = []
    placed: set[str] = set()
    for root in roots:
        if root in placed:
            continue
        group = next((g for g in BUILT_TOGETHER if root in g), (root,))
        members = [member for member in group if member in roots]
        placed.update(members)
        builds.append(members)
    return builds


def build_features(target: str, build: list[str]) -> list[str]:
    """The release features of the packages one build selects."""
    return [
        feature
        for feature in RELEASE_FEATURES.get(target, [])
        if feature.split("/", 1)[0] in build
    ]


def shipped_name_versions(target: str, build: list[str]) -> set[tuple[str, str]]:
    """Every package one release build links, as (name, version).

    `cargo tree -p ROOT…` resolves features for the selected roots alone, as
    their release build does, and `-e normal,build` leaves out
    dev-dependencies: a test-only crate is not redistributed and listing it
    would misdescribe what we hand to users.
    """
    features = build_features(target, build)
    selected = [argument for root in build for argument in ("-p", root)]
    result = subprocess.run(
        [
            "cargo",
            "tree",
            "--locked",
            *selected,
            "--target",
            target,
            "-e",
            "normal,build",
            "--prefix",
            "none",
            "--format",
            "{p}",
            *(["--features", ",".join(features)] if features else []),
        ],
        cwd=REPO,
        capture_output=True,
        text=True,
        encoding="utf-8",
        env=tree_environment(target),
        check=False,
    )
    if result.returncode != 0:
        raise SystemExit(
            f"cargo tree failed for {' '.join(build)} on {target}:\n"
            f"{result.stderr.strip()}"
        )
    found: set[tuple[str, str]] = set()
    for line in result.stdout.splitlines():
        parts = line.split()
        if len(parts) >= 2 and parts[1].startswith("v"):
            found.add((parts[0], parts[1][1:]))
    return found


def is_first_party(package: dict) -> bool:
    """True for Arcen's own crates, which carry the workspace licence.

    Being inside the repository is not sufficient. `third_party/` holds vendored
    third-party sources — the patched `opusic-sys` and its bundled libopus —
    which live below this directory but are emphatically not ours. Treating them
    as first-party silently dropped them from the generated inventory, so their
    notices depended entirely on a hand-written section that `--check` could not
    see disappear.
    """
    try:
        relative = Path(package["manifest_path"]).relative_to(REPO)
    except ValueError:
        return False
    return relative.parts[0] != "third_party"


def licence_files(package: dict) -> list[tuple[str, str]]:
    """Read the licence texts shipped alongside a package's source.

    A table of package names, versions and URLs is an *inventory*. MIT, BSD and
    Apache-2.0 all require the copyright notice and permission text to travel
    with the distribution, so an inventory does not discharge the obligation —
    and this file is copied into the Windows, macOS and Linux distributions, so
    it is the notice users actually receive.
    """
    manifest = Path(package["manifest_path"]).parent
    # Matched without regard to case, as a case-insensitive macOS volume
    # matches them: `windows-rs` ships `license-mit`, and a case-sensitive
    # Linux file system would otherwise drop its notice and give CI a
    # different file from a Mac.
    try:
        entries = {entry.name.upper(): entry for entry in manifest.iterdir()}
    except OSError:
        entries = {}
    found: list[tuple[str, str]] = []
    seen: set[str] = set()
    for candidate in LICENCE_FILENAMES:
        path = entries.get(candidate.upper())
        if path is None or not path.is_file():
            continue
        try:
            text = path.read_text(encoding="utf-8", errors="replace").strip()
        except OSError:
            continue
        if not text:
            continue
        # Some crates ship LICENSE and LICENSE-MIT with identical content.
        digest = hashlib.sha256(text.encode("utf-8")).hexdigest()
        if digest in seen:
            continue
        seen.add(digest)
        found.append((candidate, text))
    return found


def collect(
    targets: list[str] | None = None,
) -> tuple[dict[str, dict], dict[str, set[str]]]:
    inventory: dict[str, dict] = {}
    targets_for: dict[str, set[str]] = {}
    for target, roots in ARTEFACTS.items():
        if targets is not None and target not in targets:
            continue
        metadata = cargo_metadata(target)
        by_name_version: dict[tuple[str, str], list[dict]] = {}
        for package in metadata["packages"]:
            by_name_version.setdefault(
                (package["name"], package["version"]), []
            ).append(package)
        workspace = {package["name"] for package in metadata["packages"]}
        shipped: set[tuple[str, str]] = set()
        # A root that does not exist on this target is not an error: the
        # Windows installer is not in a macOS resolution.
        present = [root for root in roots if root in workspace]
        for build in release_builds(present):
            shipped |= shipped_name_versions(target, build)
        for name_version in sorted(shipped):
            for package in by_name_version.get(name_version, []):
                if is_first_party(package):
                    continue
                key = f"{package['name']} {package['version']}"
                inventory[key] = package
                targets_for.setdefault(key, set()).add(target)
    return inventory, targets_for


SHORT_TARGET = {
    "aarch64-apple-darwin": "macOS",
    "x86_64-apple-darwin": "macOS",
    "x86_64-unknown-linux-gnu": "Linux",
    "x86_64-pc-windows-msvc": "Windows",
}


def render(inventory: dict[str, dict], targets_for: dict[str, set[str]]) -> str:
    by_licence: dict[str, list[str]] = {}
    for key, package in inventory.items():
        licence = package.get("license") or "UNDECLARED — must be resolved before release"
        by_licence.setdefault(licence, []).append(key)

    lines: list[str] = []
    lines.append(GENERATED_HEADING)
    lines.append("")
    lines.append(
        "Generated by `scripts/generate-third-party-notices.py` from `Cargo.lock`."
    )
    lines.append("Do not edit this section by hand; regenerate it.")
    lines.append("")
    lines.append(
        "Every package below is reachable from a release artefact through a "
        "`normal` or `build` dependency edge on that artefact's target triple, "
        "with build-script and proc-macro dependencies resolved for the machine "
        "the release is built on (macOS on Apple silicon, Linux x86-64, "
        "Windows x86-64 MSVC). "
        "Dev-dependencies are excluded because they are not redistributed. The "
        "*Artefacts* column records which builds carry the package."
    )
    lines.append("")
    lines.append(
        f"{len(inventory)} third-party packages across "
        f"{len(by_licence)} distinct licence expressions."
    )
    lines.append("")

    for licence in sorted(by_licence, key=str.lower):
        lines.append(f"### {licence}")
        lines.append("")
        lines.append("| Package | Version | Artefacts | Source |")
        lines.append("| --- | --- | --- | --- |")
        for key in sorted(by_licence[licence], key=str.lower):
            package = inventory[key]
            artefacts = ", ".join(
                dict.fromkeys(
                    SHORT_TARGET[target] for target in sorted(targets_for[key])
                )
            )
            source = package.get("repository") or "—"
            lines.append(
                f"| `{package['name']}` | {package['version']} | {artefacts} | {source} |"
            )
        lines.append("")

    lines.append("### Full licence texts")
    lines.append("")
    lines.append(
        "The tables above are an inventory. MIT, BSD and Apache-2.0 all require "
        "the copyright notice and permission text to travel with the "
        "distribution, so the texts themselves follow. Packages are grouped by "
        "identical licence text; every package sharing a text is listed against "
        "it, with its own copyright line preserved."
    )
    lines.append("")

    # Group by exact text so 300+ MIT copies collapse to one block each, without
    # losing any individual copyright line.
    texts: dict[str, list[str]] = {}
    missing: list[str] = []
    for key in sorted(inventory, key=str.lower):
        files = licence_files(inventory[key])
        if not files:
            missing.append(key)
            continue
        for _name, text in files:
            texts.setdefault(text, []).append(key)

    for index, (text, packages) in enumerate(
        sorted(texts.items(), key=lambda item: (-len(item[1]), item[1][0].lower())), 1
    ):
        lines.append(f"#### Notice {index} — applies to {len(packages)} package(s)")
        lines.append("")
        lines.append(", ".join(f"`{name}`" for name in sorted(set(packages), key=str.lower)))
        lines.append("")
        lines.append("```text")
        lines.extend(text.splitlines())
        lines.append("```")
        lines.append("")

    if missing:
        lines.append("#### Packages shipping no licence file in their source")
        lines.append("")
        lines.append(
            "These declare a licence in metadata but ship no licence file. The "
            "declared SPDX expression in the inventory above governs; the "
            "canonical text is the standard text for that identifier."
        )
        lines.append("")
        for name in missing:
            lines.append(f"- `{name}` — {inventory[name].get('license') or 'undeclared'}")
        lines.append("")

    return "\n".join(lines).rstrip() + "\n"


def listed_inventory(existing: str) -> dict[tuple[str, str], tuple[str, set[str]]]:
    """The generated tables of a notices file: (name, version) to its
    licence heading and the platforms in its *Artefacts* column."""
    listed: dict[tuple[str, str], tuple[str, set[str]]] = {}
    if GENERATED_HEADING not in existing:
        return listed
    generated = existing.split(GENERATED_HEADING, 1)[1]
    generated = generated.split("### Full licence texts", 1)[0]
    licence = ""
    for line in generated.splitlines():
        if line.startswith("### "):
            licence = line[4:].strip()
            continue
        cells = [cell.strip() for cell in line.strip().strip("|").split("|")]
        if len(cells) != 4 or not cells[0].startswith("`"):
            continue
        name = cells[0].strip("`")
        platforms = {part.strip() for part in cells[2].split(",") if part.strip()}
        listed[(name, cells[1])] = (licence, platforms)
    return listed


def host_only_problems(
    existing: str,
    inventory: dict[str, dict],
    platforms: set[str],
) -> list[str]:
    """What is wrong with `existing` for the artefacts of `platforms`,
    given the inventory resolved for exactly those artefacts."""
    problems: list[str] = []
    listed = listed_inventory(existing)
    if not listed:
        return ["the generated dependency inventory is missing"]
    shipped: set[tuple[str, str]] = set()
    for package in inventory.values():
        name_version = (package["name"], package["version"])
        shipped.add(name_version)
        licence = package.get("license") or "UNDECLARED — must be resolved before release"
        entry = listed.get(name_version)
        if entry is None:
            problems.append(f"`{name_version[0]}` {name_version[1]} is not listed")
            continue
        listed_licence, listed_platforms = entry
        if listed_licence != licence:
            problems.append(
                f"`{name_version[0]}` {name_version[1]} is listed under "
                f"{listed_licence!r}, not {licence!r}"
            )
        for platform in sorted(platforms - listed_platforms):
            problems.append(
                f"`{name_version[0]}` {name_version[1]} is not listed for {platform}"
            )
    for name_version, (_licence, listed_platforms) in sorted(listed.items()):
        if name_version not in shipped and listed_platforms & platforms:
            problems.append(
                f"`{name_version[0]}` {name_version[1]} is listed for "
                f"{', '.join(sorted(listed_platforms & platforms))} but no longer shipped there"
            )
    return problems


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--host-only", action="store_true")
    parser.add_argument(
        "--output", default=str(REPO / "legal" / "THIRD_PARTY_NOTICES.md")
    )
    arguments = parser.parse_args()
    if arguments.host_only and not arguments.check:
        parser.error("--host-only only checks; use it with --check")

    if arguments.host_only:
        _path, host = real_rustc()
        targets = [target for target, build in BUILD_HOST.items() if build == host]
        if not targets:
            print(f"no release artefact is built on {host}", file=sys.stderr)
            return 1
        platforms = {SHORT_TARGET[target] for target in targets}
        # A platform is only fully checked when every target of it is built
        # here (the Intel Deck is cross-built on the Apple-silicon Mac).
        for target, build in BUILD_HOST.items():
            if build != host and SHORT_TARGET[target] in platforms:
                platforms.discard(SHORT_TARGET[target])
        inventory, _targets_for = collect(targets)
        output = Path(arguments.output)
        existing = output.read_text(encoding="utf-8") if output.is_file() else ""
        problems = host_only_problems(existing, inventory, platforms)
        undeclared = sorted(
            key for key, package in inventory.items() if not package.get("license")
        )
        for key in undeclared:
            problems.append(f"`{key}` has no declared licence")
        if problems:
            print(
                f"legal/THIRD_PARTY_NOTICES.md is out of date for {', '.join(sorted(platforms))}; "
                "run scripts/generate-third-party-notices.py on macOS or Linux:",
                file=sys.stderr,
            )
            for problem in problems:
                print(f"    {problem}", file=sys.stderr)
            return 1
        print(
            f"third-party notices current for {', '.join(sorted(platforms))} "
            f"({len(inventory)} packages)"
        )
        return 0

    inventory, targets_for = collect()
    undeclared = sorted(
        key for key, package in inventory.items() if not package.get("license")
    )

    generated = render(inventory, targets_for)
    output = Path(arguments.output)
    existing = output.read_text(encoding="utf-8") if output.is_file() else ""

    if GENERATED_HEADING in existing:
        prefix = existing.split(GENERATED_HEADING)[0].rstrip() + "\n\n"
    else:
        prefix = existing.rstrip() + "\n\n"
    combined = prefix + generated

    if arguments.check:
        if existing != combined:
            print(
                "legal/THIRD_PARTY_NOTICES.md is out of date; run "
                "scripts/generate-third-party-notices.py",
                file=sys.stderr,
            )
            return 1
        if undeclared:
            print("packages without a declared licence:", file=sys.stderr)
            for key in undeclared:
                print(f"    {key}", file=sys.stderr)
            return 1
        return 0

    output.write_text(combined, encoding="utf-8")
    try:
        shown = output.relative_to(REPO)
    except ValueError:
        shown = output
    print(f"wrote {shown}: {len(inventory)} third-party packages")
    if undeclared:
        print("WARNING: packages without a declared licence:", file=sys.stderr)
        for key in undeclared:
            print(f"    {key}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    import os

    if SHIM_HOST in os.environ:
        raise SystemExit(rustc_shim(sys.argv[1:]))
    raise SystemExit(main())
