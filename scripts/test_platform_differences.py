# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS

"""Tests for the earned-differences ledger check, its scanner and SAI v2."""

import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import platform_differences as ledger  # noqa: E402
import rust_scan  # noqa: E402
import shared_adoption_index as sai  # noqa: E402

REPOSITORY = Path(__file__).resolve().parent.parent

BLOCK = "".join(f"    let value_{i} = compute({i}, \"x\");\n" for i in range(12))

LEDGER = """
schema_version = 1

[[difference]]
id = "PD-001"
area = "files"
products = ["shared/core"]
difference = "No directory fsync on Windows."
earned_by = "Windows cannot open a directory as a file."
code = [{ path = "shared/core/src/lib.rs", symbol = "persist" }]
cfg_sites = [{ path = "shared/core/src/lib.rs", function = "persist", count = 1 }]
config = ["hosts/linux:platform.auth.*"]

[[gap]]
id = "GAP-001"
area = "input"
products = ["hosts/windows"]
difference = "Only Linux sends a keymap."
why_open = "Not implemented yet."
owner = "Windows Host"
code = [{ path = "hosts/linux/src/keymap.rs", symbol = "HostKeymapMsg" }]

[[capability]]
id = "host_keymap"
description = "Sends host_keymap"
proof = 'HostKeymapMsg'
status = { "hosts/linux" = "present", "hosts/windows" = "GAP-001" }

[[local_copy]]
id = "local-modifiers"
pattern = '\\bconst\\s+MOD_SHIFT\\b'
paths = ["hosts"]
shared = "arcen_protocol::keys::MOD_SHIFT"
gap = "GAP-001"
known = ["hosts/linux/src/keymap.rs"]
"""

SOURCES = {
    "shared/core/src/lib.rs": """
pub fn persist() {
    // Windows refuses to open a directory.
    if cfg!(unix) {
        sync();
    }
    #[cfg(feature = "windows")]
    let _ = 1;
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    #[test]
    fn only_on_windows() {}
}
""",
    "hosts/linux/src/keymap.rs": "pub const MOD_SHIFT: u32 = 1;\npub fn send() { HostKeymapMsg::default(); }\n",
    "hosts/windows/src/input.rs": "pub fn send() {}\n",
    "packaging/linux/arcen-pier.json": '{"platform": {"auth": {"mode": "pam"}}}',
}


class Tree:
    def __init__(self) -> None:
        self._directory = tempfile.TemporaryDirectory()
        self.root = Path(self._directory.name)
        self.write(ledger.LEDGER_PATH, LEDGER)
        for name, text in SOURCES.items():
            self.write(name, text)
        self.check(write_report=True, write_clones=True)

    def write(self, name: str, text: str) -> None:
        target = self.root / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(textwrap.dedent(text))
        rust_scan.clear_cache()

    def check(self, write_report: bool = False, write_clones: bool = False):
        report = Report()
        ledger.check(self.root, report, write_report, write_clones)
        return report

    def close(self) -> None:
        self._directory.cleanup()


class Report:
    def __init__(self) -> None:
        self.failures: list[str] = []
        self.notes: list[str] = []


class ScannerTest(unittest.TestCase):
    def test_blanking_keeps_offsets_and_hides_only_comments_and_strings(self):
        source = 'let a = "// not a comment"; // NVENC here\nlet b = \'"\'; /* cuda */ x();\n'
        comments, everything = rust_scan.blank_both(source)
        self.assertEqual(len(comments), len(source))
        self.assertEqual(comments.count("\n"), 2)
        self.assertIn('"// not a comment"', comments)
        self.assertNotIn("NVENC", comments)
        self.assertNotIn("cuda", comments)
        self.assertNotIn("not a comment", everything)
        self.assertIn("x();", everything)

    def test_test_items_are_blanked_and_production_kept(self):
        source = "fn keep() {}\n#[cfg(test)]\nmod tests {\n    fn gone() { unsafe {} }\n}\n#[test]\nfn also_gone() {}\nfn after() {}\n"
        stripped = rust_scan.strip_tests(source)
        self.assertEqual(len(stripped), len(source))
        self.assertIn("fn keep", stripped)
        self.assertIn("fn after", stripped)
        self.assertNotIn("gone", stripped)

    def test_char_and_raw_string_literals_keep_their_length(self):
        for source in ("let q = '\"';", 'let s = r###"a"###;', "let c = b'\\'';", 'let s = br##"it\'s"##;'):
            comments, everything = rust_scan.blank_both(source)
            self.assertEqual((len(comments), len(everything)), (len(source), len(source)), source)

    def test_a_test_only_field_hides_nothing_after_it(self):
        source = "pub struct Handle {\n    #[cfg(test)]\n    calls: HashMap<u32, u8>,\n    #[cfg(windows)]\n    raw: isize,\n}\nfn after() {}\n"
        stripped = rust_scan.strip_tests(source)
        self.assertNotIn("calls", stripped)
        self.assertIn("#[cfg(windows)]\n    raw: isize,\n}", stripped)

    def test_an_array_type_in_a_signature_does_not_end_the_item(self):
        code = "pub fn checksum(data: &[u8; 16]) -> u32 {\n    1\n}\nfn b() {}\n"
        self.assertEqual(rust_scan.leaf_spans(code)[0], (0, code.index("}") + 1))

    def test_only_cross_product_runs_of_the_window_count_as_clones(self):
        tree = Tree()
        self.addCleanup(tree.close)
        tree.write("hosts/linux/src/a.rs", f"fn a() {{\n{BLOCK}}}\nfn twin() {{\n{BLOCK}}}\n")
        tree.write("hosts/windows/src/a.rs", f"fn b() {{\n{BLOCK}}}\n")
        tree.write("hosts/macos/src/a.rs", "fn c() {\n" + "".join(BLOCK.splitlines(True)[:11]) + "}\n")
        report = rust_scan.clone_report(tree.root, ["hosts/linux", "hosts/windows", "hosts/macos"])
        self.assertEqual(report.pair_lines("hosts/linux", "hosts/windows"), 24)
        self.assertEqual(report.covered[("hosts/windows", "hosts/linux")], 12)
        self.assertEqual(report.pair_lines("hosts/linux", "hosts/macos"), 0)


