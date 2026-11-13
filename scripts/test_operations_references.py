# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS

"""Tests for scripts/pier_config_reference.py and scripts/error_code_reference.py."""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import error_code_reference as codes  # noqa: E402
import pier_config_reference as config  # noqa: E402

SHARED = '''
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PierConfig<P> {
    #[serde(default)]
    pub listen: ListenConfig,
    pub platform: P,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ListenConfig {
    pub host: Option<String>,
    /// The port.
    #[serde(alias = "udp_port")]
    pub port: Option<u16>,
    #[serde(skip)]
    pub internal: bool,
    #[serde(rename = "renamed")]
    pub original: Vec<Pin>,
}

#[derive(Deserialize)]
pub struct Pin {
    pub name: String,
}
'''

LINUX = '''
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LinuxPlatformConfig {
    pub auth: LinuxAuthConfig,
}

#[derive(Deserialize)]
pub struct LinuxAuthConfig {
    pub mode: Option<String>,
}
'''

HEADER = "| Key | Type | Default | Since | Meaning | Takes effect |\n| --- | --- | --- | --- | --- | --- |\n"


def files(shared: str = SHARED) -> list[tuple[str, str]]:
    return [
        ("shared/session/src/pier_config.rs", shared),
        ("hosts/linux/src/config.rs", LINUX),
    ]


def document(common_rows: list[str], linux_rows: list[str]) -> str:
    return (
        "# Reference\n\n## Common keys\n\n"
        + HEADER
        + "".join(row + "\n" for row in common_rows)
        + "\n## Linux `platform.*` keys\n\n"
        + HEADER
        + "".join(row + "\n" for row in linux_rows)
        + "\n## Windows `platform.*` keys\n\n"
        + HEADER
        + "\n## macOS `platform.*` keys\n\n"
        + HEADER
    )


COMMON = [
    "| `listen.host` | string | unset | ≤0.13 | Bind address. | Restart |",
    "| `listen.port` | u16 | `18444` | ≤0.13 | UDP port. Alias `udp_port`. | Restart |",
    "| `listen.renamed[].name` | string | none | 0.18.0 | A name. | Restart |",
]
LINUX_ROWS = ["| `platform.auth.mode` | string | `pam` | ≤0.13 | PAM only. | Restart |"]


class ConfigReferenceTests(unittest.TestCase):
    def test_keys_follow_serde_names_aliases_skips_and_nesting(self) -> None:
        expected = config.expected_sections(files())
        self.assertEqual(
            expected["## Common keys"],
            {"listen.host": [], "listen.port": ["udp_port"], "listen.renamed[].name": []},
        )
        self.assertEqual(expected["## Linux `platform.*` keys"], {"platform.auth.mode": []})

    def test_a_complete_document_passes(self) -> None:
        expected = config.expected_sections(files())
        self.assertEqual(config.check(document(COMMON, LINUX_ROWS), expected), [])

    def test_a_new_key_without_a_row_fails_and_write_adds_a_stub(self) -> None:
        added = SHARED.replace(
            "    pub host: Option<String>,\n",
            "    pub host: Option<String>,\n    pub backlog: Option<u32>,\n",
        )
        expected = config.expected_sections(files(added))
        text = document(COMMON, LINUX_ROWS)
        problems = config.check(text, expected)
        self.assertTrue(any("listen.backlog" in problem for problem in problems), problems)
        stubbed = config.stub_rows(text, expected, lambda heading, key: "0.18.0")
        self.assertIn("| `listen.backlog` | TODO | TODO | 0.18.0 | TODO | TODO |", stubbed)
        self.assertTrue(any("has no Type" in problem for problem in config.check(stubbed, expected)))

    def test_rows_for_removed_keys_and_bad_releases_fail(self) -> None:
        expected = config.expected_sections(files())
        stale = COMMON + ["| `listen.gone` | bool | `false` | 0.14.0 | Gone. | Restart |"]
        self.assertTrue(
            any("listen.gone" in problem for problem in config.check(document(stale, LINUX_ROWS), expected))
        )
        undated = [COMMON[0].replace("≤0.13", "soon")] + COMMON[1:]
        self.assertTrue(
            any("Since" in problem for problem in config.check(document(undated, LINUX_ROWS), expected))
        )

    def test_the_repository_reference_is_in_sync(self) -> None:
        self.assertEqual(config.check_repository(), [])


