#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS

"""Keep docs/operations/error-codes.md in step with Arcen's typed reasons.

Arcen reports a failure as a stable token, never as free text alone: the
connection test's finding codes, the wire's resume error codes, the Linux
launcher's reason classes, polkit and seat reasons, the Wayland provider's
blockers and qualification results, the transport's lane refusals, and the
numbered log events. This script reads every one of those from source and
fails when a token is missing from the reference, a row names a token that no
longer exists, or a row has no meaning or action.

  python3 scripts/error_code_reference.py           # check (CI, tests)
  python3 scripts/error_code_reference.py --write   # add stub rows for new codes
  python3 scripts/error_code_reference.py --dump    # print every code with its source doc
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass
from pathlib import Path


REPO = Path(__file__).resolve().parent.parent
DOCUMENT = REPO / "docs" / "operations" / "error-codes.md"
COLUMNS = ["Code", "Meaning", "What to do"]

sys.path.insert(0, str(Path(__file__).resolve().parent))


class ExtractionError(Exception):
    """A code source could not be read completely."""


@dataclass(frozen=True)
class Source:
    """Where one table's codes come from.

    `delegates` names the variants whose arm returns another enum's code
    (`reason.token()`, `Other::Variant.token()`) and the source of that enum,
    which is itself read and checked. Any other arm that is not a string
    literal, and any catch-all arm, is an error: a code the script cannot see
    is a code the reference can silently miss.
    """

    heading: str
    path: str
    enum: str
    function: str | None = None
    serde: bool = False
    enum_path: str | None = None
    delegates: tuple[tuple[str, "Source"], ...] = ()


def _read(root: Path, relative: str) -> str:
    path = root / relative
    if not path.exists():
        raise ExtractionError(f"{relative} not found")
    return path.read_text(encoding="utf-8")


def _source_tokens(root: Path, source: Source) -> dict[str, str]:
    """Token -> the doc comment of the variant(s) it names."""
    return _extract(root, source)[0]


def _extract(root: Path, source: Source) -> tuple[dict[str, str], dict[str, list[str]]]:
    """(token -> doc, variant -> its tokens), every variant accounted for."""
    docs = _variant_docs(_read(root, source.enum_path or source.path), source.enum)
    if source.serde:
        return (
            {_snake(variant): doc for variant, doc in docs.items()},
            {variant: [_snake(variant)] for variant in docs},
        )
    function = source.function or "token"
    body = _function_body(_read(root, source.path), source.enum, function)
    delegates = dict(source.delegates)
    tokens: dict[str, str] = {}
    by_variant: dict[str, list[str]] = {}
    where = f"{source.enum} {function}()"
    for pattern, expression in _match_arms(body):
        alternatives = [part.strip() for part in _split_top_level(pattern, "|")]
        if any(re.fullmatch(r"_|[a-z_]\w*", part) for part in alternatives):
            raise ExtractionError(
                f"{where} has a catch-all arm `{pattern.strip()}`; name every variant"
            )
        variants = [
            name
            for part in alternatives
            for name in re.findall(r"^(?:Self|\w+)::(\w+)", part)
            if name in docs
        ]
        if not variants:
            raise ExtractionError(f"{where}: arm `{pattern.strip()}` names no {source.enum} variant")
        literal = re.fullmatch(r'"([^"\\]+)"', expression)
        if literal:
            doc = " ".join(docs.get(variant, "") for variant in variants).strip()
            tokens.setdefault(literal.group(1), doc)
            for variant in variants:
                by_variant[variant] = [literal.group(1)]
            continue
        undeclared = [variant for variant in variants if variant not in delegates]
        if undeclared:
            raise ExtractionError(
                f"{where}: {', '.join(undeclared)} returns `{expression}`, not a literal code; "
                "declare the delegation in Source.delegates"
            )
        delegate = delegates[variants[0]]
        delegate_tokens, delegate_variants = _extract(root, delegate)
        delegate_function = delegate.function or "token"
        one = re.fullmatch(
            rf"{delegate.enum}::(\w+)\.{delegate_function}\(\)", expression
        )
        whole = re.fullmatch(rf"[a-z_]\w*\.{delegate_function}\(\)", expression)
        if one and one.group(1) in delegate_variants:
            chosen = delegate_variants[one.group(1)]
        elif whole:
            chosen = sorted(delegate_tokens)
        else:
            raise ExtractionError(
                f"{where}: `{expression}` is not a call of {delegate.enum}::{delegate_function}()"
            )
        for token in chosen:
            tokens.setdefault(token, delegate_tokens[token])
        for variant in variants:
            by_variant[variant] = list(chosen)
    missing = sorted(set(docs) - set(by_variant))
    if missing:
        raise ExtractionError(f"{where} gives no code for {', '.join(missing)}")
    return tokens, by_variant


def _split_top_level(text: str, separator: str) -> list[str]:
    parts, depth, current = [], 0, []
    for character in text:
        depth += {"(": 1, "{": 1, "[": 1, ")": -1, "}": -1, "]": -1}.get(character, 0)
        if character == separator and depth == 0:
            parts.append("".join(current))
            current = []
        else:
            current.append(character)
    parts.append("".join(current))
    return parts


def _match_arms(body: str) -> list[tuple[str, str]]:
    """(pattern, expression) for each arm of the function's `match`."""
    found = re.search(r"\bmatch\b[^{]*\{", body)
    if not found:
        raise ExtractionError("no match expression")
    text = body[found.end() :]
    arms: list[tuple[str, str]] = []
    depth, index, start, in_string = 0, 0, 0, False
    pattern = None
    while index < len(text):
        character = text[index]
        if in_string:
            if character == "\\":
                index += 1
            elif character == '"':
                in_string = False
        elif character == '"':
            in_string = True
        elif character in "({[":
            depth += 1
        elif character in ")}]":
            if depth == 0:
                if pattern is not None and text[start:index].strip():
                    arms.append((pattern, _expression(text[start:index])))
                return arms
            depth -= 1
        elif depth == 0 and text.startswith("=>", index):
            pattern = text[start:index]
            index += 2
            start = index
            continue
        elif depth == 0 and character == "," and pattern is not None:
            arms.append((pattern, _expression(text[start:index])))
            pattern = None
            start = index + 1
        elif depth == 0 and character == "}" and pattern is not None:
            pass
        index += 1
        if (
            pattern is not None
            and depth == 0
            and not in_string
            and text[start:index].strip().startswith("{")
            and text[start:index].rstrip().endswith("}")
            and text[start:index].count("{") == text[start:index].count("}")
        ):
            arms.append((pattern, _expression(text[start:index])))
            pattern = None
            start = index
    raise ExtractionError("unbalanced match expression")


