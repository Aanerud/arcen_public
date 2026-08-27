import copy
import importlib.util
import io
import json
import pathlib
import re
import sys
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
SPEC = importlib.util.spec_from_file_location(
    "validate_observability", ROOT / "scripts" / "validate_observability.py"
)
VALIDATOR = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
sys.modules[SPEC.name] = VALIDATOR
SPEC.loader.exec_module(VALIDATOR)
FIXTURES = ROOT / "tests" / "e2e" / "observability"
FROZEN_CANONICAL_FIXTURE = (
    ROOT
    / "shared"
    / "telemetry"
    / "tests"
    / "fixtures"
    / "canonical-record-v1.jsonl"
)


def fixture_records(path):
    return [
        json.loads(line)
        for line in path.read_text(encoding="utf-8").splitlines()
    ]


def encoded(records):
    return "".join(
        json.dumps(record, separators=(",", ":")) + "\n" for record in records
    ).encode("utf-8")


def validate_documents(*documents, cross_file=False):
    context = VALIDATOR.ValidationContext()
    for index, document in enumerate(documents):
        VALIDATOR.validate_stream(
            io.BytesIO(document),
            f"synthetic-{index}.jsonl",
            context=context,
        )
    if cross_file:
        VALIDATOR.validate_cross_file_lifecycle(context.lifecycle)


def sample_value(field_type):
    return {
        VALIDATOR.BOOLEAN: True,
        VALIDATOR.INTEGER: 1,
        VALIDATOR.STRING: "synthetic",
    }[field_type]


def event_record(event_id, *, include_optional=True):
    definition = VALIDATOR.EVENT_DEFINITIONS[event_id]
    field_types = dict(definition.required_fields)
    if include_optional:
        field_types.update(definition.optional_fields)
    return {
        "schema_version": 1,
        "timestamp": "2026-07-25T00:00:00.000000Z",
        "sequence": 1,
        "profile_level": definition.profile_level,
        "profile_name": definition.profile_name,
        "severity": definition.severity,
        "role": "client",
        "component": "deck",
        "platform": "macos",
        "target": "arcen::telemetry",
        "event_id": event_id,
        "event_name": definition.name,
        "category": definition.category,
        "outcome": definition.outcome,
        "sid": None,
        "user": None,
        "host": None,
        "peer_addr": None,
        "health_state": None,
        "message": "synthetic lifecycle event",
        "fields": {
            key: sample_value(field_type) for key, field_type in field_types.items()
        },
    }


def rust_event_definitions():
    source = (
        ROOT / "shared" / "telemetry" / "src" / "lifecycle.rs"
    ).read_text(encoding="utf-8")
    kinds = {
        name: int(identifier)
        for name, identifier in re.findall(r"^\s+(\w+) = (\d+),$", source, re.MULTILINE)
    }
    field_sets = {}
    for name, body in re.findall(
        r"const (\w+_FIELDS): &\[LifecycleFieldSpec\]\s*=\s*&\[(.*?)\];",
        source,
        re.DOTALL,
    ):
        required = {}
        optional = {}
        for requirement, key, field_type in re.findall(
            r'(required|optional)\("([^"]+)", LifecycleFieldType::(\w+)\)',
            body,
        ):
            target = required if requirement == "required" else optional
            target[key] = field_type.lower()
        field_sets[name] = (required, optional)

    profile_names = {"Critical": (0, "critical"), "Error": (1, "error"), "Info": (2, "info")}
    severities = {"Information": "info", "Warning": "warn", "Error": "error"}

    def snake_case(name):
        return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()

    definitions = {}
    table = source[source.index("pub static LIFECYCLE_EVENT_DEFINITIONS") :]
    for body in re.findall(r"LifecycleEventDefinition \{(.*?)\n    \},", table, re.DOTALL):
        kind = re.search(r"kind: LifecycleEventKind::(\w+)", body).group(1)
        name = re.search(r'name: "([^"]+)"', body).group(1)
        category = re.search(r"category: LifecycleCategory::(\w+)", body).group(1)
        outcome = re.search(r"outcome: EventOutcome::(\w+)", body).group(1)
        severity = re.search(r"severity: LifecycleSeverity::(\w+)", body).group(1)
        profile = re.search(r"minimum_profile: OperationalProfile::(\w+)", body).group(1)
        fields = re.search(r"fields: (\w+_FIELDS)", body).group(1)
        level, profile_name = profile_names[profile]
        required, optional = field_sets[fields]
        definitions[kinds[kind]] = (
            name,
            level,
            profile_name,
            severities[severity],
            snake_case(category),
            snake_case(outcome),
            required,
            optional,
        )
    return definitions