class ErrorCodeReferenceTests(unittest.TestCase):
    ENUM = '''
pub enum Refusal {
    /// The first.
    First,
    /// The second.
    Second { detail: u8 },
}

impl Refusal {
    pub const fn token(self) -> &'static str {
        match self {
            Self::First => "first",
            Self::Second { .. } => {
                "second"
            }
        }
    }
}
'''

    DELEGATE = '''
pub enum Inner {
    /// Inner one.
    One,
    /// Inner two.
    Two,
}

impl Inner {
    pub const fn token(self) -> &'static str {
        match self {
            Self::One => "inner_one",
            Self::Two => "inner_two",
        }
    }
}

pub enum Outer {
    /// Wraps any inner.
    Wrapped(Inner),
    /// Exactly inner two.
    Second,
    /// Plain.
    Plain,
}

impl Outer {
    pub const fn token(self) -> &'static str {
        match self {
            Self::Wrapped(reason) => reason.token(),
            Self::Second => Inner::Two.token(),
            Self::Plain => "plain",
        }
    }
}
'''

    def tokens(self, text: str, source: "codes.Source") -> dict[str, str]:
        root = Path(self.id().replace(".", "_"))
        try:
            (root / "src").mkdir(parents=True, exist_ok=True)
            (root / "src" / "lib.rs").write_text(text, encoding="utf-8")
            return codes._source_tokens(root, source)
        finally:
            for path in sorted(root.rglob("*"), reverse=True):
                path.unlink() if path.is_file() else path.rmdir()
            root.rmdir()

    def test_tokens_are_read_from_the_named_function_with_variant_docs(self) -> None:
        tokens = self.tokens(self.ENUM, codes.Source("## X", "src/lib.rs", "Refusal"))
        self.assertEqual(tokens, {"first": "The first.", "second": "The second."})

    def test_a_variant_without_a_literal_code_fails_unless_its_delegation_is_declared(self) -> None:
        inner = codes.Source("## X", "src/lib.rs", "Inner")
        with self.assertRaisesRegex(codes.ExtractionError, "Wrapped.*declare the delegation"):
            self.tokens(self.DELEGATE, codes.Source("## X", "src/lib.rs", "Outer"))
        with self.assertRaisesRegex(codes.ExtractionError, "Second.*declare the delegation"):
            self.tokens(
                self.DELEGATE,
                codes.Source("## X", "src/lib.rs", "Outer", delegates=(("Wrapped", inner),)),
            )
        tokens = self.tokens(
            self.DELEGATE,
            codes.Source(
                "## X",
                "src/lib.rs",
                "Outer",
                delegates=(("Wrapped", inner), ("Second", inner)),
            ),
        )
        self.assertEqual(set(tokens), {"inner_one", "inner_two", "plain"})

    def test_a_catch_all_arm_or_a_missing_variant_fails(self) -> None:
        catch_all = self.ENUM.replace('Self::Second { .. } => {', "_ => {")
        with self.assertRaisesRegex(codes.ExtractionError, "catch-all"):
            self.tokens(catch_all, codes.Source("## X", "src/lib.rs", "Refusal"))
        extra = self.ENUM.replace("    Second { detail: u8 },\n", "    Second { detail: u8 },\n    /// Third.\n    Third,\n")
        with self.assertRaisesRegex(codes.ExtractionError, "no code for Third"):
            self.tokens(extra, codes.Source("## X", "src/lib.rs", "Refusal"))

    def test_the_real_delegations_are_resolved(self) -> None:
        native = next(source for source in codes.SOURCES if source.enum == "NativeSeatUnavailable")
        launcher = next(source for source in codes.SOURCES if source.enum == "LauncherError")
        polkit = {"polkit_no_rules", "polkit_rule_missing", "polkit_grants_remote_session", "polkit_unverified"}
        self.assertLessEqual(polkit, set(codes._source_tokens(codes.REPO, native)))
        self.assertLessEqual(polkit, set(codes._source_tokens(codes.REPO, launcher)))

    def test_a_new_code_without_a_row_fails(self) -> None:
        expected = {"## Lanes": {"one": "", "two": ""}}
        text = "## Lanes\n\n| Code | Meaning | What to do |\n| --- | --- | --- |\n| `one` | One. | Nothing. |\n"
        problems = codes.check(text, expected)
        self.assertEqual(len(problems), 1)
        self.assertIn("two", problems[0])
        stubbed = codes.stub_rows(text, expected)
        self.assertIn("| `two` | TODO", stubbed)

    def test_events_are_matched_by_id_and_name(self) -> None:
        expected = {codes.EVENTS_HEADING: {"1002 SERVICE_FAILED": ""}}
        text = (
            f"{codes.EVENTS_HEADING}\n\n| Event | Meaning | What to do |\n| --- | --- | --- |\n"
            "| 1002 `SERVICE_FAILED` | Failed. | Look. |\n"
        )
        self.assertEqual(codes.check(text, expected), [])

    def test_the_repository_reference_is_complete(self) -> None:
        self.assertEqual(codes.check_repository(), [])


class SharedContractsWiringTests(unittest.TestCase):
    """check_shared_contracts.py fails when a reference is missing."""

    def run_check(self, root: Path) -> list[str]:
        import check_shared_contracts

        report = check_shared_contracts.Report()
        check_shared_contracts.check_operations_references(root, report)
        return report.failures

    def test_a_tree_without_the_sources_skips_both_checks(self) -> None:
        root = Path(self.id().replace(".", "_"))
        root.mkdir(exist_ok=True)
        try:
            self.assertEqual(self.run_check(root), [])
        finally:
            root.rmdir()

    def test_missing_references_fail_when_the_sources_exist(self) -> None:
        root = Path(self.id().replace(".", "_"))
        try:
            for relative in (config.CONFIG_SOURCE, codes.SOURCES[0].path):
                target = root / relative
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text((codes.REPO / relative).read_text(encoding="utf-8"), encoding="utf-8")
            failures = self.run_check(root)
        finally:
            for path in sorted(root.rglob("*"), reverse=True):
                path.unlink() if path.is_file() else path.rmdir()
            root.rmdir()
        self.assertTrue(any("config-reference.md is missing" in failure for failure in failures), failures)
        self.assertTrue(any("error-codes.md is missing" in failure for failure in failures), failures)

    def test_the_repository_passes(self) -> None:
        self.assertEqual(self.run_check(codes.REPO), [])


if __name__ == "__main__":
    unittest.main()