class LedgerTest(unittest.TestCase):
    def setUp(self):
        self.tree = Tree()
        self.addCleanup(self.tree.close)

    def assertFailure(self, needle: str, failures: list[str]) -> None:
        self.assertTrue(any(needle in failure for failure in failures), "\n".join(failures))

    def test_a_consistent_ledger_passes(self):
        self.assertEqual(self.tree.check().failures, [])

    def test_an_unlicensed_os_cfg_in_shared_fails(self):
        self.tree.write(
            "shared/core/src/clock.rs",
            "pub fn now() {\n    #[cfg(target_os = \"macos\")]\n    mach();\n}\n",
        )
        self.assertFailure("shared/core/src/clock.rs (now)", self.tree.check().failures)

    def test_a_licensed_cfg_site_count_must_match(self):
        self.tree.write(
            "shared/core/src/lib.rs",
            "pub fn persist() {\n    if cfg!(unix) { a(); }\n    if cfg!(windows) { b(); }\n}\n",
        )
        self.assertFailure("licenses 1", self.tree.check().failures)

    def test_a_licence_for_a_removed_cfg_site_fails(self):
        self.tree.write("shared/core/src/lib.rs", "pub fn persist() { sync(); }\n")
        self.assertFailure("no longer exist", self.tree.check().failures)

    def test_a_citation_whose_symbol_survives_only_in_a_comment_fails(self):
        self.tree.write("hosts/linux/src/keymap.rs", "pub const MOD_SHIFT: u32 = 1;\n// HostKeymapMsg\n")
        failures = self.tree.check().failures
        self.assertFailure("no longer shows `HostKeymapMsg`", failures)
        self.assertFailure("capability host_keymap: the matrix says hosts/linux has it", failures)

    def test_a_missing_cited_file_fails(self):
        (self.tree.root / "hosts/linux/src/keymap.rs").unlink()
        rust_scan.clear_cache()
        self.assertFailure("hosts/linux/src/keymap.rs does not exist", self.tree.check().failures)

    def test_parity_landing_without_a_matrix_update_fails(self):
        self.tree.write("hosts/windows/src/input.rs", "pub fn send() { HostKeymapMsg::default(); }\n")
        self.assertFailure("set the cell to 'present'", self.tree.check().failures)

    def test_a_new_local_copy_fails_and_a_removed_one_is_a_note(self):
        self.tree.write("hosts/windows/src/input.rs", "const MOD_SHIFT: u32 = 1;\n")
        self.assertFailure("hosts/windows/src/input.rs defines a local copy", self.tree.check().failures)
        self.tree.write("hosts/windows/src/input.rs", "pub fn send() {}\n")
        self.tree.write("hosts/linux/src/keymap.rs", "pub fn send() { HostKeymapMsg::default(); }\n")
        report = self.tree.check()
        self.assertEqual(report.failures, [])
        self.assertTrue(any("shrink `known`" in note for note in report.notes))

    def test_an_unnamed_platform_config_key_fails(self):
        self.tree.write(
            "packaging/linux/arcen-pier.json",
            '{"platform": {"auth": {"mode": "pam"}, "logging": {"rotate_mb": 32}}}',
        )
        self.assertFailure("`platform.logging.rotate_mb` is named by no", self.tree.check().failures)

    def test_a_config_pattern_matching_nothing_fails(self):
        self.tree.write("packaging/linux/arcen-pier.json", '{"platform": {}}')
        self.assertFailure("matches no platform key", self.tree.check().failures)

    def test_clone_growth_fails_and_shrinking_is_a_note(self):
        self.tree.write("hosts/linux/src/a.rs", f"fn a() {{\n{BLOCK}}}\n")
        self.tree.write("hosts/windows/src/a.rs", f"fn b() {{\n{BLOCK}}}\n")
        self.assertFailure("hosts/linux | hosts/windows share 12", self.tree.check().failures)
        self.tree.check(write_clones=True)
        self.tree.write("hosts/windows/src/a.rs", "fn b() {}\n")
        report = self.tree.check()
        self.assertEqual([f for f in report.failures if "clones" in f], [])
        self.assertTrue(any("down from 12 to 0" in note for note in report.notes))

    def test_a_stale_document_fails(self):
        self.tree.write(ledger.LEDGER_PATH, LEDGER.replace("Only Linux sends", "Linux alone sends"))
        self.assertFailure("platform-differences.md is stale", self.tree.check().failures)

    def test_a_capability_cell_for_a_missing_product_fails(self):
        self.tree.write(ledger.LEDGER_PATH, LEDGER.replace('"hosts/windows" = "GAP-001" }', '"hosts/windows" = "GAP-001", "hosts/windws" = "GAP-001" }'))
        self.assertFailure("names 'hosts/windws', which does not exist", self.tree.check().failures)

    def test_a_status_naming_no_row_fails(self):
        self.tree.write(ledger.LEDGER_PATH, LEDGER.replace('"hosts/windows" = "GAP-001"', '"hosts/windows" = "GAP-999"'))
        self.assertFailure("is neither 'present' nor a ledger id", self.tree.check().failures)


