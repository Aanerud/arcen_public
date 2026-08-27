#!/usr/bin/env python3
"""Validate bounded canonical observability JSONL and synthetic lifecycles."""

from __future__ import annotations

import argparse
import datetime as dt
import json
import math
import re
import unicodedata
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, BinaryIO

from observability_event_definitions import (
    BOOLEAN,
    EVENT_DEFINITIONS,
    INTEGER,
    STRING,
    EventDefinition,
)


MAX_FILES = 32
MAX_LINES_PER_FILE = 10_000
MAX_LINE_BYTES = 64 * 1024
MAX_FILE_BYTES = 16 * 1024 * 1024
MAX_FIELDS = 16
MAX_FIELD_KEY_BYTES = 64
MAX_FIELD_STRING_BYTES = 512
MAX_MESSAGE_BYTES = 512
MAX_IDENTITY_BYTES = 256
MAX_CORRELATION_ID_BYTES = 128
MAX_CLOCK_SKEW_SECONDS = 30
MAX_DURATION_SKEW_MS = 5_000
FRAME_TOLERANCE_PERCENT = 5
MIN_FRAME_TOLERANCE = 5

PROFILE_NAMES = {0: "critical", 1: "error", 2: "info", 3: "debug"}
SEVERITIES = {"debug", "info", "warn", "error"}
ROLES = {"host", "client", "gateway"}
PLATFORMS = {"linux", "macos", "windows"}
HEALTH_STATES = {"ok", "degraded", "critical"}
IDENTITY_KEYS = {"sid", "user", "host", "peer_addr"}
SENSITIVE_KEY_PARTS = {
    "password",
    "secret",
    "token",
    "credential",
    "authorization",
    "cookie",
    "passphrase",
    "private_key",
    "key_path",
    "session_key",
}
REQUIRED_KEYS = {
    "schema_version",
    "timestamp",
    "sequence",
    "profile_level",
    "profile_name",
    "severity",
    "role",
    "component",
    "platform",
    "target",
    "sid",
    "user",
    "host",
    "peer_addr",
    "health_state",
    "message",
    "fields",
}
OPTIONAL_KEYS = {
    "event_id",
    "event_name",
    "category",
    "outcome",
}
CLIENT_ORDER = (1502, 1503, 1100, 1700, 1801, 1800, 1505)
HOST_ORDER = (1100, 1102, 1700, 1801, 1800, 1103)
LIFECYCLE_IDS = set(CLIENT_ORDER) | set(HOST_ORDER)
TIMESTAMP_RE = re.compile(
    r"\A[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{6}Z\Z"
)
SNAKE_RE = re.compile(r"\A[a-z0-9](?:[a-z0-9_]*[a-z0-9])?\Z")


class ValidationError(ValueError):
    """A concise validation failure that never includes record values."""


class _DuplicateKeyError(ValueError):
    pass


@dataclass(frozen=True)
class ObservedRecord:
    source: str
    line: int
    value: dict[str, Any]
    timestamp: dt.datetime


@dataclass
class ValidationContext:
    lifecycle: list[ObservedRecord] = field(default_factory=list)


def _fail(source: str, line: int, reason: str) -> ValidationError:
    return ValidationError(f"{source}:{line}: {reason}")


def _is_int(value: Any) -> bool:
    return type(value) is int


def _has_control(value: str) -> bool:
    return any(unicodedata.category(character) == "Cc" for character in value)


def _utf8_length(value: str) -> int | None:
    try:
        return len(value.encode("utf-8"))
    except UnicodeEncodeError:
        return None


def _bounded_text(value: Any, maximum: int, *, allow_empty: bool = False) -> bool:
    if not isinstance(value, str) or (not allow_empty and not value):
        return False
    length = _utf8_length(value)
    return length is not None and length <= maximum and not _has_control(value)


def _unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise _DuplicateKeyError
        result[key] = value
    return result


def _parse_timestamp(value: Any, source: str, line: int) -> dt.datetime:
    if not isinstance(value, str) or TIMESTAMP_RE.fullmatch(value) is None:
        raise _fail(source, line, "timestamp is not canonical UTC microsecond form")
    try:
        return dt.datetime.strptime(value, "%Y-%m-%dT%H:%M:%S.%fZ").replace(
            tzinfo=dt.timezone.utc
        )
    except ValueError:
        raise _fail(source, line, "timestamp is not a valid calendar time") from None