def _expression(text: str) -> str:
    expression = text.strip().rstrip(",").strip()
    while expression.startswith("{") and expression.endswith("}"):
        expression = expression[1:-1].strip()
    return expression


def _snake(name: str) -> str:
    return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()


def _variant_docs(text: str, enum: str) -> dict[str, str]:
    match = re.search(rf"\benum\s+{enum}\b[^{{]*\{{", text)
    if not match:
        raise ExtractionError(f"enum {enum} not found")
    docs: dict[str, str] = {}
    pending: list[str] = []
    depth = 1
    for line in text[match.end() :].splitlines():
        stripped = line.strip()
        if depth == 1 and stripped.startswith("///"):
            pending.append(stripped[3:].strip())
        elif depth == 1 and re.match(r"^[A-Z]\w*", stripped):
            name = re.match(r"^([A-Z]\w*)", stripped).group(1)
            docs[name] = " ".join(pending)
            pending = []
        elif not stripped.startswith("#["):
            pending = [] if depth == 1 and stripped else pending
        depth += stripped.count("{") - stripped.count("}")
        if depth <= 0:
            break
    return docs


def _function_body(text: str, owner: str, function: str) -> str:
    """The body of `function` in `impl owner`, or a free fn taking `owner`."""

    def belongs(position: int) -> bool:
        signature = text[position : text.index("{", position)]
        if re.search(rf"\b{owner}\b", signature):
            return True
        headers = list(re.finditer(r"\bimpl\b[^{;]*\{", text[:position]))
        if not headers:
            return False
        header = headers[-1]
        between = text[header.end() : position]
        still_open = between.count("{") >= between.count("}")
        return still_open and re.search(rf"\b{owner}\b\s*(<[^>]*>)?\s*\{{$", header.group(0)) is not None

    candidates = [
        match.end() for match in re.finditer(rf"\bfn\s+{function}\s*\(", text) if belongs(match.start())
    ]
    if not candidates:
        raise ExtractionError(f"fn {function} for {owner} not found")
    start = text.index("{", candidates[0])
    depth = 0
    for index in range(start, len(text)):
        depth += {"{": 1, "}": -1}.get(text[index], 0)
        if depth == 0:
            return text[start : index + 1]
    raise ExtractionError(f"unbalanced fn {function}")


