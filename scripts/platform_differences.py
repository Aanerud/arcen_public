#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS

"""The earned-differences ledger: every difference between products is earned.

A difference between two Piers or two Decks is acceptable only when an
operating-system API or behaviour forces it. ``scripts/ci/platform-differences.toml``
records each earned difference (``[[difference]]``, ``PD-NNN``) with the OS
reason and a code citation, and each known unearned difference as an open gap
(``[[gap]]``, ``GAP-NNN``) with an owner. It generates
``docs/architecture/platform-differences.md``.

``check(root, report)`` (run by ``check_shared_contracts.py``) fails on:

* an OS-conditional ``cfg`` (``target_os``, ``windows``, ``unix``,
  ``target_family``...) in shared/ production code that no ledger row licenses
  through ``cfg_sites``, or a licence whose site count no longer matches;
* a citation (``code``, ``decision``) whose file, directory or symbol is gone;
* a ``[[capability]]`` matrix cell the code contradicts: ``present`` with no
  proof in that product's production code, or a ``PD``/``GAP`` cell where the
  proof now matches (the matrix must be updated when parity lands);
* a ``[[local_copy]]`` detector (a local copy of a shared constant or type)
  matching a file that is not in its ``known`` list;
* a key under a packaged ``pier.json``'s ``platform`` section that no row
  names in ``config``, or a ``config`` pattern that matches no key;
* cross-product clones (contiguous runs of at least ``window`` normalised
  production lines) that grew past ``scripts/ci/clone-baseline.toml``;
* a stale generated document.

Shrinking (a copy deleted, fewer clones) is reported as a note, never a
failure, so consolidation work merges cleanly.
"""

from __future__ import annotations

import fnmatch
import itertools
import json
import re
import tomllib
from collections import Counter
from pathlib import Path
from typing import Protocol

import rust_scan

LEDGER_PATH = "scripts/ci/platform-differences.toml"
CLONE_BASELINE_PATH = "scripts/ci/clone-baseline.toml"
DOC_PATH = "docs/architecture/platform-differences.md"
SCHEMA_VERSION = 1

PIERS = ["hosts/linux", "hosts/windows", "hosts/macos"]
DECKS = ["clients/macos", "clients/windows"]
PRODUCT_NAMES = {
    "hosts/linux": "Linux Pier",
    "hosts/windows": "Windows Pier",
    "hosts/macos": "macOS Pier",
    "hosts/capenc": "capenc",
    "clients/macos": "macOS Deck",
    "clients/windows": "Windows Deck",
    "packaging": "packaging",
    "shared": "shared",
}
CLONE_PRODUCTS = [*rust_scan.PRODUCTS, "packaging"]
# Citations may point anywhere a product or shared crate lives.
SOURCE_ROOTS = [*CLONE_PRODUCTS, "shared"]
TEMPLATES = {
    "hosts/linux": "packaging/linux/arcen-pier.json",
    "hosts/windows": "packaging/windows/pier.json",
    "hosts/macos": "packaging/macos/arcen-pier.json",
}
OWNERS = frozenset(
    {
        "Shared/Architecture",
        "Release/Security",
        "Linux Host",
        "Windows Host",
        "macOS Host",
        "macOS Client",
        "Windows Client",
    }
)
DIFFERENCE_ID = re.compile(r"^PD-\d{3}$")
GAP_ID = re.compile(r"^GAP-\d{3}$")
DIFFERENCE_KEYS = {
    "id", "area", "products", "difference", "earned_by", "code", "decision",
    "cfg_sites", "config",
}
GAP_KEYS = {"id", "area", "products", "difference", "why_open", "owner", "code", "config", "cfg_sites"}
CAPABILITY_KEYS = {"id", "description", "proof", "status", "proofs", "paths"}
LOCAL_COPY_KEYS = {"id", "pattern", "paths", "shared", "gap", "known"}

