#!/usr/bin/env python3
"""Enforce the one rule: policy lives in shared/, and hosts use it.

A shared type nobody calls is a proposal, not parity, and a host that grows
its own copy of a shared policy is where two platforms start to drift. This
check makes both visible and fails CI on them.

It reads ``scripts/ci/shared-contracts.toml`` and checks three things:

* ``[[consumer]]``: each named shared symbol is used in *production* code
  (not in ``#[cfg(test)]`` modules or ``tests/``) of every listed product
  path. Removing a host's last production call fails the build.
* ``[[forbid]]``: a pattern that marks a known duplicate or bypass (a local
  copy of a shared state machine, an ignored transaction result) must not
  appear in production code under the listed paths. With
  ``justified_by_comment = true`` a match is allowed when the line it ends on
  sits directly under a ``//`` comment saying why.
* ``[ratchet]``: Rust files under ``hosts/`` whose code touches no
  operating-system API are portable and so, by the rule, candidates for
  ``shared/``. The committed baseline lists the ones that exist today; a
  new one fails the check until it moves to ``shared/`` or is added to the
  baseline with a reason in review. The baseline may shrink, never grow.

* ``[default_configs]``: the packaged ``pier.json`` templates of every Pier
  carry identical common sections. Only the listed ``ignore`` paths (file
  locations and platform-specific keys) and ``platform`` may differ, so an
  administrator gets the same defaults on every host.

It also prints the Shared Adoption Index trend (see
``shared_adoption_index.py``); that number is informational, not a gate.

Usage: ``scripts/check_shared_contracts.py [--root DIR] [--write-baseline]``
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import tomllib
from dataclasses import dataclass, field
from pathlib import Path

# Anything that means "this file talks to the operating system". A file with
# none of these is portable Rust living in a platform directory.
OS_MARKER = re.compile(
    r"\bunsafe\b|extern\s+\"|\bwindows(_sys)?::|\bobjc2|core_foundation|core_graphics"
    r"|core_video|core_media|\blibc::|\bx11|\bxcb|\bnix::|\bpam\b|cuda|nvenc|nvfbc"
    r"|screencapturekit|\bmetal\b|videotoolbox|video_toolbox|\bcocoa\b|block2|dispatch2"
    r"|iokit|io_kit|pulse|alsa|wasapi|cfg\(\s*(target_os|windows|unix)|std::os::(unix|windows)"
    r"|\bCommand::new|/proc/|/sys/|/dev/|\bdbus|\bsystemd|\bxpc\b|\bwinapi\b"
    r"|std::process|std::fs|std::net",
    re.IGNORECASE,
)

TEST_MODULE = re.compile(
    r"#\[(?:cfg\(test\)|test|tokio::test[^\]]*)\]\s*(?:#\[[^\]]*\]\s*)*"
    r"(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?(?:mod|fn|impl|struct|enum|const|static)\b[^{;]*\{"
)

# A consumer is code that uses the symbol, not a comment, a string, or an
# import left behind after the call was removed.
STRING_LITERAL = re.compile(r'r(#*)"(?:.|\n)*?"\1|b?"(?:\\.|[^"\\])*"', re.DOTALL)
LINE_COMMENT = re.compile(r"//[^\n]*")
BLOCK_COMMENT = re.compile(r"/\*.*?\*/", re.DOTALL)
USE_ITEM = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?use\s+[^;]*;", re.MULTILINE | re.DOTALL)

# An exception to a forbidden pattern names its rule and gives a reason.
JUSTIFICATION = re.compile(r"//\s*shared-contract\s+([\w-]+)\s*:\s*\S")


def code_only(source: str) -> str:
    """Production code with strings, comments and imports blanked out."""
    source = STRING_LITERAL.sub('""', source)
    source = BLOCK_COMMENT.sub(" ", source)
    source = LINE_COMMENT.sub("", source)
    return USE_ITEM.sub("", source)


def strip_test_code(source: str) -> str:
    """Remove test-only items (``#[cfg(test)]``/``#[test]``), brace-matched."""
    out = []
    index = 0
    while True:
        match = TEST_MODULE.search(source, index)
        if not match:
            out.append(source[index:])
            return "".join(out)
        out.append(source[index : match.start()])
        depth = 1
        cursor = match.end()
        while cursor < len(source) and depth:
            char = source[cursor]
            if char == "{":
                depth += 1
            elif char == "}":
                depth -= 1
            cursor += 1
        index = cursor


def production_sources(root: Path, prefix: str) -> dict[str, str]:
    """Production Rust source under ``prefix``, test code removed."""
    base = root / prefix
    sources: dict[str, str] = {}
    if not base.exists():
        return sources
    for path in sorted(base.rglob("*.rs")):
        relative = path.relative_to(root).as_posix()
        parts = set(path.relative_to(root).parts)
        if "target" in parts or "tests" in parts or "benches" in parts:
            continue
        sources[relative] = strip_test_code(path.read_text(errors="replace"))
    return sources


@dataclass
class Report:
    failures: list[str] = field(default_factory=list)
    notes: list[str] = field(default_factory=list)


def check_consumers(root: Path, contracts: dict, report: Report) -> None:
    for contract in contracts.get("consumer", []):
        symbol = re.compile(r"\b" + re.escape(contract["symbol"]) + r"\b")
        for product in contract["products"]:
            sources = production_sources(root, product)
            hits = [path for path, text in sources.items() if symbol.search(code_only(text))]
            if hits:
                report.notes.append(
                    f"consumer {contract['id']}: {product} uses {contract['symbol']} in {hits[0]}"
                )
            else:
                report.failures.append(
                    f"consumer {contract['id']} (gate {contract.get('gate', '?')}): "
                    f"{product} has no production use of {contract['symbol']}. "
                    f"{contract['reason']}"
                )


def justified(text: str, end: int, rule_id: str) -> bool:
    """Whether the comment block directly above the line holding offset
    ``end`` carries ``// shared-contract <rule_id>: <reason>``."""
    lines = text[: text.rfind("\n", 0, end) + 1].split("\n")[:-1]
    for line in reversed(lines):
        stripped = line.strip()
        if not stripped.startswith("//"):
            return False
        marker = JUSTIFICATION.match(stripped)
        if marker and marker.group(1) == rule_id:
            return True
    return False


def check_forbidden(root: Path, contracts: dict, report: Report) -> None:
    for rule in contracts.get("forbid", []):
        pattern = re.compile(rule["pattern"], re.MULTILINE | re.DOTALL)
        for product in rule["paths"]:
            for path, text in production_sources(root, product).items():
                for match in pattern.finditer(text):
                    if rule.get("justified_by_comment") and justified(
                        text, match.end(), rule["id"]
                    ):
                        continue
                    line = text.count("\n", 0, match.start()) + 1
                    report.failures.append(
                        f"forbid {rule['id']} (gate {rule.get('gate', '?')}): "
                        f"{path}:{line} `{match.group(0).strip()[:80]}`. {rule['reason']}"
                    )


def portable_host_files(root: Path, prefixes: list[str]) -> dict[str, int]:
    """Host files whose production code touches no OS API, with line counts."""
    portable = {}
    for prefix in prefixes:
        for path, text in production_sources(root, prefix).items():
            code = "\n".join(
                line for line in text.splitlines() if not line.strip().startswith("//")
            )
            lines = sum(1 for line in code.splitlines() if line.strip())
            if lines and not OS_MARKER.search(code):
                portable[path] = lines
    return portable


def read_baseline(path: Path) -> set[str]:
    if not path.exists():
        return set()
    return {
        line.split()[0]
        for line in path.read_text().splitlines()
        if line.strip() and not line.startswith("#")
    }


def check_ratchet(root: Path, contracts: dict, report: Report, write: bool) -> None:
    ratchet = contracts.get("ratchet")
    if not ratchet:
        return
    baseline_path = root / ratchet["baseline"]
    minimum = int(ratchet.get("minimum_lines", 0))
    portable = {
        path: lines
        for path, lines in portable_host_files(root, ratchet["paths"]).items()
        if lines >= minimum
    }
    if write:
        header = (
            "# Portable Rust under hosts/ that touches no OS API: candidates for shared/.\n"
            "# Generated by scripts/check_shared_contracts.py --write-baseline.\n"
            "# May shrink; a new entry needs a reason in review.\n"
        )
        body = "".join(f"{path} {lines}\n" for path, lines in sorted(portable.items()))
        baseline_path.write_text(header + body)
        report.notes.append(f"ratchet baseline written: {len(portable)} files")
        return
    baseline = read_baseline(baseline_path)
    for path in sorted(set(portable) - baseline):
        report.failures.append(
            f"ratchet: {path} ({portable[path]} lines) is portable host code that is not in "
            f"{ratchet['baseline']}. Move it to shared/, or add it to the baseline with a reason."
        )
    gone = sorted(baseline - set(portable))
    if gone:
        report.notes.append(
            f"ratchet: {len(gone)} baseline entries are no longer portable host code "
            f"(moved or native now); shrink the baseline: {', '.join(gone[:5])}"
        )
    report.notes.append(
        f"ratchet: {len(portable)} portable host files, "
        f"{sum(portable.values())} lines (baseline {len(baseline)} files)"
    )


def _without(value: dict, ignored: list[str]) -> dict:
    trimmed = json.loads(json.dumps(value))
    for dotted in ignored:
        node = trimmed
        *parents, leaf = dotted.split(".")
        for key in parents:
            node = node.get(key, {}) if isinstance(node, dict) else {}
        if isinstance(node, dict):
            node.pop(leaf, None)
    return trimmed


def check_default_configs(root: Path, contracts: dict, report: Report) -> None:
    section = contracts.get("default_configs")
    if not section:
        return
    ignored = ["platform", *section.get("ignore", [])]
    trimmed = {}
    for relative in section["files"]:
        path = root / relative
        try:
            trimmed[relative] = _without(json.loads(path.read_text()), ignored)
        except (OSError, json.JSONDecodeError) as error:
            report.failures.append(f"default config {relative}: {error}")
            return
    reference_name, reference = next(iter(trimmed.items()))
    for name, value in trimmed.items():
        if value != reference:
            keys = sorted(
                key
                for key in set(value) | set(reference)
                if value.get(key) != reference.get(key)
            )
            report.failures.append(
                f"default config {name} differs from {reference_name} in: {', '.join(keys)}"
            )
            return
    report.notes.append(f"default configs: {len(trimmed)} templates share their common sections")


def run(root: Path, write_baseline: bool = False) -> Report:
    contracts = tomllib.loads((root / "scripts/ci/shared-contracts.toml").read_text())
    report = Report()
    check_consumers(root, contracts, report)
    check_forbidden(root, contracts, report)
    check_ratchet(root, contracts, report, write_baseline)
    check_default_configs(root, contracts, report)
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parent.parent)
    parser.add_argument("--write-baseline", action="store_true")
    parser.add_argument("--verbose", action="store_true")
    args = parser.parse_args()
    report = run(args.root, args.write_baseline)
    if args.verbose:
        for note in report.notes:
            print(f"  {note}")
    try:
        sys.path.insert(0, str(Path(__file__).resolve().parent))
        import shared_adoption_index  # noqa: PLC0415

        print("Shared Adoption Index (trend only, not a gate):")
        shared_adoption_index.print_table(args.root)
    except Exception as error:  # noqa: BLE001 - the trend must never fail the gate
        print(f"Shared Adoption Index unavailable: {error}")
    if report.failures:
        print(f"\nshared contracts: {len(report.failures)} failure(s)")
        for failure in report.failures:
            print(f"  FAIL {failure}")
        return 1
    print(f"\nshared contracts: ok ({len(report.notes)} checks)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