class RepositoryLedgerTest(unittest.TestCase):
    def test_the_committed_ledger_holds(self):
        report = Report()
        ledger.check(REPOSITORY, report)
        self.assertEqual(report.failures, [])


class AdoptionIndexV2Test(unittest.TestCase):
    def setUp(self):
        self._directory = tempfile.TemporaryDirectory()
        self.addCleanup(self._directory.cleanup)
        self.root = Path(self._directory.name)
        rust_scan.clear_cache()

    def source(self, name: str, text: str) -> rust_scan.SourceFile:
        target = self.root / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(textwrap.dedent(text))
        rust_scan.clear_cache()
        return next(s for s in rust_scan.production_files(self.root, "hosts/x") if s.path == name)

    def test_one_os_item_does_not_make_the_whole_file_os_bound(self):
        source = self.source(
            "hosts/x/src/a.rs",
            """
            //! Talks to NVENC and cuda in the docs only.
            pub fn pure(a: u32) -> u32 {
                a + 1
            }

            pub fn native() {
                unsafe { call() }
            }
            """,
        )
        self.assertEqual(sai.portable_lines(source), 3)
        self.assertTrue(sai.OS_MARKER.search((self.root / "hosts/x/src/a.rs").read_text()))

    def test_identifiers_imported_from_an_os_crate_mark_their_items(self):
        source = self.source(
            "hosts/x/src/b.rs",
            """
            use windows::Win32::Security::LogonUserW;

            pub fn check() -> bool {
                LogonUserW()
            }

            pub fn policy() -> bool {
                true
            }
            """,
        )
        self.assertEqual(sai.portable_lines(source), 3)

    def test_tests_and_lab_examples_are_not_product_code(self):
        self.source("hosts/x/src/c.rs", "pub fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() { unsafe {} }\n}\n")
        (self.root / "hosts/x/examples").mkdir(parents=True)
        (self.root / "hosts/x/examples/lab.rs").write_text("fn main() {}\n")
        rust_scan.clear_cache()
        files = rust_scan.production_files(self.root, "hosts/x")
        self.assertEqual([f.path for f in files], ["hosts/x/src/c.rs"])
        self.assertNotIn("unsafe", files[0].code)

    def test_reach_is_weighted_by_lines(self):
        self.source("hosts/x/src/big.rs", "use arcen_session::x;\n" + "fn f() {}\n" * 9)
        for index in range(3):
            self.source(f"hosts/x/src/small{index}.rs", "fn g() {}\n")
        (self.root / "hosts/x/Cargo.toml").write_text("[package]\nname = 'x'\n")
        row = sai.measure_v2(self.root, "hosts/x")
        self.assertAlmostEqual(row["reach"], 100 * 10 / 13)


if __name__ == "__main__":
    unittest.main()
