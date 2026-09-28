"""Tests for scripts/check_shared_contracts.py."""

import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import check_shared_contracts as checker  # noqa: E402

CONTRACTS = """
[[consumer]]
id = "lifecycle"
gate = 4
symbol = "SessionLifecycles"
products = ["hosts/a", "hosts/b"]
reason = "hosts drive the shared lifecycle"

[[forbid]]
id = "ignored-step"
gate = 4
pattern = 'let\\s+_\\s*=\\s*transaction\\s*\\.\\s*apply'
paths = ["hosts"]
reason = "check the result"

[[forbid]]
id = "colourless-edid"
gate = 2
pattern = 'EdidRequest\s*\{[^}]*?color:\s*None'
paths = ["hosts"]
justified_by_comment = true
reason = "forward the Deck display's colour"

[ratchet]
baseline = "baseline.txt"
paths = ["hosts"]
minimum_lines = 3
"""

PORTABLE = "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\n"


class SharedContractsTest(unittest.TestCase):
    def tree(self, files: dict[str, str], baseline: str = "") -> Path:
        root = Path(tempfile.mkdtemp())
        (root / "scripts/ci").mkdir(parents=True)
        (root / "scripts/ci/shared-contracts.toml").write_text(CONTRACTS)
        (root / "baseline.txt").write_text(baseline)
        for name, text in files.items():
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(textwrap.dedent(text))
        return root

    def test_a_host_without_a_production_consumer_fails(self):
        root = self.tree(
            {
                "hosts/a/src/lib.rs": "unsafe fn x() { SessionLifecycles::new(); }",
                "hosts/b/src/lib.rs": """
                    unsafe fn y() {}
                    #[cfg(test)]
                    mod tests { fn t() { SessionLifecycles::new(); } }
                """,
            }
        )
        report = checker.run(root)
        self.assertEqual(len(report.failures), 1, report.failures)
        self.assertIn("hosts/b has no production use of SessionLifecycles", report.failures[0])

    def test_a_forbidden_pattern_in_production_fails_but_not_in_tests(self):
        root = self.tree(
            {
                "hosts/a/src/lib.rs": "unsafe fn x() { SessionLifecycles::new(); let _ = transaction.apply(e); }",
                "hosts/b/src/lib.rs": """
                    unsafe fn y() { SessionLifecycles::new(); }
                    #[cfg(test)]
                    mod tests { fn t() { let _ = transaction.apply(e); } }
                """,
            }
        )
        failures = checker.run(root).failures
        self.assertEqual(len(failures), 1, failures)
        self.assertIn("hosts/a/src/lib.rs:1", failures[0])

    def test_the_ratchet_refuses_new_portable_host_code_and_allows_shrinking(self):
        files = {
            "hosts/a/src/lib.rs": "unsafe fn x() { SessionLifecycles::new(); }",
            "hosts/b/src/lib.rs": "unsafe fn y() { SessionLifecycles::new(); }",
            "hosts/a/src/policy.rs": PORTABLE,
        }
        failures = checker.run(self.tree(files)).failures
        self.assertEqual(len(failures), 1, failures)
        self.assertIn("hosts/a/src/policy.rs", failures[0])

        listed = self.tree(files, baseline="hosts/a/src/policy.rs 4\nhosts/gone.rs 9\n")
        report = checker.run(listed)
        self.assertEqual(report.failures, [])
        self.assertTrue(any("shrink the baseline" in note for note in report.notes))

    def test_default_configs_must_match_outside_paths_and_platform(self):
        contracts = CONTRACTS + textwrap.dedent(
            """
            [default_configs]
            files = ["a.json", "b.json"]
            ignore = ["tls.cert"]
            """
        )
        same = '{"audio":{"enabled":true},"tls":{"cert":"%s"},"platform":{"x":%d}}'
        root = self.tree({"a.json": same % ("/a", 1), "b.json": same % ("C:/b", 2)})
        (root / "scripts/ci/shared-contracts.toml").write_text(contracts)
        report = checker.run(root)
        self.assertFalse(
            [f for f in report.failures if "default config" in f], report.failures
        )
        (root / "b.json").write_text('{"audio":{"enabled":false},"tls":{},"platform":{}}')
        report = checker.run(root)
        self.assertTrue(
            any("default config b.json differs" in f and "audio" in f for f in report.failures),
            report.failures,
        )

    def test_os_code_is_not_portable(self):
        root = self.tree(
            {
                "hosts/a/src/lib.rs": "unsafe fn x() { SessionLifecycles::new(); }",
                "hosts/b/src/lib.rs": "unsafe fn y() { SessionLifecycles::new(); }",
                "hosts/a/src/native.rs": "fn a() { std::process::Command::new(\"x\"); }\n" * 4,
            }
        )
        self.assertEqual(checker.run(root).failures, [])

    def test_a_justified_exception_passes_and_an_unexplained_one_fails(self):
        root = self.tree(
            {
                "hosts/a/src/lib.rs": """
                    unsafe fn x() { SessionLifecycles::new(); }
                    unsafe fn probe() -> EdidRequest {
                        EdidRequest {
                            width: 1,
                            // shared-contract colourless-edid: operator probing has
                            // no Deck display to lend.
                            color: None,
                        }
                    }
                """,
                "hosts/b/src/lib.rs": """
                    unsafe fn y() { SessionLifecycles::new(); }
                    unsafe fn plan() -> EdidRequest {
                        EdidRequest {
                            width: 1,
                            // tmp
                            color: None,
                        }
                    }
                """,
            }
        )
        failures = checker.run(root).failures
        self.assertEqual(len(failures), 1, failures)
        self.assertIn("hosts/b/src/lib.rs", failures[0])

    def test_comments_imports_strings_and_test_fns_are_not_consumers(self):
        root = self.tree(
            {
                "hosts/a/src/lib.rs": "unsafe fn x() { SessionLifecycles::new(); }",
                "hosts/b/src/lib.rs": """
                    use arcen_session::SessionLifecycles;
                    // SessionLifecycles used to live here.
                    /* SessionLifecycles */
                    unsafe fn y() { log("SessionLifecycles"); }
                    #[test]
                    fn t() { SessionLifecycles::new(); }
                """,
            }
        )
        failures = checker.run(root).failures
        self.assertEqual(len(failures), 1, failures)
        self.assertIn("hosts/b has no production use of SessionLifecycles", failures[0])

    def test_test_modules_are_stripped_with_nested_braces(self):
        source = "fn keep() {}\n#[cfg(test)]\nmod tests {\n fn t() { if x { y } }\n}\nfn after() {}\n"
        stripped = checker.strip_test_code(source)
        self.assertIn("fn keep", stripped)
        self.assertIn("fn after", stripped)
        self.assertNotIn("fn t()", stripped)


if __name__ == "__main__":
    unittest.main()