def _validate_component(value: Any, source: str, line: int) -> None:
    length = _utf8_length(value) if isinstance(value, str) else None
    if (
        not isinstance(value, str)
        or length is None
        or length > 32
        or SNAKE_RE.fullmatch(value) is None
    ):
        raise _fail(source, line, "component is not canonical lowercase snake case")


def _validate_target(value: Any, source: str, line: int) -> None:
    length = _utf8_length(value) if isinstance(value, str) else None
    if not isinstance(value, str) or length is None or length > 64:
        raise _fail(source, line, "target is not a canonical lowercase arcen target")
    parts = value.split("::")
    if (
        len(parts) < 2
        or parts[0] != "arcen"
        or any(
            _utf8_length(part) is None
            or _utf8_length(part) > 32
            or SNAKE_RE.fullmatch(part) is None
            for part in parts
        )
    ):
        raise _fail(source, line, "target is not a canonical lowercase arcen target")


def _validate_identity(
    value: Any, maximum: int, field_name: str, source: str, line: int
) -> None:
    if value is not None and not _bounded_text(value, maximum):
        raise _fail(source, line, f"{field_name} identity is invalid or oversized")


def _validate_fields(value: Any, source: str, line: int) -> None:
    if not isinstance(value, dict):
        raise _fail(source, line, "fields must be an object")
    if len(value) > MAX_FIELDS:
        raise _fail(source, line, "fields exceeds the maximum field count")
    for key, item in value.items():
        key_length = _utf8_length(key) if isinstance(key, str) else None
        if (
            not isinstance(key, str)
            or key_length is None
            or key_length > MAX_FIELD_KEY_BYTES
            or re.fullmatch(r"[a-z0-9_]+", key) is None
        ):
            raise _fail(source, line, "fields contains an invalid key")
        if key in IDENTITY_KEYS:
            raise _fail(source, line, "fields contains a reserved identity key")
        if any(part in key for part in SENSITIVE_KEY_PARTS):
            raise _fail(source, line, "fields contains a sensitive key")
        if isinstance(item, bool):
            continue
        if _is_int(item) and -(2**63) <= item <= 2**63 - 1:
            continue
        if _bounded_text(item, MAX_FIELD_STRING_BYTES, allow_empty=True):
            continue
        raise _fail(source, line, "fields contains an invalid or oversized value")


def _field_matches_type(value: Any, field_type: str) -> bool:
    if field_type == BOOLEAN:
        return isinstance(value, bool)
    if field_type == INTEGER:
        return _is_int(value) and -(2**63) <= value <= 2**63 - 1
    if field_type == STRING:
        return _bounded_text(value, MAX_FIELD_STRING_BYTES, allow_empty=True)
    return False


def _validate_event_fields(
    fields: dict[str, Any],
    definition: EventDefinition,
    source: str,
    line: int,
) -> None:
    required = definition.required_fields.keys()
    allowed = required | definition.optional_fields.keys()
    if required - fields.keys():
        raise _fail(source, line, "lifecycle event is missing required fields")
    if fields.keys() - allowed:
        raise _fail(source, line, "lifecycle event contains unsupported fields")
    field_types = definition.required_fields | definition.optional_fields
    if any(
        not _field_matches_type(fields[key], field_types[key])
        for key in fields
    ):
        raise _fail(source, line, "lifecycle event field has the wrong type")


def _validate_event(value: dict[str, Any], source: str, line: int) -> None:
    has_id = "event_id" in value
    has_name = "event_name" in value
    if has_id != has_name:
        raise _fail(source, line, "event_id and event_name must appear together")
    has_category = "category" in value
    has_outcome = "outcome" in value
    if not has_id:
        if has_category or has_outcome:
            raise _fail(source, line, "event metadata requires event_id and event_name")
        return
    event_id = value["event_id"]
    event_name = value["event_name"]
    definition = EVENT_DEFINITIONS.get(event_id) if _is_int(event_id) else None
    if definition is None or event_name != definition.name:
        raise _fail(source, line, "event_id and event_name are not a canonical pair")
    expected_metadata = {
        "profile_level": definition.profile_level,
        "profile_name": definition.profile_name,
        "severity": definition.severity,
        "category": definition.category,
        "outcome": definition.outcome,
    }
    if any(value.get(key) != expected for key, expected in expected_metadata.items()):
        raise _fail(source, line, "lifecycle event metadata does not match its definition")
    _validate_event_fields(value["fields"], definition, source, line)