# An OS-conditional configuration predicate inside cfg(...)/cfg!(...).
CFG_START = re.compile(r"#!?\[\s*cfg(?:_attr)?\s*\(|\bcfg!\s*\(")
OS_PREDICATE = re.compile(r"\b(?:target_os|target_family|target_vendor|target_env)\b|\b(?:windows|unix)\b")
FUNCTION = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)")
BRACKETED = re.compile(r"\[[^\[\]]*\]")


class Report(Protocol):
    failures: list[str]
    notes: list[str]


# --- Loading and schema ---------------------------------------------------------


def load_ledger(root: Path) -> dict:
    return tomllib.loads((root / LEDGER_PATH).read_text())


def _known_product(root: Path, product: str) -> bool:
    return product in PRODUCT_NAMES or (root / product).exists()


def validate(root: Path, ledger: dict, report: Report) -> None:
    if ledger.get("schema_version") != SCHEMA_VERSION:
        report.failures.append(f"{LEDGER_PATH}: schema_version must be {SCHEMA_VERSION}")
    seen: set[str] = set()
    sections = (
        ("difference", DIFFERENCE_ID, DIFFERENCE_KEYS, ("area", "products", "difference", "earned_by", "code")),
        ("gap", GAP_ID, GAP_KEYS, ("area", "products", "difference", "why_open", "owner")),
    )
    for section, pattern, allowed, required in sections:
        for row in ledger.get(section, []):
            ident = row.get("id", "?")
            if not pattern.match(str(ident)):
                report.failures.append(f"{LEDGER_PATH}: [[{section}]] id {ident!r} must match {pattern.pattern}")
            if ident in seen:
                report.failures.append(f"{LEDGER_PATH}: duplicate id {ident}")
            seen.add(ident)
            for key in sorted(set(row) - allowed):
                report.failures.append(f"{LEDGER_PATH}: {ident} has unknown key {key!r}")
            for key in required:
                if not row.get(key):
                    report.failures.append(f"{LEDGER_PATH}: {ident} needs {key!r}")
            for product in row.get("products", []):
                if not _known_product(root, product):
                    report.failures.append(f"{LEDGER_PATH}: {ident} names unknown product {product!r}")
            if section == "gap" and row.get("owner") not in OWNERS:
                report.failures.append(
                    f"{LEDGER_PATH}: {ident} owner {row.get('owner')!r} is not one of {sorted(OWNERS)}"
                )
    for capability in ledger.get("capability", []):
        for key in sorted(set(capability) - CAPABILITY_KEYS):
            report.failures.append(f"{LEDGER_PATH}: capability {capability.get('id')} has unknown key {key!r}")
        products = {*capability.get("status", {}), *capability.get("paths", {}), *capability.get("proofs", {})}
        searched = [*products, *(path for paths in capability.get("paths", {}).values() for path in paths)]
        for path in sorted(p for p in searched if not (root / p).exists()):
            report.failures.append(
                f"{LEDGER_PATH}: capability {capability.get('id')} names {path!r}, which does not "
                "exist; a cell that searches nothing can never be contradicted"
            )
        for product, status in capability.get("status", {}).items():
            if status != "present" and status not in seen:
                report.failures.append(
                    f"{LEDGER_PATH}: capability {capability.get('id')} {product} status {status!r} "
                    "is neither 'present' nor a ledger id"
                )
    for detector in ledger.get("local_copy", []):
        for key in sorted(set(detector) - LOCAL_COPY_KEYS):
            report.failures.append(f"{LEDGER_PATH}: local_copy {detector.get('id')} has unknown key {key!r}")
        for path in detector.get("paths", []):
            if not (root / path).exists():
                report.failures.append(f"{LEDGER_PATH}: local_copy {detector.get('id')} path {path!r} does not exist")
        if detector.get("gap") and detector["gap"] not in seen:
            report.failures.append(f"{LEDGER_PATH}: local_copy {detector.get('id')} names unknown gap {detector['gap']}")


