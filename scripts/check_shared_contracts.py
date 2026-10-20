#!/usr/bin/env python3
"""Enforce the one rule: policy lives in shared/, and hosts use it.

A shared type nobody calls is a proposal, not parity, and a host that grows
its own copy of a shared policy is where two platforms start to drift. This
check makes both visible and fails CI on them.

It reads ``shared/<crate>/consumers.json``, ``scripts/ci/validation-profiles.toml``
and ``scripts/ci/shared-contracts.toml`` and checks:

* The shared consumer register: every shared crate (discovered from the
  workspace) has a versioned ``consumers.json`` naming each consumer package,
  its dependency kind, the shared modules it relies on, and the separate
  product paths (stable IDs, real entry points, build profiles, validation
  IDs, owner) that use it. The register is reconciled with ``cargo metadata``:
  an unregistered or stale consumer, a wrong feature/target declaration, an
  unregistered source inclusion or binary embedding, an unknown build profile
  or validation ID, a missing entry point, or a duplicate path ID fails.
* ``production_uses`` in the register: each named shared symbol is used in
  *production* code (not in ``#[cfg(test)]`` modules or ``tests/``) of the
  consumer. Removing a host's last production call fails the build.
* The generated ``docs/architecture/shared-consumers.md`` is up to date.
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

Change-impact mode (``--impact --base REV [--head REV]``) lists, for the
shared crates a change touches, every affected consumer path and the
validation it requires, using the union of the base and head dependency
graphs and registers. With ``--evidence FILE`` it reads revision-bound
results; ``--gate`` fails while a mandatory validation lacks a ``passed``
result for the head revision.

Usage: ``scripts/check_shared_contracts.py [--root DIR] [--write-baseline]
[--write-report] [--impact --base REV [--head REV] [--evidence FILE]... [--gate]]``
"""

from __future__ import annotations

import argparse
import json
import platform
import posixpath
import re
import shlex
import subprocess
import sys
import tomllib
from collections import defaultdict
from collections.abc import Callable, Iterable
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


def check_production_use(root: Path, use: dict, scope: str, report: Report) -> None:
    """A named shared symbol is called from production code under ``scope``."""
    symbol = re.compile(r"\b" + re.escape(use["symbol"]) + r"\b")
    sources = production_sources(root, scope)
    hits = [path for path, text in sources.items() if symbol.search(code_only(text))]
    if hits:
        report.notes.append(f"consumer {use['id']}: {scope} uses {use['symbol']} in {hits[0]}")
    else:
        report.failures.append(
            f"consumer {use['id']} (gate {use.get('gate', '?')}): "
            f"{scope} has no production use of {use['symbol']}. {use['reason']}"
        )