def validate_record(
    value: Any,
    source: str,
    line: int,
    context: ValidationContext,
) -> int:
    if not isinstance(value, dict):
        raise _fail(source, line, "JSON value must be an object")
    missing = REQUIRED_KEYS - value.keys()
    if missing:
        raise _fail(source, line, "record is missing required top-level keys")
    if value.keys() - REQUIRED_KEYS - OPTIONAL_KEYS:
        raise _fail(source, line, "record contains unsupported top-level keys")

    if not _is_int(value["schema_version"]) or value["schema_version"] != 1:
        raise _fail(source, line, "unsupported schema_version")
    timestamp = _parse_timestamp(value["timestamp"], source, line)
    sequence = value["sequence"]
    if not _is_int(sequence) or not 0 <= sequence <= 2**64 - 1:
        raise _fail(source, line, "sequence must be an unsigned 64-bit integer")
    level = value["profile_level"]
    if not _is_int(level) or level not in PROFILE_NAMES:
        raise _fail(source, line, "profile_level must be an integer from 0 through 3")
    if value["profile_name"] != PROFILE_NAMES[level]:
        raise _fail(source, line, "profile_level and profile_name do not match")
    if not isinstance(value["severity"], str) or value["severity"] not in SEVERITIES:
        raise _fail(source, line, "severity is invalid")
    if not isinstance(value["role"], str) or value["role"] not in ROLES:
        raise _fail(source, line, "role is invalid")
    _validate_component(value["component"], source, line)
    if not isinstance(value["platform"], str) or value["platform"] not in PLATFORMS:
        raise _fail(source, line, "platform is invalid")
    _validate_target(value["target"], source, line)
    _validate_fields(value["fields"], source, line)
    _validate_event(value, source, line)

    _validate_identity(
        value["sid"], MAX_CORRELATION_ID_BYTES, "sid", source, line
    )
    for identity in ("user", "host", "peer_addr"):
        _validate_identity(
            value[identity], MAX_IDENTITY_BYTES, identity, source, line
        )
    if (
        value["health_state"] is not None
        and (
            not isinstance(value["health_state"], str)
            or value["health_state"] not in HEALTH_STATES
        )
    ):
        raise _fail(source, line, "health_state is invalid")
    if not _bounded_text(value["message"], MAX_MESSAGE_BYTES):
        raise _fail(source, line, "message is empty, invalid, or oversized")

    if value.get("event_id") in LIFECYCLE_IDS and value["role"] in {"client", "host"}:
        context.lifecycle.append(ObservedRecord(source, line, value, timestamp))
    return sequence


def validate_stream(
    stream: BinaryIO,
    source: str,
    *,
    context: ValidationContext | None = None,
) -> ValidationContext:
    """Incrementally validate one binary JSONL stream using fixed read bounds."""
    current = context or ValidationContext()
    total_bytes = 0
    records_read = 0
    previous_sequence = -1
    seen_sequences: set[int] = set()
    for line_number in range(1, MAX_LINES_PER_FILE + 1):
        raw = stream.readline(MAX_LINE_BYTES + 1)
        if not raw:
            if records_read == 0:
                raise _fail(source, 1, "file contains no JSONL records")
            return current
        records_read += 1
        total_bytes += len(raw)
        if len(raw) > MAX_LINE_BYTES:
            raise _fail(source, line_number, "line exceeds the maximum byte length")
        if total_bytes > MAX_FILE_BYTES:
            raise _fail(source, line_number, "file exceeds the maximum byte length")
        try:
            text = raw.decode("utf-8")
        except UnicodeDecodeError:
            raise _fail(source, line_number, "line is not valid UTF-8") from None
        if not text.strip():
            raise _fail(source, line_number, "blank JSONL lines are not allowed")
        try:
            value = json.loads(text, object_pairs_hook=_unique_object)
        except _DuplicateKeyError:
            raise _fail(source, line_number, "JSON object contains a duplicate key") from None
        except (json.JSONDecodeError, RecursionError):
            raise _fail(source, line_number, "malformed JSON") from None
        sequence = validate_record(value, source, line_number, current)
        if sequence in seen_sequences:
            raise _fail(source, line_number, "sequence is duplicated in its process stream")
        if sequence <= previous_sequence:
            raise _fail(source, line_number, "sequence regressed in its process stream")
        seen_sequences.add(sequence)
        previous_sequence = sequence
    if stream.read(1):
        raise _fail(
            source,
            MAX_LINES_PER_FILE + 1,
            "file exceeds the maximum line count",
        )
    return current