# --- Citations ------------------------------------------------------------------


def _text_for(root: Path, path: Path) -> str:
    relative = path.relative_to(root).as_posix()
    if path.suffix == ".rs":
        for source in _sources_under(root, relative):
            if source.path == relative:
                return source.code_with_strings
        # Test-only or otherwise unlisted Rust file: comments still never count.
        return rust_scan.blank_comments(path.read_text(errors="replace"))
    return path.read_text(errors="replace")


def _sources_under(root: Path, relative: str) -> list[rust_scan.SourceFile]:
    top = next(
        (prefix for prefix in SOURCE_ROOTS if relative == prefix or relative.startswith(prefix + "/")),
        None,
    )
    if top is None:
        return []
    return [
        source
        for source in rust_scan.production_files(root, top)
        if source.path == relative or source.path.startswith(relative.rstrip("/") + "/")
    ]


def symbol_pattern(symbol: str) -> re.Pattern:
    left = r"\b" if re.match(r"\w", symbol) else ""
    right = r"\b" if re.search(r"\w$", symbol) else ""
    return re.compile(left + re.escape(symbol) + right)


def citation_holds(root: Path, path_text: str, symbol: str | None) -> str | None:
    """None when ``path_text`` exists and shows ``symbol``; else the reason."""
    path = root / path_text.split("#", 1)[0]
    if not path.exists():
        return f"{path_text} does not exist"
    if not symbol:
        return None
    pattern = symbol_pattern(symbol)
    if path.is_dir():
        sources = _sources_under(root, path.relative_to(root).as_posix())
        if any(pattern.search(source.code_with_strings) for source in sources):
            return None
        others = (p for p in path.rglob("*") if p.is_file() and p.suffix != ".rs" and "target" not in p.parts)
        if any(pattern.search(p.read_text(errors="replace")) for p in others):
            return None
        return f"{path_text} no longer shows `{symbol}`"
    if pattern.search(_text_for(root, path)):
        return None
    return f"{path_text} no longer shows `{symbol}`"


def check_citations(root: Path, ledger: dict, report: Report) -> None:
    count = 0
    for row in [*ledger.get("difference", []), *ledger.get("gap", [])]:
        for citation in row.get("code", []):
            count += 1
            problem = citation_holds(root, citation["path"], citation.get("symbol"))
            if problem:
                report.failures.append(
                    f"ledger {row['id']}: citation {problem}. Update the citation, or the row "
                    "if the difference has gone (a closed gap is deleted, a moved one re-cited)."
                )
        if row.get("decision"):
            count += 1
            problem = citation_holds(root, row["decision"], None)
            if problem:
                report.failures.append(f"ledger {row['id']}: decision {problem}")
    report.notes.append(f"platform differences: {count} citations hold")


# --- OS-conditional code in shared/ ---------------------------------------------


def _balanced(code: str, open_index: int) -> int:
    depth = 0
    for index in range(open_index, len(code)):
        char = code[index]
        if char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
            if depth == 0:
                return index + 1
    return len(code)


def _enclosing_function(code: str, offset: int) -> str:
    """Name of the innermost ``fn`` whose body contains ``offset``."""
    best = "<module>"
    for match in FUNCTION.finditer(code, 0, offset):
        body = code.find("{", match.end())
        if body == -1 or body > offset:
            continue
        if ";" in BRACKETED.sub("", code[match.end() : body]):
            continue  # a declaration without a body (trait method, extern fn)
        if rust_scan._item_end(code, body) > offset:
            best = match.group(1)
    return best