def check_consumers(root: Path, contracts: dict, report: Report) -> None:
    if contracts.get("consumer"):
        report.failures.append(
            "scripts/ci/shared-contracts.toml: [[consumer]] rules moved to the "
            "production_uses of shared/<crate>/consumers.json; declare them there only"
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


# --- The shared consumer register --------------------------------------------
#
# Every shared crate carries shared/<crate>/consumers.json: who depends on it,
# the separate product paths that dependency serves, and the reviewed
# validation IDs (scripts/ci/validation-profiles.toml) that protect each path.
# Cargo proves dependency edges; the register names paths and validation. The
# register never holds commands or results: commands come only from catalogue
# templates, and results live in revision-bound evidence files.

REGISTRY_FILE = "consumers.json"
REGISTRY_SCHEMA_VERSION = 1
CATALOGUE_PATH = "scripts/ci/validation-profiles.toml"
CATALOGUE_SCHEMA_VERSION = 1
EVIDENCE_SCHEMA_VERSION = 1
REPORT_PATH = "docs/architecture/shared-consumers.md"
CARGO_KINDS = ("normal", "build", "dev")
# source: compiles in a shared file; embedded: packs a consumer's binary;
# indirect: reaches the crate only through a non-shared helper package.
EDGE_KINDS = (*CARGO_KINDS, "source", "embedded", "indirect")
OWNERS = frozenset(
    {
        "Shared/Architecture",
        "Release/Security",
        "Linux Host",
        "Windows Host",
        "macOS Host",
        "macOS Client",
    }
)
TARGET_OSES = ("any", "linux", "macos", "windows")
VALIDATION_KINDS = frozenset({"schema", "unit", "lint", "build", "hardware", "evidence"})
RESULTS = ("passed", "failed", "blocked", "not_run")
PLACEHOLDERS = frozenset(
    {"{package}", "{features}", "{target}", "{module}", "{script}", "{filter}"}
)
IDENT = re.compile(r"^[a-z0-9]+(?:-[a-z0-9]+)*$")
FEATURE = re.compile(r"^[A-Za-z0-9_][A-Za-z0-9_+.-]*$")
RUST_TARGET = re.compile(r"^[a-z0-9_]+(?:-[a-z0-9_]+){2,3}$")
TEST_FILTER = re.compile(r"^[A-Za-z0-9_:]+$")
REVISION = re.compile(r"^[0-9a-f]{40}$")
EMBED_SITE = re.compile(r"\binclude_bytes!\s*\(\s*env!\s*\(")
INCLUDE_SITE = re.compile(
    r"\b(?:include_str|include_bytes|include)!\s*\(\s*"
    r'(?P<manifest>concat!\s*\(\s*env!\s*\(\s*"CARGO_MANIFEST_DIR"\s*\)\s*,\s*)?'
    r'"(?P<path>[^"]+)"'
)
PATH_ATTR = re.compile(r'#\[path\s*=\s*"(?P<path>[^"]+)"\s*\]')
# Changes to these inputs alter how every shared crate builds.
GLOBAL_BUILD_INPUTS = re.compile(r"^(?:rust-toolchain(?:\.toml)?|\.cargo/.*)$")

Reader = Callable[[str], "str | None"]


def fs_reader(root: Path) -> Reader:
    def read(relative: str) -> str | None:
        path = root / relative
        return path.read_text(errors="replace") if path.is_file() else None

    return read


def git(root: Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-C", str(root), *args], capture_output=True, text=True, check=True
    ).stdout


def git_lister(root: Path, revision: str) -> Callable[[str], list[str]]:
    def list_files(directory: str) -> list[str]:
        result = subprocess.run(
            ["git", "-C", str(root), "ls-tree", "-r", "--name-only", revision, "--", directory],
            capture_output=True,
            text=True,
        )
        return sorted(line for line in result.stdout.splitlines() if line)

    return list_files


def git_reader(root: Path, revision: str) -> Reader:
    cache: dict[str, str | None] = {}

    def read(relative: str) -> str | None:
        if relative not in cache:
            result = subprocess.run(
                ["git", "-C", str(root), "show", f"{revision}:{relative}"],
                capture_output=True,
                text=True,
            )
            cache[relative] = result.stdout if result.returncode == 0 else None
        return cache[relative]

    return read


def _reject_duplicate_keys(pairs: list[tuple[str, object]]) -> dict:
    seen: dict[str, object] = {}
    for key, value in pairs:
        if key in seen:
            raise ValueError(f"duplicate key {key!r}")
        seen[key] = value
    return seen


def parse_json(text: str) -> object:
    return json.loads(text, object_pairs_hook=_reject_duplicate_keys)


# --- The Cargo graph -----------------------------------------------------------


@dataclass(frozen=True)
class Dependency:
    """One first-party dependency declaration, as Cargo resolves it."""

    package: str
    crate: str
    kind: str
    target: str | None = None
    optional: bool = False
    features: tuple[str, ...] = ()


@dataclass
class Workspace:
    dirs: dict[str, str]
    features: dict[str, dict[str, list[str]]]
    dependencies: list[Dependency]

    @property
    def shared(self) -> set[str]:
        return {name for name, path in self.dirs.items() if path.startswith("shared/")}

    def package_of(self, relative: str) -> str | None:
        """The innermost workspace package that owns a repository file."""
        best = None
        for name, directory in self.dirs.items():
            if relative.startswith(directory + "/") and (
                best is None or len(directory) > len(self.dirs[best])
            ):
                best = name
        return best

    def edges(self, kinds: Iterable[str] = CARGO_KINDS) -> set[tuple[str, str]]:
        wanted = set(kinds)
        return {(dep.package, dep.crate) for dep in self.dependencies if dep.kind in wanted}


def reverse_closure(start: Iterable[str], edges: Iterable[tuple[str, str]]) -> set[str]:
    """``start`` plus everything that depends on it, directly or not."""
    dependents: dict[str, set[str]] = defaultdict(set)
    for dependent, dependency in edges:
        dependents[dependency].add(dependent)
    seen = set(start)
    stack = list(seen)
    while stack:
        for dependent in dependents[stack.pop()]:
            if dependent not in seen:
                seen.add(dependent)
                stack.append(dependent)
    return seen


def load_cargo_metadata(root: Path) -> dict:
    """The same source scripts/ci/check-workspace-boundaries.sh reads."""
    result = subprocess.run(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
        cwd=root,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(f"cargo metadata failed: {result.stderr.strip()[:400]}")
    return json.loads(result.stdout)


def workspace_from_metadata(metadata: dict) -> Workspace:
    root = metadata["workspace_root"].replace("\\", "/").rstrip("/")
    members = set(metadata.get("workspace_members") or [])

    def relative(path: str) -> str:
        path = path.replace("\\", "/")
        return path[len(root) + 1 :] if path.startswith(root + "/") else path

    packages = [p for p in metadata["packages"] if not members or p["id"] in members]
    dirs = {p["name"]: posixpath.dirname(relative(p["manifest_path"])) for p in packages}
    by_dir = {directory: name for name, directory in dirs.items()}
    dependencies = []
    for package in packages:
        for dep in package.get("dependencies", []):
            crate = by_dir.get(relative(dep["path"])) if dep.get("path") else None
            if crate:
                dependencies.append(
                    Dependency(
                        package["name"],
                        crate,
                        dep.get("kind") or "normal",
                        dep.get("target"),
                        bool(dep.get("optional")),
                        tuple(sorted(set(dep.get("features") or []))),
                    )
                )
    features = {p["name"]: {k: list(v) for k, v in p.get("features", {}).items()} for p in packages}
    return Workspace(dirs, features, dependencies)


def workspace_from_manifests(read: Reader) -> Workspace:
    """The first-party graph from Cargo.toml files alone.

    Used for revisions that are not checked out (the change-impact base). The
    default check proves it agrees with ``cargo metadata`` on the working tree.
    """
    text = read("Cargo.toml")
    if text is None:
        raise ValueError("no root Cargo.toml")
    workspace = tomllib.loads(text).get("workspace", {})
    shared_deps = workspace.get("dependencies", {})
    manifests: dict[str, dict] = {}
    for member in workspace.get("members", []):
        if any(char in member for char in "*?["):
            raise ValueError(f"glob workspace member {member!r} is not supported")
        member_text = read(f"{member}/Cargo.toml")
        if member_text is None:
            raise ValueError(f"workspace member {member} has no Cargo.toml")
        manifests[member] = tomllib.loads(member_text)
    dirs = {manifest["package"]["name"]: member for member, manifest in manifests.items()}
    by_dir = {directory: name for name, directory in dirs.items()}
    dependencies = []
    features: dict[str, dict[str, list[str]]] = {}
    for member, manifest in manifests.items():
        name = manifest["package"]["name"]
        package_features = {k: list(v) for k, v in manifest.get("features", {}).items()}
        tables: list[tuple[dict, str, str | None]] = []
        for scope, target in [(manifest, None)] + [
            (table, cfg) for cfg, table in manifest.get("target", {}).items()
        ]:
            for key, kind in (
                ("dependencies", "normal"),
                ("build-dependencies", "build"),
                ("dev-dependencies", "dev"),
            ):
                tables.append((scope.get(key, {}), kind, target))
        optional_keys = set()
        for table, kind, target in tables:
            for key, spec in table.items():
                if not isinstance(spec, dict):
                    continue
                crate_name = spec.get("package", key)
                extra = list(spec.get("features", []))
                if spec.get("workspace"):
                    inherited = shared_deps.get(key)
                    if not isinstance(inherited, dict) or "path" not in inherited:
                        continue
                    location = posixpath.normpath(inherited["path"])
                    crate_name = inherited.get("package", crate_name)
                    extra = list(inherited.get("features", [])) + extra
                elif "path" in spec:
                    location = posixpath.normpath(posixpath.join(member, spec["path"]))
                else:
                    continue
                crate = by_dir.get(location)
                if crate is None or crate != crate_name:
                    continue
                optional = bool(spec.get("optional", False))
                if optional:
                    optional_keys.add(key)
                dependencies.append(
                    Dependency(name, crate, kind, target, optional, tuple(sorted(set(extra))))
                )
        for key in optional_keys:
            if not any(f"dep:{key}" in values for values in package_features.values()):
                package_features.setdefault(key, [f"dep:{key}"])
        features[name] = package_features
    return Workspace(dirs, features, dependencies)


def compare_workspaces(metadata: Workspace, manifests: Workspace) -> list[str]:
    differences = []
    if metadata.dirs != manifests.dirs:
        differences.append("package set differs between cargo metadata and the manifest reader")
    for dep in sorted(set(metadata.dependencies) - set(manifests.dependencies), key=repr):
        differences.append(f"cargo metadata has {dep} that the manifest reader missed")
    for dep in sorted(set(manifests.dependencies) - set(metadata.dependencies), key=repr):
        differences.append(f"the manifest reader has {dep} that cargo metadata does not")
    return differences


def expected_cargo_records(workspace: Workspace, crate: str) -> dict[tuple[str, str], dict]:
    """What a register must say about each direct Cargo consumer of ``crate``."""
    grouped: dict[tuple[str, str], list[Dependency]] = defaultdict(list)
    for dep in workspace.dependencies:
        if dep.crate == crate:
            grouped[(dep.package, dep.kind)].append(dep)
    records = {}
    for (package, kind), deps in grouped.items():
        records[(package, kind)] = {
            "targets": sorted({dep.target or "*" for dep in deps}),
            "features": sorted({feature for dep in deps for feature in dep.features}),
            "optional": all(dep.optional for dep in deps),
        }
    for package in {package for package, _ in records}:
        kinds = [kind for kind in CARGO_KINDS if (package, kind) in records]
        forwarded: dict[str, list[str]] = {}
        enabled_by = []
        for feature, values in sorted(workspace.features.get(package, {}).items()):
            crate_features = sorted(
                value.split("/", 1)[1]
                for value in values
                if value.startswith((f"{crate}/", f"{crate}?/"))
            )
            if crate_features:
                forwarded[feature] = crate_features
            if any(
                value in (f"dep:{crate}", crate) or value.startswith(f"{crate}/")
                for value in values
            ):
                enabled_by.append(feature)
        primary = records[(package, kinds[0])]
        primary["forwarded_features"] = forwarded
        for kind in kinds:
            records[(package, kind)].setdefault("forwarded_features", {})
            records[(package, kind)]["enabled_by"] = (
                sorted(enabled_by) if records[(package, kind)]["optional"] else []
            )
    return records


def strip_rust_comments(text: str) -> str:
    text = STRING_LITERAL.sub('""', text)
    text = BLOCK_COMMENT.sub(" ", text)
    return LINE_COMMENT.sub("", text)


def rust_files(read_root: Path, workspace: Workspace, package: str) -> list[str]:
    """Rust files that belong to ``package`` itself, not a nested package."""
    base = read_root / workspace.dirs[package]
    files = []
    if not base.exists():
        return files
    for path in sorted(base.rglob("*.rs")):
        relative = path.relative_to(read_root).as_posix()
        if "/target/" in f"/{relative}" or workspace.package_of(relative) != package:
            continue
        files.append(relative)
    return files


def include_targets(text: str, relative: str, package_dir: str) -> list[str]:
    """Repository paths a Rust file compiles in (``include*!`` and ``#[path]``).

    ``concat!(env!("CARGO_MANIFEST_DIR"), "/x")`` resolves against the
    package directory; a plain literal against the including file's directory.
    """
    text = BLOCK_COMMENT.sub(" ", LINE_COMMENT.sub("", text))
    here = posixpath.dirname(relative)
    found = [
        posixpath.normpath(
            posixpath.join(package_dir, m["path"].lstrip("/"))
            if m["manifest"]
            else posixpath.join(here, m["path"])
        )
        for m in INCLUDE_SITE.finditer(text)
    ]
    found += [posixpath.normpath(posixpath.join(here, m["path"])) for m in PATH_ATTR.finditer(text)]
    return found


def scan_non_cargo_edges(
    root: Path, workspace: Workspace, assets: dict[str, str]
) -> tuple[dict[tuple[str, str], dict[str, set[str]]], dict[str, set[str]]]:
    """Source inclusions of shared files and binary-embedding sites.

    Returns ``{(package, crate): {included file: {including files}}}`` for
    every file a package compiles in from a shared crate (or a registered
    shared asset), and ``{package: {files with include_bytes!(env!(..))}}``.
    """
    includes: dict[tuple[str, str], dict[str, set[str]]] = defaultdict(lambda: defaultdict(set))
    embeds: dict[str, set[str]] = defaultdict(set)
    shared_dirs = {crate: workspace.dirs[crate] for crate in sorted(workspace.shared)}
    for package in sorted(workspace.dirs):
        for relative in rust_files(root, workspace, package):
            raw = (root / relative).read_text(errors="replace")
            if EMBED_SITE.search(BLOCK_COMMENT.sub(" ", LINE_COMMENT.sub("", raw))):
                embeds[package].add(relative)
            for target in include_targets(raw, relative, workspace.dirs[package]):
                owner = assets.get(target) or next(
                    (
                        crate
                        for crate, directory in shared_dirs.items()
                        if target.startswith(directory + "/")
                    ),
                    None,
                )
                if owner and owner != package:
                    includes[(package, owner)][target].add(relative)
    return includes, embeds


# --- The validation catalogue -----------------------------------------------------


@dataclass
class Catalogue:
    templates: dict[str, dict]
    profiles: dict[str, dict]
    validations: dict[str, dict]
    registry_validation: str | None

    def resolved(self, validation_id: str) -> dict:
        """A validation with its build profile's package/target/features applied."""
        validation = dict(self.validations[validation_id])
        profile = self.profiles.get(validation.get("build_profile", ""), {})
        for key in ("package", "target_os", "rust_target", "features"):
            if key not in validation and key in profile:
                validation[key] = profile[key]
        validation.setdefault("features", [])
        validation.setdefault("mandatory", True)
        return validation

    def command(self, validation_id: str) -> list[str]:
        """The argv for a validation, built only from a catalogue template."""
        validation = self.resolved(validation_id)
        argv = []
        for token in self.templates[validation["template"]]["argv"]:
            if token == "{features}":
                if validation["features"]:
                    argv += ["--features", ",".join(validation["features"])]
            elif token == "{target}":
                if validation.get("rust_target"):
                    argv += ["--target", validation["rust_target"]]
            elif token in PLACEHOLDERS:
                argv.append(str(validation[token[1:-1]]))
            else:
                argv.append(token)
        return argv


def load_catalogue(read: Reader) -> tuple[Catalogue | None, list[str]]:
    text = read(CATALOGUE_PATH)
    if text is None:
        return None, [f"{CATALOGUE_PATH} is missing"]
    try:
        doc = tomllib.loads(text)
    except tomllib.TOMLDecodeError as error:
        return None, [f"{CATALOGUE_PATH}: {error}"]
    errors = []
    if doc.get("schema_version") != CATALOGUE_SCHEMA_VERSION:
        errors.append(f"{CATALOGUE_PATH}: schema_version must be {CATALOGUE_SCHEMA_VERSION}")
    catalogue = Catalogue(
        templates=doc.get("template", {}),
        profiles=doc.get("build_profile", {}),
        validations=doc.get("validation", {}),
        registry_validation=doc.get("registry_validation"),
    )
    return catalogue, errors


def validate_catalogue(catalogue: Catalogue, workspace: Workspace, root: Path) -> list[str]:
    where = CATALOGUE_PATH
    errors = []

    def check_package_features(owner: str, package: str | None, features: object) -> None:
        if package is not None and package not in workspace.dirs:
            errors.append(f"{where}: {owner} names unknown package {package!r}")
            return
        if not isinstance(features, list) or not all(
            isinstance(f, str) and FEATURE.match(f) for f in features
        ):
            errors.append(f"{where}: {owner} features must be a list of feature names")
            return
        if package is not None:
            unknown = sorted(set(features) - set(workspace.features.get(package, {})))
            if unknown:
                errors.append(f"{where}: {owner} enables unknown {package} features {unknown}")

    for template_id, template in catalogue.templates.items():
        argv = template.get("argv")
        if not IDENT.match(template_id) or not isinstance(argv, list):
            errors.append(f"{where}: template {template_id!r} needs an ID and an argv list")
            continue
        for token in argv:
            if not isinstance(token, str) or ("{" in token and token not in PLACEHOLDERS):
                errors.append(f"{where}: template {template_id} has unsupported token {token!r}")
    for profile_id, profile in catalogue.profiles.items():
        owner = f"build profile {profile_id}"
        if not IDENT.match(profile_id):
            errors.append(f"{where}: {owner} is not a stable kebab-case ID")
        if profile.get("target_os") not in TARGET_OSES:
            errors.append(f"{where}: {owner} target_os must be one of {TARGET_OSES}")
        if "rust_target" in profile and not RUST_TARGET.match(str(profile["rust_target"])):
            errors.append(f"{where}: {owner} has an invalid rust_target")
        if not isinstance(profile.get("description"), str):
            errors.append(f"{where}: {owner} needs a description")
        check_package_features(owner, profile.get("package"), profile.get("features", []))
    if catalogue.registry_validation not in catalogue.validations:
        errors.append(f"{where}: registry_validation must name a catalogue validation")
    for validation_id in catalogue.validations:
        owner = f"validation {validation_id}"
        raw = catalogue.validations[validation_id]
        if not IDENT.match(validation_id):
            errors.append(f"{where}: {owner} is not a stable kebab-case ID")
        if raw.get("build_profile") is not None and raw["build_profile"] not in catalogue.profiles:
            errors.append(f"{where}: {owner} names unknown build profile {raw['build_profile']!r}")
            continue
        validation = catalogue.resolved(validation_id)
        if validation.get("kind") not in VALIDATION_KINDS:
            errors.append(f"{where}: {owner} kind must be one of {sorted(VALIDATION_KINDS)}")
        if validation.get("target_os") not in TARGET_OSES:
            errors.append(f"{where}: {owner} target_os must be one of {TARGET_OSES}")
        if validation.get("owner") not in OWNERS:
            errors.append(f"{where}: {owner} owner must be one of {sorted(OWNERS)}")
        if not isinstance(validation.get("description"), str):
            errors.append(f"{where}: {owner} needs a description")
        if not isinstance(validation["mandatory"], bool):
            errors.append(f"{where}: {owner} mandatory must be true or false")
        if "rust_target" in validation and not RUST_TARGET.match(str(validation["rust_target"])):
            errors.append(f"{where}: {owner} has an invalid rust_target")
        check_package_features(owner, validation.get("package"), validation["features"])
        template = catalogue.templates.get(validation.get("template", ""))
        if template is None:
            errors.append(f"{where}: {owner} names unknown template {validation.get('template')!r}")
            continue
        argv = template.get("argv") or []
        if validation.get("kind") == "hardware" and argv:
            errors.append(f"{where}: {owner} is hardware; its evidence is recorded, not run here")
        for token in argv:
            if token in PLACEHOLDERS and token not in ("{features}", "{target}"):
                value = validation.get(token[1:-1])
                if not isinstance(value, str) or not value:
                    errors.append(f"{where}: {owner} template needs {token}")
                elif token in ("{module}", "{script}") and not (
                    value.startswith(("scripts/", "packaging/")) and (root / value).is_file()
                ):
                    errors.append(f"{where}: {owner} {token} {value!r} is not a reviewed script")
                elif token == "{filter}" and not TEST_FILTER.match(value):
                    errors.append(f"{where}: {owner} filter {value!r} is not a test path")
    return errors


# --- Loading and checking the register ----------------------------------------------


def load_registries(read: Reader, workspace: Workspace) -> tuple[dict[str, dict], list[str]]:
    registries, errors = {}, []
    for crate in sorted(workspace.shared):
        relative = f"{workspace.dirs[crate]}/{REGISTRY_FILE}"
        text = read(relative)
        if text is None:
            errors.append(f"{relative}: shared crate {crate} has no consumer register")
            continue
        try:
            doc = parse_json(text)
        except ValueError as error:
            errors.append(f"{relative}: {error}")
            continue
        if not isinstance(doc, dict):
            errors.append(f"{relative}: the register must be a JSON object")
            continue
        registries[crate] = doc
    return registries, errors


def registry_assets(registries: dict[str, dict]) -> dict[str, str]:
    return {
        asset: crate
        for crate, doc in registries.items()
        for asset in doc.get("assets", [])
        if isinstance(asset, str)
    }


def _strings(value: object) -> bool:
    return isinstance(value, list) and all(isinstance(item, str) and item for item in value)


def module_exists(root: Path, crate_dir: str, contract: str) -> bool:
    """``crate`` (the crate root) or a ``::`` module path inside the crate."""
    if contract == "crate":
        return (root / crate_dir / "src/lib.rs").is_file()
    parts = contract.split("::")
    if not all(re.match(r"^[a-z_][a-z0-9_]*$", part) for part in parts):
        return False
    source = root / crate_dir / "src"
    if (source / f"{'/'.join(parts)}.rs").is_file() or (
        source / "/".join(parts) / "mod.rs"
    ).is_file():
        return True
    parent = parts[:-1]
    parent_files = (
        [source / "lib.rs"]
        if not parent
        else [source / f"{'/'.join(parent)}.rs", source / "/".join(parent) / "mod.rs"]
    )
    inline = re.compile(r"\bmod\s+" + re.escape(parts[-1]) + r"\s*\{")
    return any(f.is_file() and inline.search(f.read_text(errors="replace")) for f in parent_files)


REGISTRY_KEYS = {
    "schema_version",
    "crate",
    "owner",
    "description",
    "validation",
    "assets",
    "consumers",
}
CONSUMER_KEYS = {
    "package",
    "dependency_kind",
    "contracts",
    "targets",
    "features",
    "optional",
    "enabled_by",
    "forwarded_features",
    "includes",
    "via",
    "production_uses",
    "paths",
    "notes",
}
PATH_KEYS = {"id", "description", "entry_points", "build_profiles", "validation", "owner"}
USE_KEYS = {"id", "gate", "symbol", "reason", "scope"}


def validate_registry(
    crate: str,
    doc: dict,
    workspace: Workspace,
    catalogue: Catalogue,
    root: Path,
) -> list[str]:
    """Schema, references and file existence for one register."""
    where = f"{workspace.dirs[crate]}/{REGISTRY_FILE}"
    errors = []
    unknown = sorted(set(doc) - REGISTRY_KEYS)
    if unknown:
        errors.append(f"{where}: unknown keys {unknown}")
    if doc.get("schema_version") != REGISTRY_SCHEMA_VERSION:
        errors.append(f"{where}: schema_version must be {REGISTRY_SCHEMA_VERSION}")
    if doc.get("crate") != crate:
        errors.append(f"{where}: crate must be {crate!r}")
    if doc.get("owner") not in OWNERS:
        errors.append(f"{where}: owner must be one of {sorted(OWNERS)}")
    for validation_id in doc.get("validation", []) if _strings(doc.get("validation", [])) else []:
        if validation_id not in catalogue.validations:
            errors.append(f"{where}: unknown validation ID {validation_id!r}")
    if not _strings(doc.get("validation", [])) or not doc.get("validation"):
        errors.append(f"{where}: validation must list the crate's own validation IDs")
    if not _strings(doc.get("assets", [])):
        errors.append(f"{where}: assets must be a list of repository files")
    else:
        for asset in doc.get("assets", []):
            if not (root / asset).is_file():
                errors.append(f"{where}: asset {asset} does not exist")
    consumers = doc.get("consumers")
    if not isinstance(consumers, list):
        return errors + [f"{where}: consumers must be a list"]
    seen_records = set()
    for index, record in enumerate(consumers):
        label = f"{where}: consumers[{index}]"
        if not isinstance(record, dict):
            errors.append(f"{label} must be an object")
            continue
        package, kind = record.get("package"), record.get("dependency_kind")
        label = f"{where}: consumer {package} ({kind})"
        unknown = sorted(set(record) - CONSUMER_KEYS)
        if unknown:
            errors.append(f"{label}: unknown keys {unknown}")
        if package not in workspace.dirs:
            errors.append(f"{label}: package {package!r} is not in the workspace")
            continue
        if package == crate:
            errors.append(f"{label}: a crate does not consume itself")
        if kind not in EDGE_KINDS:
            errors.append(f"{label}: dependency_kind must be one of {EDGE_KINDS}")
            continue
        if (package, kind) in seen_records:
            errors.append(f"{label}: registered twice")
        seen_records.add((package, kind))
        contracts = record.get("contracts", [])
        if not _strings(contracts) or not contracts:
            errors.append(f"{label}: contracts must name the shared modules relied on")
        else:
            for contract in contracts:
                if not module_exists(root, workspace.dirs[crate], contract):
                    errors.append(f"{label}: contract {contract!r} is not a {crate} module")
        for key in ("targets", "features", "enabled_by", "includes", "via"):
            if key in record and not _strings(record[key]):
                errors.append(f"{label}: {key} must be a list of strings")
        forwarded = record.get("forwarded_features", {})
        if not isinstance(forwarded, dict) or not all(_strings(v) for v in forwarded.values()):
            errors.append(f"{label}: forwarded_features maps a consumer feature to crate features")
        if kind in CARGO_KINDS and ("via" in record or "includes" in record):
            errors.append(f"{label}: via/includes belong to embedded/source records only")
        if kind in ("embedded", "indirect") and not record.get("via"):
            errors.append(f"{label}: an {kind} record names the packages it goes through in via")
        if kind == "source" and not record.get("includes"):
            errors.append(f"{label}: a source record lists the included shared files")
        if kind not in CARGO_KINDS and any(
            key in record
            for key in ("targets", "features", "optional", "enabled_by", "forwarded_features")
        ):
            errors.append(f"{label}: Cargo declaration fields belong to normal/build/dev records")
        for use in record.get("production_uses", []):
            if (
                not isinstance(use, dict)
                or set(use) - USE_KEYS
                or not all(
                    isinstance(use.get(key), str) and use.get(key)
                    for key in ("id", "symbol", "reason")
                )
            ):
                errors.append(f"{label}: production_uses entries need id, symbol and reason")
            elif kind not in CARGO_KINDS:
                errors.append(f"{label}: production_uses belong to Cargo consumers")
            elif "scope" in use and not (root / use["scope"]).is_dir():
                errors.append(f"{label}: production use {use['id']} scope does not exist")
        paths = record.get("paths", [])
        if not isinstance(paths, list):
            errors.append(f"{label}: paths must be a list")
            continue
        if package in workspace.shared:
            if paths:
                errors.append(
                    f"{label}: a shared consumer has no product paths; impact flows through "
                    f"its own register"
                )
            continue
        if not paths:
            errors.append(f"{label}: a product consumer names the paths that use {crate}")
        seen_paths = set()
        for path in paths:
            if not isinstance(path, dict):
                errors.append(f"{label}: paths entries must be objects")
                continue
            path_id = path.get("id")
            path_label = f"{label} path {path_id}"
            if set(path) - PATH_KEYS:
                errors.append(f"{path_label}: unknown keys {sorted(set(path) - PATH_KEYS)}")
            if not isinstance(path_id, str) or not IDENT.match(path_id):
                errors.append(f"{label}: path id {path_id!r} is not a stable kebab-case ID")
                continue
            if path_id in seen_paths:
                errors.append(f"{path_label}: duplicate path ID in one consumer record")
            seen_paths.add(path_id)
            if path.get("owner") not in OWNERS:
                errors.append(f"{path_label}: owner must be one of {sorted(OWNERS)}")
            for key, known, what in (
                ("build_profiles", catalogue.profiles, "build profile"),
                ("validation", catalogue.validations, "validation ID"),
            ):
                values = path.get(key)
                if not _strings(values) or not values:
                    errors.append(f"{path_label}: {key} must be a non-empty list")
                    continue
                for value in values:
                    if value not in known:
                        errors.append(f"{path_label}: unknown {what} {value!r}")
                if len(set(values)) != len(values):
                    errors.append(f"{path_label}: {key} lists an ID twice")
            entry_points = path.get("entry_points")
            if not _strings(entry_points) or not entry_points:
                errors.append(f"{path_label}: entry_points must be a non-empty list")
                continue
            for entry in entry_points:
                if not (root / entry).is_file():
                    errors.append(f"{path_label}: entry point {entry} does not exist")
    return errors


def _entry_shows(root: Path, entry_points: list[str], pattern: re.Pattern) -> bool:
    for entry in entry_points:
        path = root / entry
        if path.suffix == ".rs" and path.is_file():
            if pattern.search(strip_rust_comments(path.read_text(errors="replace"))):
                return True
    return False


def reconcile_registries(
    root: Path,
    workspace: Workspace,
    registries: dict[str, dict],
    report: Report,
) -> None:
    """The register says what Cargo and the source tree actually do."""
    includes, embed_sites = scan_non_cargo_edges(root, workspace, registry_assets(registries))
    product_edges = workspace.edges(("normal", "build"))
    reach_edges = workspace.edges() | registered_edges(registries, ("embedded",))
    embedded_by_any: set[tuple[str, str]] = set()
    for doc in registries.values():
        for record in doc.get("consumers", []):
            if isinstance(record, dict) and record.get("dependency_kind") == "embedded":
                for via in record.get("via", []):
                    embedded_by_any.add((record.get("package"), via))
    registered_embedders = {embedder for embedder, _ in embedded_by_any}
    for package, sites in sorted(embed_sites.items()):
        if package not in registered_embedders:
            report.failures.append(
                f"{package} embeds a binary ({sorted(sites)[0]}) but no consumers.json "
                f"registers it as an embedded consumer"
            )
    for crate, doc in sorted(registries.items()):
        where = f"{workspace.dirs[crate]}/{REGISTRY_FILE}"
        ident = re.compile(r"\b" + re.escape(crate.replace("-", "_")) + r"\b")
        records = {
            (r.get("package"), r.get("dependency_kind")): r
            for r in doc.get("consumers", [])
            if isinstance(r, dict)
        }
        expected = expected_cargo_records(workspace, crate)
        for package, kind in sorted(set(expected) - set(records)):
            report.failures.append(
                f"{where}: missing consumer {package} ({kind}); Cargo says it depends on {crate}"
            )
        for (package, kind), record in sorted(records.items(), key=lambda item: repr(item[0])):
            label = f"{where}: consumer {package} ({kind})"
            if kind in CARGO_KINDS:
                if (package, kind) not in expected:
                    report.failures.append(
                        f"{label} is stale; Cargo has no {kind} dependency on {crate}"
                    )
                    continue
                want = expected[(package, kind)]
                have = {
                    "targets": sorted(record.get("targets", ["*"])),
                    "features": sorted(record.get("features", [])),
                    "optional": bool(record.get("optional", False)),
                    "forwarded_features": {
                        k: sorted(v) for k, v in record.get("forwarded_features", {}).items()
                    },
                    "enabled_by": sorted(record.get("enabled_by", [])),
                }
                for key, value in want.items():
                    if have[key] != value:
                        report.failures.append(
                            f"{label}: {key} is {have[key]!r} in the register but {value!r} in Cargo"
                        )
                for path in record.get("paths", []):
                    if isinstance(path, dict) and not _entry_shows(
                        root, path.get("entry_points", []), ident
                    ):
                        report.failures.append(
                            f"{label} path {path.get('id')}: no Rust entry point refers to "
                            f"{crate.replace('-', '_')}"
                        )
                for use in record.get("production_uses", []):
                    if isinstance(use, dict) and {"id", "symbol", "reason"} <= set(use):
                        check_production_use(
                            root, use, use.get("scope") or workspace.dirs[package], report
                        )
            elif kind == "source":
                found = includes.get((package, crate), {})
                declared = set(record.get("includes", []))
                if not found:
                    report.failures.append(
                        f"{label} is stale; {package} does not compile in any {crate} file"
                    )
                    continue
                for missing in sorted(set(found) - declared):
                    report.failures.append(f"{label}: includes is missing {missing}")
                for stale in sorted(declared - set(found)):
                    report.failures.append(f"{label}: {stale} is no longer included")
                sites = {site for files in found.values() for site in files}
                for path in record.get("paths", []):
                    if isinstance(path, dict) and not sites & set(path.get("entry_points", [])):
                        report.failures.append(
                            f"{label} path {path.get('id')}: name an including file "
                            f"({sorted(sites)[0]}) as an entry point"
                        )
            elif kind == "embedded":
                if package not in embed_sites:
                    report.failures.append(f"{label} is stale; {package} embeds no binary")
                reaching = reverse_closure([crate], product_edges)
                for via in record.get("via", []):
                    if via not in reaching:
                        report.failures.append(
                            f"{label}: embedded {via} does not depend on {crate}; the record is stale"
                        )
                for path in record.get("paths", []):
                    if isinstance(path, dict) and not _entry_shows(
                        root, path.get("entry_points", []), EMBED_SITE
                    ):
                        report.failures.append(
                            f"{label} path {path.get('id')}: name the file that embeds the binary"
                        )
            elif kind == "indirect":
                if (package, crate) in workspace.edges():
                    report.failures.append(
                        f"{label}: {package} depends on {crate} directly; register that instead"
                    )
                reaching = reverse_closure([crate], reach_edges)
                for via in record.get("via", []):
                    if (
                        via in workspace.shared
                        or via not in reaching
                        or via == package
                        or package not in reverse_closure([via], reach_edges)
                    ):
                        report.failures.append(
                            f"{label}: {package} does not reach {crate} through the non-shared "
                            f"package {via}; the record is stale"
                        )
        for package, crate_owner in sorted(includes):
            if crate_owner == crate and (package, "source") not in records:
                report.failures.append(
                    f"{where}: missing consumer {package} (source); it compiles in "
                    f"{sorted(includes[(package, crate)])[0]}"
                )
        reaching = reverse_closure([crate], product_edges)
        for embedder, via in sorted(embedded_by_any):
            if via in reaching and embedder != crate:
                record = records.get((embedder, "embedded"))
                if record is None or via not in record.get("via", []):
                    report.failures.append(
                        f"{where}: missing consumer {embedder} (embedded via {via}); "
                        f"{via} depends on {crate} and {embedder} embeds it"
                    )
        report.notes.append(
            f"register {crate}: {len(records)} consumer records, "
            f"{sum(len(r.get('paths', [])) for r in records.values())} path records"
        )


def check_path_identity(registries: dict[str, dict], report: Report) -> None:
    """A path ID names one product path, owned by one role, everywhere."""
    owners: dict[str, set[str]] = defaultdict(set)
    for doc in registries.values():
        for record in doc.get("consumers", []):
            if not isinstance(record, dict):
                continue
            for path in record.get("paths", []):
                if isinstance(path, dict) and isinstance(path.get("id"), str):
                    owners[path["id"]].add(str(path.get("owner")))
    for path_id, names in sorted(owners.items()):
        if len(names) > 1:
            report.failures.append(f"path {path_id} has conflicting owners {sorted(names)}")


def check_registry(
    root: Path,
    workspace: Workspace,
    report: Report,
) -> tuple[dict[str, dict], Catalogue | None]:
    registries, errors = load_registries(fs_reader(root), workspace)
    report.failures.extend(errors)
    stray = (
        sorted(
            path.relative_to(root).as_posix()
            for path in (root / "shared").rglob(REGISTRY_FILE)
            if "target" not in path.parts
            and path.parent.relative_to(root).as_posix() not in workspace.dirs.values()
        )
        if (root / "shared").is_dir()
        else []
    )
    for path in stray:
        report.failures.append(f"{path} belongs to no shared workspace crate")
    if not workspace.shared:
        return registries, None
    catalogue, errors = load_catalogue(fs_reader(root))
    report.failures.extend(errors)
    if catalogue is None:
        return registries, None
    report.failures.extend(validate_catalogue(catalogue, workspace, root))
    for crate, doc in sorted(registries.items()):
        report.failures.extend(validate_registry(crate, doc, workspace, catalogue, root))
    check_path_identity(registries, report)
    reconcile_registries(root, workspace, registries, report)
    check_every_product_has_paths(workspace, registries, report)
    return registries, catalogue


def check_every_product_has_paths(
    workspace: Workspace, registries: dict[str, dict], report: Report
) -> None:
    """Every product reaching a shared crate, however indirectly, has a path.

    A package that reaches shared code only through a non-shared helper (or
    by embedding a consumer) would otherwise have no validation obligation.
    """
    edges = workspace.edges() | registered_edges(registries, ("embedded",))
    reaching = reverse_closure(workspace.shared, edges) - workspace.shared
    with_paths = {
        record.get("package")
        for doc in registries.values()
        for record in doc.get("consumers", [])
        if isinstance(record, dict) and record.get("paths")
    }
    for package in sorted(reaching - with_paths):
        through = sorted(dependency for dependent, dependency in edges if dependent == package)
        report.failures.append(
            f"{package} reaches shared crates through {', '.join(through)} but no "
            f"consumers.json names a path for it; add an indirect record (via the helper)"
        )


# --- Evidence --------------------------------------------------------------------------


def host_os() -> str:
    return {"Darwin": "macos", "Linux": "linux", "Windows": "windows"}.get(
        platform.system(), platform.system().lower()
    )


def load_evidence(paths: Iterable[Path]) -> tuple[list[dict], list[str]]:
    """Validation results from evidence files; malformed files are errors."""
    records, errors = [], []
    for path in paths:
        try:
            doc = parse_json(path.read_text())
        except (OSError, ValueError) as error:
            errors.append(f"evidence {path}: {error}")
            continue
        if not isinstance(doc, dict) or doc.get("schema_version") != EVIDENCE_SCHEMA_VERSION:
            errors.append(f"evidence {path}: schema_version must be {EVIDENCE_SCHEMA_VERSION}")
            continue
        for index, result in enumerate(doc.get("results", [])):
            if not isinstance(result, dict):
                errors.append(f"evidence {path}: results[{index}] must be an object")
                continue
            record = {"revision": doc.get("revision"), **result, "file": str(path)}
            if record.get("result") not in RESULTS:
                errors.append(f"evidence {path}: results[{index}] result must be one of {RESULTS}")
                continue
            if not isinstance(record.get("validation"), str) or not isinstance(
                record.get("revision"), str
            ):
                errors.append(f"evidence {path}: results[{index}] needs validation and revision")
                continue
            records.append(record)
    return records, errors


def target_matches(validation: dict, target: object) -> bool:
    if validation.get("rust_target"):
        return target == validation["rust_target"]
    if validation.get("target_os") == "any":
        return target in TARGET_OSES[1:] or bool(
            isinstance(target, str) and RUST_TARGET.match(target)
        )
    return target == validation.get("target_os")


def validation_status(
    catalogue: Catalogue, validation_id: str, revision: str, evidence: list[dict]
) -> tuple[str, str]:
    """``(result, why)`` for one validation at one revision.

    Only evidence for exactly this revision, target and feature set counts.
    Anything else, and no evidence at all, is ``not_run``: never a pass.
    """
    return definition_status(catalogue.resolved(validation_id), validation_id, revision, evidence)


def definition_status(
    validation: dict, validation_id: str, revision: str, evidence: list[dict]
) -> tuple[str, str]:
    """``(result, why)`` for one resolved validation definition."""
    relevant = [r for r in evidence if r.get("validation") == validation_id]
    if not relevant:
        return "not_run", "no evidence"
    usable, rejected = [], []
    for record in relevant:
        if record.get("revision") != revision:
            rejected.append(f"evidence is for revision {str(record.get('revision'))[:12]}")
        elif not target_matches(validation, record.get("target")):
            rejected.append(f"evidence is for target {record.get('target')!r}")
        elif sorted(record.get("features", [])) != sorted(validation["features"]):
            rejected.append(f"evidence is for features {record.get('features', [])!r}")
        else:
            usable.append(record["result"])
    for result in ("failed", "blocked", "passed", "not_run"):
        if result in usable:
            return result, f"{usable.count(result)} record(s)"
    return "not_run", "; ".join(sorted(set(rejected)))


# --- Change impact ---------------------------------------------------------------------


@dataclass
class Snapshot:
    """The graph, register and catalogue as of one revision."""

    revision: str
    workspace: Workspace
    registries: dict[str, dict]
    catalogue: Catalogue | None
    read: Reader
    list_files: Callable[[str], list[str]]


def snapshot(root: Path, revision: str) -> Snapshot:
    read = git_reader(root, revision)
    workspace = workspace_from_manifests(read)
    registries, _ = load_registries(read, workspace)
    catalogue, _ = load_catalogue(read)
    return Snapshot(revision, workspace, registries, catalogue, read, git_lister(root, revision))


@dataclass
class Requirement:
    validation: str
    reasons: list[tuple[str, str, str]] = field(default_factory=list)


@dataclass
class Impact:
    base: str
    head: str
    changed: dict[str, set[str]]
    registry_changes: list[str]
    doc_only: dict[str, set[str]]
    affected_shared: set[str]
    affected_products: set[str]
    requirements: dict[str, Requirement]
    retired: dict[str, list[tuple[str, str, str]]]
    review: list[str]
    base_catalogue: Catalogue | None = None
    weakened: dict[str, list[str]] = field(default_factory=dict)


def _lock_packages(text: str | None) -> dict[str, tuple]:
    if not text:
        return {}
    packages = {}
    for package in tomllib.loads(text).get("package", []):
        key = f"{package['name']} {package.get('version', '')}"
        packages[key] = (
            package.get("source"),
            package.get("checksum"),
            tuple(sorted(package.get("dependencies", []))),
        )
    return packages


def lock_changed_shared(base: str | None, head: str | None, shared: set[str]) -> set[str]:
    """Shared crates whose resolved dependency graph changed in Cargo.lock."""
    old, new = _lock_packages(base), _lock_packages(head)
    changed = {key for key in set(old) | set(new) if old.get(key) != new.get(key)}
    edges = set()
    for packages in (old, new):
        names = defaultdict(list)
        for key in packages:
            names[key.split(" ")[0]].append(key)
        for key, (_, _, deps) in packages.items():
            for dep in deps:
                parts = dep.split(" ")
                targets = [f"{parts[0]} {parts[1]}"] if len(parts) > 1 else names[parts[0]]
                edges.update((key, target) for target in targets)
    affected = reverse_closure(changed, edges)
    return {key.split(" ")[0] for key in affected} & shared


def _workspace_deps(text: str | None) -> dict:
    if not text:
        return {}
    return tomllib.loads(text).get("workspace", {}).get("dependencies", {})


def _without_workspace_deps(text: str | None) -> dict:
    if not text:
        return {}
    doc = tomllib.loads(text)
    doc.get("workspace", {}).pop("dependencies", None)
    return doc


def classify_changes(
    files: list[str], base: Snapshot, head: Snapshot
) -> tuple[dict[str, set[str]], list[str], dict[str, set[str]]]:
    """Which shared crates a change touches, and how."""
    shared = sorted(base.workspace.shared | head.workspace.shared)
    # A renamed crate keeps its directory: attribute a change to every name
    # the directory had at the base or the head, so neither side's
    # obligations are dropped.
    names_by_dir: dict[str, set[str]] = defaultdict(set)
    for snap in (base, head):
        for crate in snap.workspace.shared:
            names_by_dir[snap.workspace.dirs[crate]].add(crate)
    assets: dict[str, set[str]] = defaultdict(set)
    for snap in (base, head):
        for asset, crate in registry_assets(snap.registries).items():
            assets[asset].add(crate)
    included_cache: dict[str, set[str]] = {}

    def included(directory: str) -> set[str]:
        if directory not in included_cache:
            included_cache[directory] = _included_files(base, directory) | _included_files(
                head, directory
            )
        return included_cache[directory]

    changed: dict[str, set[str]] = defaultdict(set)
    registry_changes: list[str] = []
    doc_only: dict[str, set[str]] = defaultdict(set)
    for path in files:
        if path == CATALOGUE_PATH:
            registry_changes.append(path)
            continue
        if path in assets:
            for crate in sorted(assets[path]):
                changed[crate].add(f"asset {path}")
            continue
        owning_dirs = sorted(d for d in names_by_dir if path.startswith(d + "/"))
        for directory in owning_dirs:
            owners = sorted(names_by_dir[directory])
            if path == f"{directory}/{REGISTRY_FILE}":
                registry_changes.append(path)
            elif path.endswith(".md") and path not in included(directory):
                for owner in owners:
                    doc_only[owner].add(path)
            else:
                for owner in owners:
                    changed[owner].add(f"source {path}")
        if not owning_dirs and GLOBAL_BUILD_INPUTS.match(path):
            for crate in shared:
                changed[crate].add(f"build input {path}")
    if "Cargo.toml" in files:
        old, new = base.read("Cargo.toml"), head.read("Cargo.toml")
        if _without_workspace_deps(old) != _without_workspace_deps(new):
            for crate in shared:
                changed[crate].add("workspace Cargo.toml")
        old_deps, new_deps = _workspace_deps(old), _workspace_deps(new)
        keys = {k for k in set(old_deps) | set(new_deps) if old_deps.get(k) != new_deps.get(k)}
        for snap in (base, head):
            for crate in sorted(set(shared) & set(snap.workspace.dirs)):
                manifest = snap.read(f"{snap.workspace.dirs[crate]}/Cargo.toml") or ""
                for key in sorted(keys):
                    if re.search(
                        rf"^\s*{re.escape(key)}\s*(?:\.workspace|=\s*\{{[^}}]*workspace)",
                        manifest,
                        re.M,
                    ):
                        changed[crate].add(f"workspace dependency {key}")
    dep_paths = {}
    for text in (base.read("Cargo.toml"), head.read("Cargo.toml")):
        for key, spec in _workspace_deps(text).items():
            if (
                isinstance(spec, dict)
                and "path" in spec
                and not str(spec["path"]).startswith(
                    ("shared/", "hosts/", "clients/", "packaging/", "tools/")
                )
            ):
                dep_paths[posixpath.normpath(spec["path"])] = key
    for path in files:
        for location, key in dep_paths.items():
            if path.startswith(location + "/"):
                for snap in (base, head):
                    for crate in sorted(set(shared) & set(snap.workspace.dirs)):
                        manifest = snap.read(f"{snap.workspace.dirs[crate]}/Cargo.toml") or ""
                        if re.search(rf"^\s*{re.escape(key)}\b", manifest, re.M):
                            changed[crate].add(f"vendored dependency {key}")
    if "Cargo.lock" in files:
        for crate in sorted(
            lock_changed_shared(base.read("Cargo.lock"), head.read("Cargo.lock"), set(shared))
        ):
            changed[crate].add("Cargo.lock")
    return dict(changed), sorted(set(registry_changes)), dict(doc_only)


def _included_files(snap: Snapshot, directory: str) -> set[str]:
    """Files any Rust source of the crate at ``directory`` compiles in.

    Covers every ``.rs`` file of the crate, ``build.rs`` included, with the
    same resolution as the source-inclusion scan.
    """
    included = set()
    for relative in snap.list_files(directory):
        if relative.endswith(".rs") and "/target/" not in f"/{relative}":
            text = snap.read(relative) or ""
            included.update(include_targets(text, relative, directory))
    return included


def registered_edges(
    registries: dict[str, dict], kinds: Iterable[str] = ("source", "embedded")
) -> set[tuple[str, str]]:
    """Source-inclusion and embedding edges, which Cargo does not see."""
    wanted = set(kinds)
    edges = set()
    for crate, doc in registries.items():
        for record in doc.get("consumers", []):
            if not isinstance(record, dict) or record.get("dependency_kind") not in wanted:
                continue
            if record.get("dependency_kind") == "source":
                edges.add((record.get("package"), crate))
            else:
                edges.update((record.get("package"), via) for via in record.get("via", []))
    return edges


def _union_edges(snapshots: Iterable[Snapshot]) -> set[tuple[str, str]]:
    """Cargo and embedding edges of both revisions.

    Source-inclusion edges are not here: a package that compiles in one
    shared file is affected when that file changes, not by the rest of the
    crate (see ``_source_hits``).
    """
    edges = set()
    for snap in snapshots:
        edges |= snap.workspace.edges() | registered_edges(snap.registries, ("embedded",))
    return edges


def _source_hits(snapshots: Iterable[Snapshot], files: list[str]) -> set[tuple[str, str]]:
    """``(crate, package)`` source records whose included files changed."""
    changed = set(files)
    hits = set()
    for snap in snapshots:
        for crate, doc in snap.registries.items():
            for record in doc.get("consumers", []):
                if (
                    isinstance(record, dict)
                    and record.get("dependency_kind") == "source"
                    and changed & set(record.get("includes", []))
                ):
                    hits.add((crate, record.get("package")))
    return hits


def compute_impact(base: Snapshot, head: Snapshot, files: list[str]) -> Impact:
    catalogue = head.catalogue
    if catalogue is None:
        raise RuntimeError(f"the head revision has no {CATALOGUE_PATH}")
    snapshots = (base, head)
    shared = base.workspace.shared | head.workspace.shared
    changed, registry_changes, doc_only = classify_changes(files, base, head)
    source_hits = _source_hits(snapshots, files)
    closure = reverse_closure(
        set(changed) | {package for _, package in source_hits}, _union_edges(snapshots)
    )
    affected_shared = closure & shared
    affected_products = closure - shared
    requirements: dict[str, Requirement] = {}
    retired: dict[str, list[tuple[str, str, str]]] = defaultdict(list)

    def require(validation_id: str, reason: tuple[str, str, str]) -> None:
        if validation_id not in catalogue.validations:
            retired[validation_id].append(reason)
            return
        requirement = requirements.setdefault(validation_id, Requirement(validation_id))
        if reason not in requirement.reasons:
            requirement.reasons.append(reason)

    covered: set[str] = set()
    for crate in sorted(affected_shared):
        for snap in snapshots:
            doc = snap.registries.get(crate, {})
            for validation_id in doc.get("validation", []):
                require(validation_id, (crate, crate, "own validation"))
            for record in doc.get("consumers", []):
                if not isinstance(record, dict):
                    continue
                if (
                    record.get("dependency_kind") == "source"
                    and (crate, record.get("package")) not in source_hits
                ):
                    continue
                covered.add(record.get("package"))
                for path in record.get("paths", []):
                    for validation_id in path.get("validation", []):
                        require(validation_id, (crate, record.get("package"), path.get("id")))
    # Crate-level impact is conservative: every path of every affected product
    # keeps its builds and tests, even where no entry point names the changed
    # crate. Hardware runs follow the reviewed per-crate path mapping, except
    # for a product reached only through a product helper, which keeps all.
    for product in sorted(affected_products):
        for snap in snapshots:
            for doc in snap.registries.values():
                for record in doc.get("consumers", []):
                    if not isinstance(record, dict) or record.get("package") != product:
                        continue
                    for path in record.get("paths", []):
                        for validation_id in path.get("validation", []):
                            kind = catalogue.validations.get(validation_id, {}).get("kind")
                            if product not in covered or kind != "hardware":
                                require(validation_id, ("(closure)", product, path.get("id")))
    if registry_changes or doc_only:
        require(catalogue.registry_validation, ("(register)", "-", "schema and documentation"))
    for validation_id, reasons in dropped_requirements(base, head).items():
        for reason in reasons:
            if reason not in retired[validation_id]:
                retired[validation_id].append(reason)
    weakened = weakened_definitions(base.catalogue, catalogue)
    review = []
    for path in registry_changes:
        old, new = base.read(path), head.read(path)
        if path == CATALOGUE_PATH:
            review.append(
                f"{path}: validation catalogue changed (Release/Security, Shared/Architecture)"
            )
            continue
        owners = set()
        for text in (old, new):
            try:
                doc = parse_json(text) if text else {}
            except ValueError:
                doc = {}
            owners.add(str(doc.get("owner", "Shared/Architecture")))
            for record in doc.get("consumers", []) if isinstance(doc, dict) else []:
                for p in record.get("paths", []) if isinstance(record, dict) else []:
                    owners.add(str(p.get("owner")))
        review.append(f"{path}: register changed ({', '.join(sorted(owners))})")
    for validation_id in sorted(retired):
        where = ", ".join(sorted({path for _, _, path in retired[validation_id]}))
        review.append(
            f"validation {validation_id} was required at the base ({where}) and the head no "
            f"longer requires it there; its owner must acknowledge dropping it"
        )
    for validation_id in sorted(weakened):
        review.append(
            f"validation {validation_id} changed definition ({'; '.join(weakened[validation_id])}); "
            f"its owner must acknowledge the new definition"
        )
    return Impact(
        base.revision,
        head.revision,
        changed,
        registry_changes,
        doc_only,
        affected_shared,
        affected_products,
        requirements,
        {k: v for k, v in sorted(retired.items())},
        review,
        base.catalogue,
        weakened,
    )


def _register_references(snap: Snapshot) -> tuple[dict[str, set[str]], set[str]]:
    """``({path ID: validation IDs}, own validation IDs)`` across all registers."""
    by_path: dict[str, set[str]] = defaultdict(set)
    own: set[str] = set()
    for doc in snap.registries.values():
        own.update(v for v in doc.get("validation", []) if isinstance(v, str))
        for record in doc.get("consumers", []):
            for path in record.get("paths", []) if isinstance(record, dict) else []:
                if isinstance(path, dict) and isinstance(path.get("id"), str):
                    by_path[path["id"]].update(
                        v for v in path.get("validation", []) if isinstance(v, str)
                    )
    return by_path, own


def dropped_requirements(base: Snapshot, head: Snapshot) -> dict[str, list[tuple[str, str, str]]]:
    """Validation obligations the base had and the head no longer has.

    Computed over the whole base, independent of what this change touches,
    so a register- or catalogue-only change cannot quietly remove one and
    let the next change inherit the weaker base:

    - a validation the base catalogue defines, or a base register names,
      that the head catalogue does not define;
    - a validation a base path record required that no head path record
      with the same path ID requires any more;
    - a crate's own validation that no head register lists any more.
    """
    head_ids = set(head.catalogue.validations) if head.catalogue else set()
    head_paths, head_own = _register_references(head)
    dropped: dict[str, list[tuple[str, str, str]]] = defaultdict(list)

    def add(validation_id: str, reason: tuple[str, str, str]) -> None:
        if reason not in dropped[validation_id]:
            dropped[validation_id].append(reason)

    for crate, doc in sorted(base.registries.items()):
        for validation_id in doc.get("validation", []):
            if validation_id not in head_ids or validation_id not in head_own:
                add(validation_id, (crate, crate, "own validation"))
        for record in doc.get("consumers", []):
            if not isinstance(record, dict):
                continue
            for path in record.get("paths", []):
                if not isinstance(path, dict):
                    continue
                for validation_id in path.get("validation", []):
                    if validation_id not in head_ids or validation_id not in head_paths.get(
                        path.get("id"), set()
                    ):
                        add(validation_id, (crate, str(record.get("package")), str(path.get("id"))))
    if base.catalogue is not None:
        for validation_id in sorted(set(base.catalogue.validations) - head_ids):
            if validation_id not in dropped:
                add(validation_id, ("(catalogue)", "-", "catalogue definition"))
    return dict(dropped)


def _normal_definition(catalogue: Catalogue, validation_id: str) -> dict:
    try:
        resolved = catalogue.resolved(validation_id)
        command: object = catalogue.command(validation_id)
    except (KeyError, TypeError, ValueError):
        resolved, command = catalogue.validations.get(validation_id, {}), None
    out = {key: resolved.get(key) for key in DEFINITION_KEYS}
    out["command"] = command
    if isinstance(out["features"], list):
        out["features"] = sorted(out["features"])
    return out


def weakened_definitions(base: Catalogue | None, head: Catalogue) -> dict[str, list[str]]:
    """Validations whose head definition differs from the base one.

    Every such change except making a validation mandatory needs its owner's
    acknowledgement, whether or not this change also requires the validation:
    a required one is judged by both definitions this time (``evaluate``),
    but the next change inherits the head definition as its base.
    """
    if base is None:
        return {}
    weakened = {}
    for validation_id in sorted(set(base.validations) & set(head.validations)):
        old = _normal_definition(base, validation_id)
        new = _normal_definition(head, validation_id)
        differences = [
            f"{key} {old[key]!r} -> {new[key]!r}"
            for key in DEFINITION_KEYS
            if old[key] != new[key] and not (key == "mandatory" and new[key] is True)
        ]
        if differences:
            weakened[validation_id] = differences
    return weakened


# A validation definition is the contract a result must meet. A head change
# that weakens one of these must not weaken what the change itself must prove.
DEFINITION_KEYS = ("kind", "target_os", "rust_target", "features", "mandatory", "command")


def definitions(impact: Impact, catalogue: Catalogue, validation_id: str) -> list[dict]:
    """The head definition, plus the base one where it differs: both must hold."""
    head = dict(catalogue.resolved(validation_id), command=catalogue.command(validation_id))
    head["label"] = "head definition"
    found = [head]
    base_catalogue = impact.base_catalogue
    if base_catalogue is not None and validation_id in base_catalogue.validations:
        try:
            base = dict(
                base_catalogue.resolved(validation_id),
                command=base_catalogue.command(validation_id),
            )
        except (KeyError, TypeError, ValueError):
            base = dict(base_catalogue.resolved(validation_id), command=None)
        base["label"] = "base definition"

        def normal(definition: dict, key: str) -> object:
            value = definition.get(key)
            return sorted(value) if key == "features" and isinstance(value, list) else value

        if any(normal(base, key) != normal(head, key) for key in DEFINITION_KEYS):
            found.append(base)
    return found


def evaluate(
    impact: Impact, catalogue: Catalogue, validation_id: str, evidence: list[dict] | None
) -> dict:
    """Status of one required validation under the stricter of base and head.

    Mandatory if either definition says so; passed only when evidence meets
    every definition's target and features.
    """
    found = definitions(impact, catalogue, validation_id)
    mandatory = any(definition.get("mandatory", True) for definition in found)
    if evidence is None:
        status, why = "not_run", "no evidence supplied"
    else:
        results = [
            (
                definition["label"],
                *definition_status(definition, validation_id, impact.head, evidence),
            )
            for definition in found
        ]
        if all(result == "passed" for _, result, _ in results):
            status, why = "passed", results[0][2]
        else:
            status = next(
                worst
                for worst in ("failed", "blocked", "not_run")
                if any(result == worst for _, result, _ in results)
            )
            why = "; ".join(
                f"{label}: {result}, {reason}" if len(results) > 1 else reason
                for label, result, reason in results
                if result != "passed"
            )
    return {"definitions": found, "mandatory": mandatory, "status": status, "why": why}


def retired_items(impact: Impact, acknowledged: Iterable[str]) -> tuple[list[str], list[str]]:
    """``(unacknowledged, acknowledged)`` retired or weakened validation IDs."""
    acknowledged = set(acknowledged)
    changed = sorted(set(impact.retired) | set(impact.weakened))
    return [v for v in changed if v not in acknowledged], [v for v in changed if v in acknowledged]


def unmatched_acknowledgements(impact: Impact, acknowledged: Iterable[str]) -> list[str]:
    return sorted(set(acknowledged) - set(impact.retired) - set(impact.weakened))


def render_impact(
    impact: Impact,
    catalogue: Catalogue,
    evidence: list[dict] | None,
    show_commands: bool,
    acknowledged: Iterable[str] = (),
) -> tuple[str, list[str]]:
    """Readable impact text and what keeps the gate closed.

    A mandatory validation that has not passed, or a validation the base
    required that the head catalogue dropped without an owner's
    acknowledgement, keeps the gate closed.
    """
    lines = [f"Change impact {impact.base[:12]}..{impact.head[:12]}"]
    if not impact.changed and not impact.registry_changes and not impact.doc_only:
        lines.append("  No shared crate, register or catalogue changed.")
    for crate, reasons in sorted(impact.changed.items()):
        lines.append(f"  changed {crate}: {', '.join(sorted(reasons)[:4])}")
    for crate, files in sorted(impact.doc_only.items()):
        lines.append(f"  documentation only {crate}: {', '.join(sorted(files))}")
    for path in impact.registry_changes:
        lines.append(f"  register/catalogue {path}")
    if impact.affected_shared:
        lines.append(f"  affected shared crates: {', '.join(sorted(impact.affected_shared))}")
    if impact.affected_products:
        lines.append(f"  affected products: {', '.join(sorted(impact.affected_products))}")
    missing = []
    lines.append(f"Required validation ({len(impact.requirements)}):")
    for validation_id in sorted(impact.requirements):
        result = evaluate(impact, catalogue, validation_id, evidence)
        head = result["definitions"][0]
        status, why = result["status"], result["why"]
        label = "mandatory" if result["mandatory"] else "advisory"
        if result["mandatory"] and status != "passed":
            missing.append(f"{validation_id}: {status} ({why})")
        lines.append(
            f"  [{status}] {validation_id} ({label}, {head['kind']}, "
            f"{head.get('rust_target') or head['target_os']})"
        )
        for definition in result["definitions"][1:]:
            lines.append(
                f"      changed since the base; the base definition still applies: "
                f"{definition.get('kind')}, "
                f"{definition.get('rust_target') or definition.get('target_os')}, "
                f"features {definition.get('features')}, "
                f"{'mandatory' if definition.get('mandatory', True) else 'advisory'}"
            )
        if show_commands:
            argv = head["command"]
            lines.append(f"      run: {shlex.join(argv) if argv else '(recorded evidence only)'}")
        for crate, package, path in impact.requirements[validation_id].reasons[:6]:
            lines.append(f"      {crate} -> {package} -> {path}")
        if len(impact.requirements[validation_id].reasons) > 6:
            lines.append(f"      ... {len(impact.requirements[validation_id].reasons) - 6} more")
    open_retired, acknowledged_retired = retired_items(impact, acknowledged)
    if impact.retired or impact.weakened:
        lines.append("Retired or weakened validation (needs the owner's acknowledgement):")
        for validation_id in open_retired + acknowledged_retired:
            state = (
                "acknowledged by owner"
                if validation_id in acknowledged_retired
                else "unacknowledged"
            )
            what = "retired" if validation_id in impact.retired else "weakened"
            lines.append(f"  [{state}] {validation_id} ({what})")
        for validation_id in open_retired:
            what = "retired" if validation_id in impact.retired else "weakened"
            missing.append(
                f"{validation_id}: {what} without owner acknowledgement "
                f"(--acknowledge-retired {validation_id})"
            )
    if impact.review:
        lines.append("Owner review required:")
        lines.extend(f"  {item}" for item in impact.review)
    return "\n".join(lines), missing


def impact_json(
    impact: Impact,
    catalogue: Catalogue,
    evidence: list[dict] | None,
    acknowledged: Iterable[str] = (),
) -> dict:
    _, acknowledged_retired = retired_items(impact, acknowledged)
    out = {
        "base": impact.base,
        "head": impact.head,
        "changed": {k: sorted(v) for k, v in sorted(impact.changed.items())},
        "documentation_only": {k: sorted(v) for k, v in sorted(impact.doc_only.items())},
        "register_changes": impact.registry_changes,
        "affected_shared": sorted(impact.affected_shared),
        "affected_products": sorted(impact.affected_products),
        "required": [],
        "retired": [
            {
                "validation": validation_id,
                "acknowledged": validation_id in acknowledged_retired,
                "paths": [
                    {"crate": c, "consumer": p, "path": i}
                    for c, p, i in impact.retired[validation_id]
                ],
            }
            for validation_id in sorted(impact.retired)
        ],
        "weakened": [
            {
                "validation": validation_id,
                "acknowledged": validation_id in acknowledged_retired,
                "changes": impact.weakened[validation_id],
            }
            for validation_id in sorted(impact.weakened)
        ],
        "unmatched_acknowledgements": unmatched_acknowledgements(impact, acknowledged),
        "owner_review": impact.review,
    }
    for validation_id in sorted(impact.requirements):
        result = evaluate(impact, catalogue, validation_id, evidence)
        head = result["definitions"][0]
        out["required"].append(
            {
                "validation": validation_id,
                "kind": head["kind"],
                "target": head.get("rust_target") or head["target_os"],
                "features": head["features"],
                "mandatory": result["mandatory"],
                "command": head["command"],
                "base_definition": (
                    {k: result["definitions"][1].get(k) for k in DEFINITION_KEYS}
                    if len(result["definitions"]) > 1
                    else None
                ),
                "status": result["status"],
                "status_reason": result["why"],
                "paths": [
                    {"crate": c, "consumer": p, "path": i}
                    for c, p, i in impact.requirements[validation_id].reasons
                ],
            }
        )
    return out


def worktree_mismatch(root: Path, revision: str) -> str | None:
    """Why commands run in ``root`` would not test ``revision``, if they would not.

    Evidence is stamped with the revision, so the checked-out tree must be
    exactly that commit: same HEAD and no tracked or untracked change
    (ignored files are allowed).
    """
    try:
        head = git(root, "rev-parse", "--verify", "HEAD^{commit}").strip()
        status = git(root, "status", "--porcelain", "--untracked-files=all")
    except (OSError, subprocess.CalledProcessError) as error:
        return f"cannot inspect the working tree: {error}"
    if head != revision:
        return f"the working tree is at {head[:12]}, not the head revision {revision[:12]}"
    if status.strip():
        return "the working tree has uncommitted changes"
    return None


def run_local_validations(
    root: Path,
    impact: Impact,
    catalogue: Catalogue,
    runner: Callable[[list[str], Path], int] | None = None,
) -> dict:
    """Run required validations this host can run; record every outcome.

    Only catalogue templates are executed, as argv lists without a shell.
    Validations for another OS, with recorded-only evidence, whose base and
    head definitions differ in command, or any validation at all when the
    working tree is not exactly the head revision, are recorded as
    ``not_run``.
    """

    def default_runner(argv: list[str], cwd: Path) -> int:
        # The commands' own output is a diagnostic: keep stdout for the
        # impact listing (one JSON document with --format json).
        sys.stdout.flush()
        return subprocess.run(argv, cwd=cwd, stdout=sys.stderr).returncode

    runner = runner or default_runner
    here = host_os()
    refusal = worktree_mismatch(root, impact.head)
    results = []
    for validation_id in sorted(impact.requirements):
        found = definitions(impact, catalogue, validation_id)
        validation = found[0]
        argv = validation["command"]
        target = validation.get("rust_target") or (
            here if validation["target_os"] == "any" else validation["target_os"]
        )
        record = {
            "validation": validation_id,
            "target": target,
            "features": validation["features"],
            "command": argv,
        }
        if refusal:
            record["result"] = "not_run"
            record["note"] = refusal
        elif any(d.get("kind") == "hardware" for d in found):
            record["result"] = "not_run"
            record["note"] = "hardware evidence is recorded by the lab, not run here"
        elif len({tuple(d.get("command") or ()) for d in found}) > 1:
            record["result"] = "not_run"
            record["note"] = "definition changed since the base; record evidence for both"
        elif not argv or validation["target_os"] not in ("any", here):
            record["result"] = "not_run"
        else:
            record["result"] = "passed" if runner(argv, root) == 0 else "failed"
        results.append(record)
    return {"schema_version": EVIDENCE_SCHEMA_VERSION, "revision": impact.head, "results": results}


# --- The generated report -----------------------------------------------------------------


def _cell(values: Iterable[str]) -> str:
    return "<br>".join(f"`{value}`" for value in values) or "—"


def render_report(
    workspace: Workspace,
    registries: dict[str, dict],
    catalogue: Catalogue,
    statuses: dict[str, str] | None = None,
    revision: str | None = None,
) -> str:
    def validation_cell(ids: list[str]) -> str:
        if statuses is None:
            return _cell(ids)
        return "<br>".join(f"`{v}` {statuses.get(v, 'not_run')}" for v in ids) or "—"

    records = [r for doc in registries.values() for r in doc.get("consumers", [])]
    path_ids = {p["id"] for r in records for p in r.get("paths", [])}
    lines = [
        "# Shared consumer register",
        "",
        "<!-- Generated by scripts/check_shared_contracts.py --write-report from",
        "     shared/*/consumers.json, scripts/ci/validation-profiles.toml and",
        "     cargo metadata. Do not edit by hand; the default check fails when",
        "     this file is stale. -->",
        "",
        "Each shared crate lists who uses it and which separate product paths",
        "that use serves. Changing a shared crate requires the validation of every",
        "path reached through the reverse-dependency closure, not only the crate's",
        "own tests: run `python3 scripts/check_shared_contracts.py --impact --base",
        "<rev>` to list it.",
        "",
    ]
    if statuses is None:
        lines += [
            "Results are revision-bound and are not recorded here; generate a report",
            "with `--report FILE --evidence FILE --head REV` to show them.",
            "",
        ]
    else:
        lines += [f"Validation results for revision `{revision}`.", ""]
    lines += [
        "Adding a product path means registering it here: its entry points, build",
        "profiles and validation IDs from `scripts/ci/validation-profiles.toml`.",
        "Changing a shared crate runs the consumer-impact gate:",
        "",
        "```sh",
        "python3 scripts/check_shared_contracts.py                # register + report",
        "python3 scripts/check_shared_contracts.py --impact --base main --commands",
        "python3 scripts/check_shared_contracts.py --impact --base main \\",
        "    --evidence linux.json --evidence lab.json --gate",
        "```",
        "",
        "An evidence file records what actually ran, for one exact revision. A",
        "result for another revision, target or feature set, a missing result,",
        "`blocked` and `not_run` never count as `passed`:",
        "",
        "```json",
        '{"schema_version": 1, "revision": "<40-hex commit>", "results": [',
        '  {"validation": "linux-pier-test", "target": "linux", "features": [],',
        '   "result": "passed"}]}',
        "```",
        "",
        "`--run --evidence-out FILE` runs the required validations this host can",
        "run, using only catalogue command templates, and writes such a file. It",
        "refuses (records `not_run`) unless the working tree is exactly the head",
        "revision with no uncommitted or untracked changes.",
        "",
        "A required validation is judged by the stricter of its base and head",
        "definitions: mandatory if either says so, and evidence must meet both",
        "definitions' targets and features. Independently of what a change",
        "touches, a validation the base required that the head no longer requires",
        "(dropped from the catalogue, or from a path record) or whose definition",
        "the head weakened keeps `--gate` closed until its owner acknowledges it",
        "with `--acknowledge-retired ID`, which the output records. With",
        "`--format json`, stdout is one JSON document; diagnostics go to stderr.",
        "",
    ]
    lines += [
        f"**{len(registries)} shared crates, {len(records)} consumer records, "
        f"{len(path_ids)} distinct paths, {len(catalogue.validations)} validations.**",
        "",
    ]
    edges = workspace.edges() | registered_edges(registries)
    for crate in sorted(registries):
        doc = registries[crate]
        direct = {package for package, dependency in edges if dependency == crate}
        indirect = sorted(reverse_closure([crate], edges) - direct - {crate})
        lines += [
            f"## `{crate}`",
            "",
            f"Owner: {doc.get('owner')}. Own validation: {validation_cell(doc.get('validation', []))}.",
        ]
        if doc.get("assets"):
            lines.append(f"Shared assets outside the crate: {_cell(doc['assets'])}.")
        lines.append(
            f"Indirect consumers (reverse-dependency closure, including registered "
            f"embedding and source edges): {_cell(indirect)}."
        )
        lines += [
            "",
            "| Consumer | Kind | Contracts | Path | Entry points | Build profiles | Validation |",
            "| --- | --- | --- | --- | --- | --- | --- |",
        ]
        for record in sorted(
            doc.get("consumers", []),
            key=lambda r: (
                r.get("package", ""),
                EDGE_KINDS.index(r.get("dependency_kind", "normal")),
            ),
        ):
            kind = record.get("dependency_kind")
            detail = []
            if record.get("features"):
                detail.append("features " + ", ".join(record["features"]))
            if record.get("targets"):
                detail.append("targets " + ", ".join(record["targets"]))
            if record.get("via"):
                verb = "embeds " if kind == "embedded" else "through "
                detail.append(verb + ", ".join(record["via"]))
            if record.get("includes"):
                detail.append("includes " + ", ".join(record["includes"]))
            kind_cell = kind + (f" ({'; '.join(detail)})" if detail else "")
            contracts = _cell(record.get("contracts", []))
            paths = record.get("paths", []) or [None]
            for path in paths:
                if path is None:
                    lines.append(
                        f"| `{record.get('package')}` | {kind_cell} | {contracts} | "
                        f"via its own register | — | — | — |"
                    )
                    continue
                lines.append(
                    f"| `{record.get('package')}` | {kind_cell} | {contracts} | "
                    f"`{path['id']}` ({path.get('owner')}) | {_cell(path.get('entry_points', []))} | "
                    f"{_cell(path.get('build_profiles', []))} | "
                    f"{validation_cell(path.get('validation', []))} |"
                )
        lines.append("")
    lines += [
        "## Validation catalogue",
        "",
        "| Validation | Kind | Target | Features | Mandatory | Command | Owner |",
        "| --- | --- | --- | --- | --- | --- | --- |",
    ]
    for validation_id in sorted(catalogue.validations):
        validation = catalogue.resolved(validation_id)
        argv = catalogue.command(validation_id)
        command = f"`{shlex.join(argv)}`" if argv else "recorded evidence"
        lines.append(
            f"| `{validation_id}` | {validation['kind']} | "
            f"{validation.get('rust_target') or validation['target_os']} | "
            f"{', '.join(validation['features']) or '—'} | "
            f"{'yes' if validation['mandatory'] else 'no'} | {command} | {validation['owner']} |"
        )
    return "\n".join(lines) + "\n"


def check_report(
    root: Path,
    workspace: Workspace,
    registries: dict[str, dict],
    catalogue: Catalogue | None,
    report: Report,
    write: bool,
) -> None:
    if catalogue is None or not registries:
        return
    try:
        text = render_report(workspace, registries, catalogue)
    except (KeyError, TypeError, ValueError) as error:
        report.failures.append(f"{REPORT_PATH}: cannot render the register: {error!r}")
        return
    path = root / REPORT_PATH
    if write:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        report.notes.append(f"wrote {REPORT_PATH}")
    elif not path.is_file() or path.read_text() != text:
        report.failures.append(
            f"{REPORT_PATH} is stale; run scripts/check_shared_contracts.py --write-report"
        )


# --- Entry points -------------------------------------------------------------------------


def run(
    root: Path,
    write_baseline: bool = False,
    workspace: Workspace | None = None,
    write_report: bool = False,
) -> Report:
    contracts = tomllib.loads((root / "scripts/ci/shared-contracts.toml").read_text())
    report = Report()
    if workspace is None:
        try:
            workspace = workspace_from_metadata(load_cargo_metadata(root))
        except (OSError, RuntimeError, ValueError) as error:
            report.failures.append(f"shared consumer register: {error}")
            workspace = None
        if workspace is not None:
            try:
                manifest_view = workspace_from_manifests(fs_reader(root))
                report.failures.extend(compare_workspaces(workspace, manifest_view))
            except (ValueError, KeyError, tomllib.TOMLDecodeError) as error:
                report.failures.append(f"manifest reader: {error}")
    check_consumers(root, contracts, report)
    if workspace is not None:
        registries, catalogue = check_registry(root, workspace, report)
        check_report(root, workspace, registries, catalogue, report, write_report)
    check_forbidden(root, contracts, report)
    check_ratchet(root, contracts, report, write_baseline)
    check_default_configs(root, contracts, report)
    return report


def impact_between(root: Path, base: str, head: str) -> tuple[Impact, Snapshot]:
    """The impact of ``head`` relative to its merge base with ``base``."""
    head_sha = git(root, "rev-parse", "--verify", f"{head}^{{commit}}").strip()
    base_sha = git(root, "merge-base", base, head_sha).strip()
    files = [
        line
        for line in git(
            root, "diff", "--name-only", "--no-renames", base_sha, head_sha
        ).splitlines()
        if line
    ]
    base_snap, head_snap = snapshot(root, base_sha), snapshot(root, head_sha)
    return compute_impact(base_snap, head_snap, files), head_snap


def run_impact(args: argparse.Namespace) -> int:
    """Impact listing and gate. With ``--format json`` stdout is one JSON
    document and every diagnostic goes to stderr."""
    as_json = args.format == "json"

    def diagnose(message: str) -> None:
        print(message, file=sys.stderr)

    root = args.root
    impact, head = impact_between(root, args.base, args.head)
    catalogue = head.catalogue
    assert catalogue is not None
    failures: list[str] = []
    evidence = None
    if args.evidence or args.gate:
        evidence, errors = load_evidence(args.evidence)
        failures += errors
    refusal = None
    if args.run:
        produced = run_local_validations(root, impact, catalogue)
        refusal = worktree_mismatch(root, impact.head)
        if refusal:
            diagnose(f"--run refused: {refusal}; every result is recorded as not_run")
        if args.evidence_out:
            args.evidence_out.write_text(json.dumps(produced, indent=2) + "\n")
        evidence = (evidence or []) + [
            {"revision": produced["revision"], **r} for r in produced["results"]
        ]
    acknowledged = sorted(
        {
            item.strip()
            for value in args.acknowledge_retired
            for item in value.split(",")
            if item.strip()
        }
    )
    unmatched = unmatched_acknowledgements(impact, acknowledged)
    failures += [
        f"--acknowledge-retired {validation_id} names no retired or weakened validation"
        for validation_id in unmatched
    ]
    text, missing = render_impact(impact, catalogue, evidence, args.commands, acknowledged)
    gate_failures = missing if args.gate else []
    if as_json:
        document = impact_json(impact, catalogue, evidence, acknowledged)
        document["run_refusal"] = refusal
        document["errors"] = failures
        document["gate"] = (
            {"ok": not gate_failures and not failures, "failures": gate_failures}
            if args.gate
            else None
        )
        print(json.dumps(document, indent=2))
    else:
        print(text)
    for failure in failures:
        diagnose(f"FAIL {failure}")
    if args.gate:
        if gate_failures:
            report = [f"shared consumer gate: {len(gate_failures)} requirement(s) not met"]
            report += [f"  FAIL {item}" for item in gate_failures]
        else:
            report = [
                "shared consumer gate: ok" if not failures else "shared consumer gate: errors"
            ]
        for line in report:
            if as_json:
                diagnose(line)
            else:
                print(("\n" if line is report[0] else "") + line)
    return 1 if failures or gate_failures else 0


def write_status_report(args: argparse.Namespace) -> int:
    root = args.root
    workspace = workspace_from_metadata(load_cargo_metadata(root))
    registries, errors = load_registries(fs_reader(root), workspace)
    catalogue, catalogue_errors = load_catalogue(fs_reader(root))
    if errors or catalogue_errors or catalogue is None:
        for error in errors + catalogue_errors:
            print(f"FAIL {error}")
        return 1
    revision = git(root, "rev-parse", "--verify", f"{args.head}^{{commit}}").strip()
    evidence, evidence_errors = load_evidence(args.evidence)
    for error in evidence_errors:
        print(f"FAIL {error}")
    statuses = {
        validation_id: validation_status(catalogue, validation_id, revision, evidence)[0]
        for validation_id in catalogue.validations
    }
    text = render_report(workspace, registries, catalogue, statuses, revision)
    if str(args.report) == "-":
        print(text, end="")
    else:
        args.report.write_text(text)
    return 1 if evidence_errors else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parent.parent)
    parser.add_argument("--write-baseline", action="store_true")
    parser.add_argument("--write-report", action="store_true", help=f"regenerate {REPORT_PATH}")
    parser.add_argument("--verbose", action="store_true")
    parser.add_argument("--impact", action="store_true", help="list validation a change requires")
    parser.add_argument("--base", help="comparison revision for --impact")
    parser.add_argument("--head", default="HEAD", help="candidate revision (default HEAD)")
    parser.add_argument("--evidence", type=Path, action="append", default=[])
    parser.add_argument(
        "--gate", action="store_true", help="fail while mandatory validation is not passed"
    )
    parser.add_argument("--commands", action="store_true", help="show catalogue commands")
    parser.add_argument(
        "--acknowledge-retired",
        action="append",
        default=[],
        metavar="ID[,ID]",
        help="owner acknowledgement that a retired validation may be dropped (recorded in output)",
    )
    parser.add_argument(
        "--run", action="store_true", help="run required validations this host can run"
    )
    parser.add_argument("--evidence-out", type=Path, help="write --run results here")
    parser.add_argument("--format", choices=("text", "json"), default="text")
    parser.add_argument(
        "--report", type=Path, help="write a report with evidence results ('-' = stdout)"
    )
    args = parser.parse_args()
    if args.impact:
        if not args.base:
            parser.error("--impact needs --base")
        return run_impact(args)
    if args.report:
        return write_status_report(args)
    report = run(args.root, args.write_baseline, write_report=args.write_report)
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