def _ordered_records(
    records: list[ObservedRecord], role: str, expected: tuple[int, ...]
) -> dict[int, ObservedRecord]:
    selected: dict[int, ObservedRecord] = {}
    cursor = -1
    for event_id in expected:
        match = next(
            (
                (index, record)
                for index, record in enumerate(records)
                if index > cursor
                and record.value["role"] == role
                and record.value.get("event_id") == event_id
            ),
            None,
        )
        if match is None:
            if any(
                record.value["role"] == role
                and record.value.get("event_id") == event_id
                for record in records
            ):
                raise ValidationError(f"{role} lifecycle events are out of order")
            raise ValidationError(f"{role} lifecycle is missing a required event")
        cursor, selected[event_id] = match
    return selected


def _require_summary_integer(
    record: ObservedRecord, key: str, description: str
) -> int:
    value = record.value["fields"].get(key)
    if not _is_int(value) or value < 0:
        raise ValidationError(f"{description} summary field is missing or invalid")
    return value


def _within_frame_tolerance(left: int, right: int) -> bool:
    tolerance = max(
        MIN_FRAME_TOLERANCE,
        math.ceil(max(left, right) * FRAME_TOLERANCE_PERCENT / 100),
    )
    return abs(left - right) <= tolerance


def validate_cross_file_lifecycle(records: list[ObservedRecord]) -> None:
    """Validate one synthetic Deck+Pier lifecycle joined by correlation ID."""
    if not records:
        raise ValidationError("cross-file lifecycle contains no recognized events")
    if any(record.value["sid"] is None for record in records):
        raise ValidationError("cross-file lifecycle event is missing sid")
    if len({record.value["sid"] for record in records}) != 1:
        raise ValidationError("cross-file lifecycle sid values do not match")

    client = _ordered_records(records, "client", CLIENT_ORDER)
    host = _ordered_records(records, "host", HOST_ORDER)
    paired = ((1100, 1100), (1700, 1700), (1801, 1801), (1800, 1800), (1505, 1103))
    for client_id, host_id in paired:
        skew = abs(
            (client[client_id].timestamp - host[host_id].timestamp).total_seconds()
        )
        if skew > MAX_CLOCK_SKEW_SECONDS:
            raise ValidationError("Deck and Pier lifecycle timestamps exceed tolerance")
    if (
        abs((client[1503].timestamp - host[1102].timestamp).total_seconds())
        > MAX_CLOCK_SKEW_SECONDS
    ):
        raise ValidationError("connect and stream timestamps exceed tolerance")

    client_duration = _require_summary_integer(
        client[1505], "duration_ms", "client session-end"
    )
    host_duration = _require_summary_integer(
        host[1103], "duration_ms", "host session-end"
    )
    if abs(client_duration - host_duration) > MAX_DURATION_SKEW_MS:
        raise ValidationError("Deck and Pier session durations exceed tolerance")
    client_frames = _require_summary_integer(
        client[1505], "frames_decoded", "client session-end"
    )
    host_frames = _require_summary_integer(
        host[1103], "frames_sent", "host session-end"
    )
    client_dropped = _require_summary_integer(
        client[1505], "frames_dropped", "client session-end"
    )
    host_dropped = _require_summary_integer(
        host[1103], "frames_dropped", "host session-end"
    )
    if not _within_frame_tolerance(client_frames, host_frames) or not _within_frame_tolerance(
        client_dropped, host_dropped
    ):
        raise ValidationError("Deck and Pier frame summaries exceed tolerance")


def validate_paths(paths: list[Path], *, cross_file: bool = False) -> None:
    if not paths:
        raise ValidationError("at least one JSONL file is required")
    if len(paths) > MAX_FILES:
        raise ValidationError("input exceeds the maximum file count")
    if cross_file and len(paths) < 2:
        raise ValidationError("cross-file mode requires at least two files")
    context = ValidationContext()
    for path in paths:
        with path.open("rb") as stream:
            validate_stream(
                stream,
                path.name,
                context=context,
            )
    if cross_file:
        validate_cross_file_lifecycle(context.lifecycle)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Validate bounded canonical Arcen observability JSONL."
    )
    parser.add_argument(
        "--cross-file",
        action="store_true",
        help="join one synthetic Deck+Pier lifecycle by sid and check ordering/tolerance",
    )
    parser.add_argument("files", nargs="+", type=Path)
    arguments = parser.parse_args()
    validate_paths(arguments.files, cross_file=arguments.cross_file)
    print(f"validated {len(arguments.files)} observability JSONL file(s)")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValidationError) as error:
        raise SystemExit(f"observability validation failed: {error}") from None