POLKIT = Source(
    "## Linux sign-in refusals",
    "shared/session/src/polkit_remote.rs",
    "PolkitUnprotected",
)

SOURCES = [
    Source(
        "## Connection test",
        "shared/session/src/connection_diagnostics.rs",
        "Finding",
        "code",
    ),
    Source(
        "## Colour check",
        "shared/media/src/colour_check.rs",
        "Finding",
        "code",
    ),
    Source(
        "## Session resume refusals",
        "shared/protocol/src/messages.rs",
        "ResumeErrorCode",
        serde=True,
    ),
    Source(
        "## Linux sign-in refusals",
        "hosts/linux/src/net/server.rs",
        "LauncherError",
        "launcher_error_reason_class",
        enum_path="hosts/linux/src/session/launcher.rs",
        delegates=(("PolkitUnprotected", POLKIT),),
    ),
    Source(
        "## Taking over a disconnected desktop",
        "shared/session/src/session_admission.rs",
        "DetachedTakeoverRefusal",
    ),
    Source(
        "## Deskside privacy refusals",
        "shared/session/src/deskside.rs",
        "DesksideRefusalReason",
        "as_str",
    ),
    Source(
        "## Native Wayland seat",
        "shared/session/src/native_seat.rs",
        "NativeSeatUnavailable",
        delegates=(
            ("PolkitNoRules", POLKIT),
            ("PolkitRuleMissing", POLKIT),
            ("PolkitRuleIneffective", POLKIT),
            ("PolkitUnverified", POLKIT),
        ),
    ),
    Source(
        "## Wayland provider blockers",
        "shared/session/src/desktop_provider.rs",
        "WaylandBlocker",
    ),
    Source(
        "## Wayland qualification",
        "shared/session/src/wayland_qualification.rs",
        "StaleReason",
    ),
    Source(
        "## Wayland qualification",
        "shared/session/src/wayland_qualification.rs",
        "QualificationError",
    ),
    Source(
        "## Grading and HDR refusals",
        "shared/media/src/video/pipeline/software.rs",
        "FidelityRefusal",
        "code",
    ),
    Source(
        "## Windows HDR session refusals",
        "shared/session/src/setup_budget.rs",
        "HdrSetupRefusal",
        "code",
    ),
    Source(
        "## Linux installer refusals",
        "shared/session/src/install_provider.rs",
        "InstallRefusal",
    ),
    Source(
        "## Transport lane refusals",
        "shared/transport/src/lanes.rs",
        "LaneRefusal",
    ),
]
EVENTS_HEADING = "## Log events that need attention"


def expected_codes(root: Path = REPO) -> dict[str, dict[str, str]]:
    """Section heading -> code -> source doc."""
    sections: dict[str, dict[str, str]] = {}
    for source in SOURCES:
        sections.setdefault(source.heading, {}).update(_source_tokens(root, source))
    from observability_event_definitions import EVENT_DEFINITIONS

    sections[EVENTS_HEADING] = {
        f"{identifier} {definition.name}": f"{definition.severity}, {definition.category}"
        for identifier, definition in sorted(EVENT_DEFINITIONS.items())
        if definition.severity != "info"
    }
    return sections


@dataclass
class Row:
    code: str
    cells: list[str]
    line: int


def _code_of(cell: str) -> str:
    event = re.fullmatch(r"(\d+)\s+`([A-Z_]+)`", cell)
    if event:
        return f"{event.group(1)} {event.group(2)}"
    plain = re.fullmatch(r"`([^`]+)`", cell)
    return plain.group(1) if plain else cell