def shared_cfg_sites(root: Path) -> Counter:
    """(path, enclosing function) -> number of OS-conditional cfg sites in
    shared/ production code."""
    sites: Counter = Counter()
    for source in rust_scan.production_files(root, "shared"):
        code = source.code  # strings blanked: `feature = "windows"` is not a platform
        for match in CFG_START.finditer(code):
            open_index = code.index("(", match.end() - 1)
            if OS_PREDICATE.search(code[open_index : _balanced(code, open_index)]):
                sites[(source.path, _enclosing_function(code, match.start()))] += 1
    return sites


def check_shared_cfg(root: Path, ledger: dict, report: Report) -> None:
    found = shared_cfg_sites(root)
    licensed: Counter = Counter()
    owners: dict[tuple[str, str], list[str]] = {}
    for row in [*ledger.get("difference", []), *ledger.get("gap", [])]:
        for site in row.get("cfg_sites", []):
            key = (site["path"], site["function"])
            licensed[key] += int(site.get("count", 1))
            owners.setdefault(key, []).append(row["id"])
    owner = {key: "+".join(ids) for key, ids in owners.items()}
    for key, count in sorted(found.items()):
        path, function = key
        if key not in licensed:
            report.failures.append(
                f"earned: {path} ({function}) has {count} OS-conditional cfg site(s) that no "
                f"{LEDGER_PATH} row licenses. Name the OS API or behaviour that forces it in a "
                "[[difference]] row's cfg_sites, or make the code portable."
            )
        elif licensed[key] != count:
            report.failures.append(
                f"earned: {path} ({function}) has {count} OS-conditional cfg site(s); "
                f"{owner[key]} licenses {licensed[key]}. Update cfg_sites to the reviewed count."
            )
    for key in sorted(set(licensed) - set(found)):
        report.failures.append(
            f"earned: {owner[key]} licenses cfg sites in {key[0]} ({key[1]}) that no longer exist; "
            "remove them from cfg_sites"
        )
    report.notes.append(f"platform differences: {sum(found.values())} shared cfg sites licensed")


# --- Capability matrix ----------------------------------------------------------


def _proof_sources(root: Path, capability: dict, product: str) -> list[rust_scan.SourceFile]:
    paths = capability.get("paths", {}).get(product)
    if not paths:
        return rust_scan.production_files(root, product)
    return [source for path in paths for source in _sources_under(root, path)]


def check_capabilities(root: Path, ledger: dict, report: Report) -> None:
    cells = 0
    for capability in ledger.get("capability", []):
        for product, status in capability.get("status", {}).items():
            cells += 1
            proof = re.compile(capability.get("proofs", {}).get(product, capability.get("proof", "")))
            if not proof.pattern:
                report.failures.append(f"capability {capability['id']}: {product} has no proof regex")
                continue
            hit = next(
                (
                    source.path
                    for source in _proof_sources(root, capability, product)
                    if proof.search(source.code_with_strings)
                ),
                None,
            )
            if status == "present" and not hit:
                report.failures.append(
                    f"capability {capability['id']}: the matrix says {product} has it, but no "
                    f"production code there matches `{proof.pattern}`. Restore it, or record the "
                    "difference (PD row) or gap (GAP row)."
                )
            elif status != "present" and hit:
                report.failures.append(
                    f"capability {capability['id']}: the matrix says {product} lacks it ({status}), "
                    f"but {hit} matches `{proof.pattern}`. If parity landed, set the cell to "
                    "'present' and close the gap."
                )
    report.notes.append(f"platform differences: {cells} capability cells agree with the code")


# --- Local copies of shared logic -----------------------------------------------


def local_copy_hits(root: Path, detector: dict) -> set[str]:
    pattern = re.compile(detector["pattern"], re.MULTILINE)
    hits = set()
    for prefix in detector["paths"]:
        for source in rust_scan.production_files(root, prefix):
            if pattern.search(source.code):
                hits.add(source.path)
    return hits