class ObservabilityValidationTests(unittest.TestCase):
    def setUp(self):
        self.linux_deck = fixture_records(
            FIXTURES / "linux-session" / "macos-deck.jsonl"
        )
        self.linux_pier = fixture_records(
            FIXTURES / "linux-session" / "linux-pier.jsonl"
        )

    def assert_rejected(self, records, expected):
        with self.assertRaisesRegex(VALIDATOR.ValidationError, expected):
            validate_documents(encoded(records))

    def test_rights_safe_linux_and_windows_lifecycles_conform(self):
        for directory, client, host in (
            ("linux-session", "macos-deck.jsonl", "linux-pier.jsonl"),
            ("windows-session", "macos-deck.jsonl", "windows-pier.jsonl"),
        ):
            with self.subTest(directory=directory):
                VALIDATOR.validate_paths(
                    [FIXTURES / directory / client, FIXTURES / directory / host],
                    cross_file=True,
                )

    def test_static_table_matches_all_47_rust_event_definitions(self):
        static = {
            event_id: tuple(definition)
            for event_id, definition in VALIDATOR.EVENT_DEFINITIONS.items()
        }
        self.assertEqual(len(static), 47)
        self.assertEqual(static, rust_event_definitions())

    def test_frozen_canonical_record_fixture_conforms(self):
        records = fixture_records(FROZEN_CANONICAL_FIXTURE)
        self.assertEqual(len(records), 1)
        self.assertEqual(records[0].get("event_id"), 1100)
        VALIDATOR.validate_paths([FROZEN_CANONICAL_FIXTURE])

    def test_all_47_event_definitions_accept_required_and_optional_fields(self):
        self.assertEqual(len(VALIDATOR.EVENT_DEFINITIONS), 47)
        for event_id in VALIDATOR.EVENT_DEFINITIONS:
            with self.subTest(event_id=event_id):
                validate_documents(encoded([event_record(event_id)]))

    def test_malformed_json_is_rejected_without_echoing_content(self):
        hostile = b'{"schema_version":1,"message":"do-not-echo"\n'
        with self.assertRaises(VALIDATOR.ValidationError) as caught:
            validate_documents(hostile)
        self.assertIn("malformed JSON", str(caught.exception))
        self.assertNotIn("do-not-echo", str(caught.exception))

    def test_duplicate_and_regressing_sequences_are_rejected(self):
        duplicate = copy.deepcopy(self.linux_deck[:2])
        duplicate[1]["sequence"] = duplicate[0]["sequence"]
        self.assert_rejected(duplicate, "sequence is duplicated")

        regressing = copy.deepcopy(self.linux_deck[:2])
        regressing[0]["sequence"] = 2
        regressing[1]["sequence"] = 1
        self.assert_rejected(regressing, "sequence regressed")

    def test_sequences_are_scoped_only_to_each_input_stream(self):
        first = encoded(self.linux_deck[:1])
        second = encoded(self.linux_deck[:1])
        validate_documents(first, second)

    def test_cross_file_mode_never_compares_deck_and_pier_sequence(self):
        client = copy.deepcopy(self.linux_deck)
        host = copy.deepcopy(self.linux_pier)
        for sequence, record in enumerate(client, start=100):
            record["sequence"] = sequence
        for sequence, record in enumerate(host, start=100):
            record["sequence"] = sequence
        validate_documents(encoded(client), encoded(host), cross_file=True)

    def test_nested_identity_is_rejected(self):
        records = copy.deepcopy(self.linux_deck[:1])
        records[0]["fields"]["user"] = "synthetic-user"
        self.assert_rejected(records, "reserved identity key")

    def test_bad_profile_name_and_event_pair_are_rejected(self):
        records = copy.deepcopy(self.linux_deck[:1])
        records[0]["profile_name"] = "debug"
        self.assert_rejected(records, "profile_level and profile_name")

        records = copy.deepcopy(self.linux_deck[:1])
        records[0]["event_name"] = "CLIENT_CONNECT_FAIL"
        self.assert_rejected(records, "canonical pair")

    def test_event_definition_rejects_metadata_and_field_schema_drift(self):
        probes = []
        for key, replacement in (
            ("severity", "warn"),
            ("category", "network"),
            ("outcome", "failed"),
        ):
            record = event_record(1503)
            record[key] = replacement
            probes.append((record, "metadata"))

        record = event_record(1503)
        record["profile_level"] = 1
        record["profile_name"] = "error"
        probes.append((record, "metadata"))

        record = event_record(1503)
        del record["category"]
        probes.append((record, "metadata"))

        record = event_record(1503)
        record["event_revision"] = 1
        probes.append((record, "unsupported top-level keys"))

        record = event_record(1503)
        del record["fields"]["tls_version"]
        probes.append((record, "missing required fields"))

        record = event_record(1503)
        record["fields"]["unexpected"] = 1
        probes.append((record, "unsupported fields"))

        record = event_record(1503)
        record["fields"]["tls_version"] = 1
        probes.append((record, "wrong type"))

        record = event_record(1503)
        record["fields"]["rtt_ms"] = "one"
        probes.append((record, "wrong type"))

        record = event_record(1200)
        record["fields"]["changed"] = 1
        probes.append((record, "wrong type"))

        for record, expected in probes:
            with self.subTest(expected=expected):
                self.assert_rejected([record], expected)

    def test_structured_field_strings_may_be_empty(self):
        record = event_record(1502, include_optional=False)
        record["fields"]["transport"] = ""
        validate_documents(encoded([record]))

    def test_lone_surrogates_are_rejected_without_echo_or_traceback(self):
        probes = []
        for key, value in (
            ("component", "\ud800"),
            ("target", "arcen::\ud800"),
            ("message", "\ud800"),
            ("sid", "\ud800"),
            ("user", "\ud800"),
        ):
            record = event_record(1502, include_optional=False)
            record[key] = value
            probes.append(record)

        record = event_record(1502, include_optional=False)
        record["fields"]["transport"] = "\ud800"
        probes.append(record)
        record = event_record(1502, include_optional=False)
        record["fields"] = {"\ud800": "synthetic"}
        probes.append(record)

        for record in probes:
            with self.subTest(record=record):
                with self.assertRaises(VALIDATOR.ValidationError) as caught:
                    validate_documents(encoded([record]))
                rendered = str(caught.exception)
                self.assertNotIn("\\ud800", rendered)
                self.assertNotIn("Traceback", rendered)

    def test_non_scalar_enum_types_are_rejected_concisely(self):
        for key in ("severity", "role", "platform", "health_state"):
            with self.subTest(key=key):
                records = copy.deepcopy(self.linux_deck[:1])
                records[0][key] = []
                self.assert_rejected(records, key)

    def test_unsupported_schema_is_rejected(self):
        records = copy.deepcopy(self.linux_deck[:1])
        records[0]["schema_version"] = 2
        self.assert_rejected(records, "unsupported schema_version")

    def test_mismatched_sid_is_rejected(self):
        host = copy.deepcopy(self.linux_pier)
        for record in host:
            record["sid"] = "different-synthetic-session"
        with self.assertRaisesRegex(VALIDATOR.ValidationError, "sid values do not match"):
            validate_documents(
                encoded(self.linux_deck), encoded(host), cross_file=True
            )

    def test_malformed_lifecycle_order_is_rejected(self):
        client = copy.deepcopy(self.linux_deck)
        degraded = next(
            index
            for index, record in enumerate(client)
            if record.get("event_id") == 1801
        )
        healthy = next(
            index
            for index, record in enumerate(client)
            if record.get("event_id") == 1800
        )
        client[degraded], client[healthy] = client[healthy], client[degraded]
        for sequence, record in enumerate(client, start=1):
            record["sequence"] = sequence
        with self.assertRaisesRegex(VALIDATOR.ValidationError, "out of order"):
            validate_documents(
                encoded(client), encoded(self.linux_pier), cross_file=True
            )

    def test_line_and_file_count_bounds_are_enforced(self):
        oversized = b"{" + b" " * VALIDATOR.MAX_LINE_BYTES + b"}\n"
        with self.assertRaisesRegex(VALIDATOR.ValidationError, "line exceeds"):
            validate_documents(oversized)

        original_limit = VALIDATOR.MAX_LINES_PER_FILE
        VALIDATOR.MAX_LINES_PER_FILE = 1
        try:
            with self.assertRaisesRegex(VALIDATOR.ValidationError, "line count"):
                validate_documents(encoded(self.linux_deck[:2]))
        finally:
            VALIDATOR.MAX_LINES_PER_FILE = original_limit

        with self.assertRaisesRegex(VALIDATOR.ValidationError, "maximum file count"):
            VALIDATOR.validate_paths(
                [pathlib.Path(f"synthetic-{index}.jsonl") for index in range(33)]
            )


if __name__ == "__main__":
    unittest.main()
