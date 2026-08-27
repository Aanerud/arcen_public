#!/usr/bin/env python3
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

Usage:
    scripts/generate-third-party-notices.py [--check] [--output PATH]

    --check   exit non-zero if the generated inventory differs from the file on
              disk, without writing. This is what CI runs.
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
    "aarch64-apple-darwin": ["arcen-deck-macos", "arcen-usb-helper"],
    "x86_64-unknown-linux-gnu": [
        "arcen-pier-linux",
        "arcen-pier-linux-installer",
    ],
    "x86_64-pc-windows-msvc": [
        "arcen-pier-windows",
        "arcen-credential-provider",
        "arcen-pier-windows-installer",
    ],
}

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
        ],
        cwd=REPO,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise SystemExit(
            f"cargo metadata failed for {target}:\n{result.stderr.strip()}"
        )
    return json.loads(result.stdout)


def shipped_package_ids(metadata: dict, roots: list[str]) -> set[str]:
    """Walk normal and build edges from the artefact roots.

    Dev-dependency edges are excluded deliberately: a test-only crate is not
    redistributed and listing it would misdescribe what we hand to users.
    """
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}

    by_name: dict[str, list[str]] = {}
    for identifier, package in packages.items():
        by_name.setdefault(package["name"], []).append(identifier)

    frontier = []
    for root in roots:
        if root not in by_name:
            # A root that does not resolve on this target is not an error: the
            # Windows installer does not exist in a macOS resolution.
            continue
        frontier.extend(by_name[root])

    seen: set[str] = set()
    while frontier:
        identifier = frontier.pop()
        if identifier in seen or identifier not in nodes:
            continue
        seen.add(identifier)
        for dependency in nodes[identifier]["deps"]:
            kinds = {kind.get("kind") for kind in dependency.get("dep_kinds", [])}
            # `None` is cargo's spelling of a normal dependency.
            if kinds and not ({None, "build"} & kinds):
                continue
            frontier.append(dependency["pkg"])
    return seen


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
    found: list[tuple[str, str]] = []
    seen: set[str] = set()
    for candidate in LICENCE_FILENAMES:
        path = manifest / candidate
        if not path.is_file():
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


def collect() -> tuple[dict[str, dict], dict[str, set[str]]]:
    inventory: dict[str, dict] = {}
    targets_for: dict[str, set[str]] = {}
    for target, roots in ARTEFACTS.items():
        metadata = cargo_metadata(target)
        packages = {package["id"]: package for package in metadata["packages"]}
        for identifier in shipped_package_ids(metadata, roots):
            package = packages[identifier]
            if is_first_party(package):
                continue
            key = f"{package['name']} {package['version']}"
            inventory[key] = package
            targets_for.setdefault(key, set()).add(target)
    return inventory, targets_for


SHORT_TARGET = {
    "aarch64-apple-darwin": "macOS",
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
        "`normal` or `build` dependency edge on that artefact's target triple. "
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
                SHORT_TARGET[target] for target in sorted(targets_for[key])
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


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    parser.add_argument(
        "--output", default=str(REPO / "legal" / "THIRD_PARTY_NOTICES.md")
    )
    arguments = parser.parse_args()

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
    raise SystemExit(main())