def check_local_copies(root: Path, ledger: dict, report: Report) -> None:
    for detector in ledger.get("local_copy", []):
        hits = local_copy_hits(root, detector)
        known = set(detector.get("known", []))
        for path in sorted(hits - known):
            report.failures.append(
                f"local copy {detector['id']}: {path} defines a local copy of shared "
                f"{detector['shared']}. Use the shared item instead."
            )
        gone = sorted(known - hits)
        if gone:
            report.notes.append(
                f"local copy {detector['id']}: no longer in {', '.join(gone)}; "
                f"shrink `known` in {LEDGER_PATH}"
            )


# --- Platform configuration keys ------------------------------------------------


def _leaves(value: object, prefix: str) -> list[str]:
    if isinstance(value, dict) and value:
        return [leaf for key, child in value.items() for leaf in _leaves(child, f"{prefix}.{key}")]
    return [prefix]


def check_platform_config(root: Path, ledger: dict, report: Report) -> None:
    patterns = [
        (pattern, row["id"])
        for row in [*ledger.get("difference", []), *ledger.get("gap", [])]
        for pattern in row.get("config", [])
    ]
    used: set[str] = set()
    keys = 0
    for product, template in TEMPLATES.items():
        path = root / template
        if not path.is_file():
            continue
        platform = json.loads(path.read_text()).get("platform", {})
        for leaf in _leaves(platform, "platform"):
            keys += 1
            name = f"{product}:{leaf}"
            matched = [pattern for pattern, _ in patterns if fnmatch.fnmatchcase(name, pattern)]
            used.update(matched)
            if not matched:
                report.failures.append(
                    f"platform config {template} `{leaf}` is named by no {LEDGER_PATH} row. "
                    "A platform key is a difference: earn it (PD row) or record the gap."
                )
    for pattern, ident in patterns:
        if pattern not in used:
            report.failures.append(f"ledger {ident}: config pattern {pattern!r} matches no platform key")
    report.notes.append(f"platform differences: {keys} platform config keys named")


# --- Clones ---------------------------------------------------------------------


def _pair_key(a: str, b: str) -> str:
    return f"{a} | {b}"


def read_clone_baseline(root: Path) -> dict:
    path = root / CLONE_BASELINE_PATH
    if not path.is_file():
        return {"window": 12, "pairs": {}}
    return tomllib.loads(path.read_text())


def measure_clones(root: Path, window: int) -> dict[str, int]:
    clones = rust_scan.clone_report(root, CLONE_PRODUCTS, window)
    return {
        _pair_key(a, b): clones.pair_lines(a, b)
        for a, b in itertools.combinations(CLONE_PRODUCTS, 2)
        if clones.pair_lines(a, b)
    }


def write_clone_baseline(root: Path, report: Report) -> None:
    window = int(read_clone_baseline(root).get("window", 12))
    pairs = measure_clones(root, window)
    lines = [
        "# SPDX-" + "License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception",
        "# Copyright (C) 2026 TUTTIFRUTTI AS",
        "# Cross-product clones: production lines (comments, tests and trivial lines",
        "# removed) inside contiguous runs of at least `window` identical normalised",
        "# lines shared by two products (the larger side of each pair). Checked by",
        "# scripts/check_shared_contracts.py; a pair may shrink, never grow.",
        "# Regenerate after a reviewed reduction with",
        "# scripts/check_shared_contracts.py --write-clone-baseline.",
        "",
        f"window = {window}",
        "",
        "[pairs]",
        *(f'"{key}" = {value}' for key, value in sorted(pairs.items())),
        "",
    ]
    (root / CLONE_BASELINE_PATH).write_text("\n".join(lines))
    report.notes.append(f"clone baseline written: {len(pairs)} pairs")


