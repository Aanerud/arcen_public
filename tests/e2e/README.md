# End-to-End

Automated product flows using isolated test identities, machines, and
entitlements belong here. No production credentials or customer data are
permitted.

## Observability conformance

`observability/` contains rights-safe synthetic Deck and Pier JSONL. It does
not exercise hardware or claim platform integration coverage. Validate one or
more process logs. Lifecycle records are checked against all 47 canonical event
definitions, including exact metadata and closed field schemas:

```sh
python3 scripts/validate_observability.py path/to/process.jsonl [...]
```

Validate each synthetic cross-file lifecycle (SID join, per-process sequence,
event order, timestamp and summary tolerances):

```sh
python3 scripts/validate_observability.py --cross-file \
  tests/e2e/observability/linux-session/macos-deck.jsonl \
  tests/e2e/observability/linux-session/linux-pier.jsonl
python3 scripts/validate_observability.py --cross-file \
  tests/e2e/observability/windows-session/macos-deck.jsonl \
  tests/e2e/observability/windows-session/windows-pier.jsonl
```

Run the stdlib-only negative and fixture tests with:

```sh
python3 -m unittest scripts/test_validate_observability.py
```

Cross-file mode validates one synthetic Deck+Pier session. It allows 30 seconds
of clock skew, 5 seconds of duration skew, and 5% (at least five frames) of
summary skew; timestamps need not be identical. Sequence is compared per file,
and cross-file mode never compares Deck and Pier sequence values.