def read_document(text: str, headings: set[str]) -> dict[str, list[Row]]:
    sections: dict[str, list[Row]] = {}
    current = None
    for number, line in enumerate(text.splitlines(), start=1):
        if line.startswith("## "):
            current = line.strip() if line.strip() in headings else None
            if current is not None:
                sections.setdefault(current, [])
            continue
        if current is None or not line.startswith("|"):
            continue
        cells = [cell.strip() for cell in line.strip().strip("|").split("|")]
        if cells[0] in ("Code", "Event", "") or set(cells[0]) <= set("-: "):
            continue
        sections[current].append(Row(_code_of(cells[0]), cells, number))
    return sections


def check(text: str, expected: dict[str, dict[str, str]]) -> list[str]:
    problems = []
    documented = read_document(text, set(expected))
    for heading, codes in expected.items():
        rows = documented.get(heading)
        if rows is None:
            problems.append(f"missing section {heading!r}")
            continue
        seen = set()
        for row in rows:
            if row.code in seen:
                problems.append(f"line {row.line}: {row.code} is listed twice")
            seen.add(row.code)
            if row.code not in codes:
                problems.append(f"line {row.line}: {row.code} is not a code Arcen produces")
            if len(row.cells) != len(COLUMNS):
                problems.append(f"line {row.line}: expected {len(COLUMNS)} columns")
                continue
            for column, cell in zip(COLUMNS[1:], row.cells[1:]):
                if not cell or "TODO" in cell:
                    problems.append(f"line {row.line}: {row.code} has no {column}")
        for code in sorted(set(codes) - seen):
            problems.append(
                f"{heading}: {code} is produced by the code but undocumented "
                "(run scripts/error_code_reference.py --write and fill in the row)"
            )
    return problems


def stub_rows(text: str, expected: dict[str, dict[str, str]]) -> str:
    documented = read_document(text, set(expected))
    insertions: dict[int, list[str]] = {}
    for heading, codes in expected.items():
        rows = documented.get(heading, [])
        missing = sorted(set(codes) - {row.code for row in rows})
        if not missing or not rows:
            continue

        def render(code: str) -> str:
            if heading == EVENTS_HEADING:
                identifier, name = code.split(" ", 1)
                return f"{identifier} `{name}`"
            return f"`{code}`"

        insertions[rows[-1].line] = [
            f"| {render(code)} | TODO {codes[code]} | TODO |" for code in missing
        ]
    output = []
    for number, line in enumerate(text.splitlines(), start=1):
        output.append(line)
        output.extend(insertions.get(number, []))
    return "\n".join(output) + "\n"


def sources_present(root: Path = REPO) -> bool:
    """Whether `root` holds the code the reference describes (not a fixture)."""
    return any((root / source.path).exists() for source in SOURCES)


def check_repository(root: Path = REPO) -> list[str]:
    """Problems with the reference in `root`, for other checkers."""
    document = root / "docs" / "operations" / "error-codes.md"
    if not document.exists():
        return [f"{document.relative_to(root)} is missing"]
    try:
        expected = expected_codes(root)
    except ExtractionError as error:
        return [str(error)]
    return check(document.read_text(encoding="utf-8"), expected)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--write", action="store_true", help="add stub rows for new codes")
    mode.add_argument("--dump", action="store_true", help="print every code")
    arguments = parser.parse_args(argv)
    try:
        expected = expected_codes()
    except ExtractionError as error:
        print(f"error-codes: {error}", file=sys.stderr)
        return 1
    if arguments.dump:
        for heading, codes in expected.items():
            print(heading)
            for code, doc in codes.items():
                print(f"  {code}\t{doc}")
        return 0
    if arguments.write:
        DOCUMENT.write_text(
            stub_rows(DOCUMENT.read_text(encoding="utf-8"), expected), encoding="utf-8"
        )
    problems = check(DOCUMENT.read_text(encoding="utf-8"), expected)
    for problem in problems:
        print(f"error-codes: {problem}", file=sys.stderr)
    if problems:
        return 1
    total = sum(len(codes) for codes in expected.values())
    print(f"error-codes: {total} codes documented and in sync")
    return 0


if __name__ == "__main__":
    sys.exit(main())