def check_clones(root: Path, report: Report) -> None:
    baseline = read_clone_baseline(root)
    recorded = baseline.get("pairs", {})
    current = measure_clones(root, int(baseline.get("window", 12)))
    for key in sorted(set(current) | set(recorded)):
        now, then = current.get(key, 0), int(recorded.get(key, 0))
        if now > then:
            report.failures.append(
                f"clones: {key} share {now} cloned lines, more than the baseline {then} in "
                f"{CLONE_BASELINE_PATH}. Move the copied logic to shared/ instead of copying it."
            )
        elif now < then:
            report.notes.append(
                f"clones: {key} down from {then} to {now} lines; lower the baseline with "
                "--write-clone-baseline"
            )
    report.notes.append(
        f"clones: {sum(current.values())} cross-product clone lines "
        f"(baseline {sum(int(v) for v in recorded.values())})"
    )


# --- The generated document -----------------------------------------------------


def _cell(text: str) -> str:
    return " ".join(str(text).split()).replace("|", "\\|")


def _products(products: list[str]) -> str:
    return ", ".join(PRODUCT_NAMES.get(product, f"`{product}`") for product in products)


def _citations(row: dict) -> str:
    parts = []
    for citation in row.get("code", []):
        symbol = citation.get("symbol")
        parts.append(f"`{citation['path']}`" + (f" (`{symbol}`)" if symbol else ""))
    return "<br>".join(parts) or "—"


def render(ledger: dict, baseline: dict) -> str:
    out = [
        "# Platform differences",
        "",
        "<!-- Generated by scripts/check_shared_contracts.py --write-report from",
        f"     {LEDGER_PATH} and {CLONE_BASELINE_PATH}. Do not edit by",
        "     hand; the default check fails when this file is stale. -->",
        "",
        "Every difference between products (Piers, Decks) must be **earned**: an",
        "operating-system API or behaviour forces it. Anything else is a gap to",
        "close or logic to move to `shared/`. This page is the ledger of both.",
        "",
        "- **Earned differences** name the OS API or behaviour and cite the code.",
        "- **Open gaps** are differences nobody has earned. Each has an owner; it",
        "  closes when parity lands or when it is earned with evidence. \"Not",
        "  implemented yet\" and \"not possible\" are different statements.",
        "- `python3 scripts/check_shared_contracts.py` fails when an OS-conditional",
        "  `cfg` in `shared/` has no row, a citation no longer holds, the",
        "  capability matrix disagrees with the code, a new local copy of shared",
        "  logic appears, a `platform` config key has no row, or cross-product",
        "  clones grow past the baseline.",
        "",
        "To add a difference, edit the ledger and run",
        "`python3 scripts/check_shared_contracts.py --write-report`.",
        "",
        "## Earned differences",
        "",
        "| ID | Area | Products | Difference | Earned by (OS API or behaviour) | Code | Decision |",
        "| --- | --- | --- | --- | --- | --- | --- |",
    ]
    for row in ledger.get("difference", []):
        decision = f"`{row['decision']}`" if row.get("decision") else "—"
        out.append(
            f"| {row['id']} | {_cell(row['area'])} | {_products(row['products'])} | "
            f"{_cell(row['difference'])} | {_cell(row['earned_by'])} | {_citations(row)} | {decision} |"
        )
    out += [
        "",
        "## Open gaps",
        "",
        "Unearned differences. They are not rows of the ledger above: each is",
        "work to do, owned by the role named.",
        "",
        "| ID | Area | Products affected | Difference | Why it is open | Owner | Code |",
        "| --- | --- | --- | --- | --- | --- | --- |",
    ]
    for row in ledger.get("gap", []):
        out.append(
            f"| {row['id']} | {_cell(row['area'])} | {_products(row['products'])} | "
            f"{_cell(row['difference'])} | {_cell(row['why_open'])} | {row['owner']} | {_citations(row)} |"
        )
    capabilities = ledger.get("capability", [])
    if capabilities:
        out += [
            "",
            "## Capability matrix",
            "",
            "Checked against production code: \"yes\" must be provable by the",
            "capability's proof pattern, and a PD or GAP cell must not be.",
        ]
        for title, group in (("Piers", PIERS), ("Decks", DECKS)):
            rows = [c for c in capabilities if any(p in c.get("status", {}) for p in group)]
            if not rows:
                continue
            out += [
                "",
                f"### {title}",
                "",
                "| Capability | " + " | ".join(PRODUCT_NAMES[p] for p in group) + " |",
                "| --- | " + " | ".join("---" for _ in group) + " |",
            ]
            for capability in rows:
                status = capability.get("status", {})
                cells = ["yes" if status.get(p) == "present" else (status.get(p) or "—") for p in group]
                out.append(f"| {_cell(capability['description'])} | " + " | ".join(cells) + " |")
    licensed = [
        (site, row["id"])
        for row in [*ledger.get("difference", []), *ledger.get("gap", [])]
        for site in row.get("cfg_sites", [])
    ]
    if licensed:
        out += [
            "",
            "## OS-conditional code in shared/",
            "",
            "Every `cfg(target_os…)`, `cfg(windows)` or `cfg(unix)` site in shared",
            "production code, by enclosing function.",
            "",
            "| File | Function | Sites | Row |",
            "| --- | --- | ---: | --- |",
        ]
        for site, ident in sorted(licensed, key=lambda item: (item[0]["path"], item[0]["function"])):
            out.append(f"| `{site['path']}` | `{site['function']}` | {site.get('count', 1)} | {ident} |")
    detectors = ledger.get("local_copy", [])
    if detectors:
        out += [
            "",
            "## Local copies of shared logic",
            "",
            "A new match outside the listed files fails the check.",
            "",
            "| Detector | Shared item | Known copies | Gap |",
            "| --- | --- | --- | --- |",
        ]
        for detector in detectors:
            known = "<br>".join(f"`{path}`" for path in detector.get("known", [])) or "—"
            out.append(
                f"| `{detector['id']}` | {_cell(detector['shared'])} | {known} | {detector.get('gap', '—')} |"
            )
    pairs = baseline.get("pairs", {})
    if pairs:
        out += [
            "",
            "## Cross-product clone baseline",
            "",
            f"Production lines inside contiguous runs of at least {baseline.get('window', 12)}",
            "identical normalised lines (comments, tests and lines of only",
            "punctuation removed), on the larger side of each pair. A pair may",
            "shrink, never grow.",
            "",
            "| Products | Cloned lines |",
            "| --- | ---: |",
        ]
        for key, value in sorted(pairs.items(), key=lambda item: -int(item[1])):
            a, b = key.split(" | ")
            out.append(f"| {PRODUCT_NAMES.get(a, a)} ↔ {PRODUCT_NAMES.get(b, b)} | {int(value):,} |")
    return "\n".join(out) + "\n"


def check_document(root: Path, ledger: dict, report: Report, write: bool) -> None:
    text = render(ledger, read_clone_baseline(root))
    path = root / DOC_PATH
    if write:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        report.notes.append(f"wrote {DOC_PATH}")
    elif not path.is_file() or path.read_text() != text:
        report.failures.append(f"{DOC_PATH} is stale; run scripts/check_shared_contracts.py --write-report")


# --- Entry point ----------------------------------------------------------------


def check(root: Path, report: Report, write_report: bool = False, write_clones: bool = False) -> None:
    if not (root / LEDGER_PATH).is_file():
        return
    try:
        ledger = load_ledger(root)
    except tomllib.TOMLDecodeError as error:
        report.failures.append(f"{LEDGER_PATH}: {error}")
        return
    validate(root, ledger, report)
    check_citations(root, ledger, report)
    check_shared_cfg(root, ledger, report)
    check_capabilities(root, ledger, report)
    check_local_copies(root, ledger, report)
    check_platform_config(root, ledger, report)
    if write_clones:
        write_clone_baseline(root, report)
    check_clones(root, report)
    check_document(root, ledger, report, write_report or write_clones)
