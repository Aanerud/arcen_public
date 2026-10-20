"""Tests for scripts/check_shared_contracts.py."""

import copy
import json
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import check_shared_contracts as checker  # noqa: E402

CONTRACTS = """
[[forbid]]
id = "ignored-step"
gate = 4
pattern = 'let\\s+_\\s*=\\s*transaction\\s*\\.\\s*apply'
paths = ["hosts"]
reason = "check the result"

[[forbid]]
id = "colourless-edid"
gate = 2
pattern = 'EdidRequest\\s*\\{[^}]*?color:\\s*None'
paths = ["hosts"]
justified_by_comment = true
reason = "forward the Deck display's colour"

[ratchet]
baseline = "baseline.txt"
paths = ["hosts"]
minimum_lines = 3
"""

PORTABLE = "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\n"

CATALOGUE = """
schema_version = 1
registry_validation = "registry"

[template.cargo-test]
argv = ["cargo", "test", "--locked", "{target}", "-p", "{package}", "{features}"]

[template.python-script]
argv = ["python3", "{script}"]

[template.recorded]
argv = []

[build_profile.shared-x]
description = "x"
target_os = "any"
package = "arcen-x"

[build_profile.shared-y]
description = "y"
target_os = "any"
package = "arcen-y"

[build_profile.linux-a]
description = "a on Linux"
target_os = "linux"
package = "arcen-a"

[build_profile.windows-b]
description = "b on Windows"
target_os = "windows"
rust_target = "x86_64-pc-windows-msvc"
package = "arcen-b"

[build_profile.linux-inst]
description = "installer embedding a"
target_os = "linux"
package = "arcen-inst"

[validation.registry]
description = "register"
kind = "schema"
target_os = "any"
template = "python-script"
script = "scripts/check_shared_contracts.py"
owner = "Shared/Architecture"

[validation.x-test]
description = "x tests"
kind = "unit"
build_profile = "shared-x"
template = "cargo-test"
owner = "Shared/Architecture"

[validation.y-test]
description = "y tests"
kind = "unit"
build_profile = "shared-y"
template = "cargo-test"
owner = "Shared/Architecture"

[validation.a-test]
description = "a tests"
kind = "unit"
build_profile = "linux-a"
template = "cargo-test"
owner = "Linux Host"

[validation.a-lab]
description = "a on hardware"
kind = "hardware"
build_profile = "linux-a"
template = "recorded"
owner = "Linux Host"

[validation.b-test]
description = "b tests"
kind = "unit"
build_profile = "windows-b"
template = "cargo-test"
owner = "Windows Host"

[validation.b-lab]
description = "b on hardware"
kind = "hardware"
build_profile = "windows-b"
template = "recorded"
owner = "Windows Host"

[validation.inst-build]
description = "installer build"
kind = "build"
build_profile = "linux-inst"
template = "recorded"
owner = "Linux Host"
"""

MANIFESTS = {
    "Cargo.toml": """
        [workspace]
        members = ["shared/x", "shared/y", "hosts/a", "hosts/b", "packaging/inst"]
        [workspace.dependencies]
        arcen-x = { path = "shared/x" }
    """,
    "shared/x/Cargo.toml": """
        [package]
        name = "arcen-x"
        [features]
        simd = []
    """,
    "shared/y/Cargo.toml": """
        [package]
        name = "arcen-y"
        [dependencies]
        arcen-x.workspace = true
    """,
    "hosts/a/Cargo.toml": """
        [package]
        name = "arcen-a"
        [features]
        fast = ["arcen-x/simd"]
        [dependencies]
        arcen-x.workspace = true
        arcen-y = { path = "../../shared/y" }
    """,
    "hosts/b/Cargo.toml": """
        [package]
        name = "arcen-b"
        [target.'cfg(windows)'.dependencies]
        arcen-x = { path = "../../shared/x", features = ["simd"] }
        [dev-dependencies]
        arcen-y = { path = "../../shared/y" }
    """,
    "packaging/inst/Cargo.toml": """
        [package]
        name = "arcen-inst"
    """,
}

SOURCES = {
    "shared/x/src/lib.rs": "pub mod policy;\npub struct SessionLifecycles;\n",
    "shared/x/src/policy.rs": "pub fn rule() {}\n",
    "shared/x/fixtures/v.json": "{}\n",
    "shared/x/README.md": "# x\n",
    "shared/y/src/lib.rs": "pub fn y() { arcen_x::policy::rule(); }\n",
    "hosts/a/src/lib.rs": "unsafe fn x() { arcen_x::SessionLifecycles::new(); arcen_y::y(); }\n",
    "hosts/a/src/region.rs": (
        'unsafe fn v() -> &str { include_str!("../../../shared/x/fixtures/v.json") }\n'
    ),
    "hosts/b/src/lib.rs": "unsafe fn y() { arcen_x::SessionLifecycles::new(); }\n",
    "hosts/b/tests/y.rs": "#[test]\nfn t() { arcen_y::y(); }\n",
    "packaging/inst/src/main.rs": 'const PIER: &[u8] = include_bytes!(env!("A_BIN"));\nfn main() {}\n',
    "scripts/check_shared_contracts.py": "print('stand-in register check output')\n",
    "baseline.txt": "",
}


def path(path_id, entry_points, profiles, validation, owner):
    return {
        "id": path_id,
        "entry_points": entry_points,
        "build_profiles": profiles,
        "validation": validation,
        "owner": owner,
    }


A_AUTO = path(
    "linux-xorg-auto", ["hosts/a/src/lib.rs"], ["linux-a"], ["a-test", "a-lab"], "Linux Host"
)
B_AUTO = path(
    "windows-auto", ["hosts/b/src/lib.rs"], ["windows-b"], ["b-test", "b-lab"], "Windows Host"
)
INSTALLER = path(
    "linux-installer", ["packaging/inst/src/main.rs"], ["linux-inst"], ["inst-build"], "Linux Host"
)
USE = {"id": "lifecycle", "gate": 4, "symbol": "SessionLifecycles", "reason": "hosts drive it"}

REGISTERS = {
    "shared/x/consumers.json": {
        "schema_version": 1,
        "crate": "arcen-x",
        "owner": "Shared/Architecture",
        "validation": ["x-test"],
        "consumers": [
            {
                "package": "arcen-a",
                "dependency_kind": "normal",
                "contracts": ["crate"],
                "forwarded_features": {"fast": ["simd"]},
                "production_uses": [USE],
                "paths": [A_AUTO],
            },
            {
                "package": "arcen-b",
                "dependency_kind": "normal",
                "contracts": ["crate"],
                "targets": ["cfg(windows)"],
                "features": ["simd"],
                "production_uses": [USE],
                "paths": [B_AUTO],
            },
            {"package": "arcen-y", "dependency_kind": "normal", "contracts": ["policy"]},
            {
                "package": "arcen-a",
                "dependency_kind": "source",
                "contracts": ["crate"],
                "includes": ["shared/x/fixtures/v.json"],
                "paths": [
                    path(
                        "linux-xorg-auto",
                        ["hosts/a/src/region.rs"],
                        ["linux-a"],
                        ["a-test"],
                        "Linux Host",
                    )
                ],
            },
            {
                "package": "arcen-inst",
                "dependency_kind": "embedded",
                "contracts": ["crate"],
                "via": ["arcen-a"],
                "paths": [INSTALLER],
            },
        ],
    },
    "shared/y/consumers.json": {
        "schema_version": 1,
        "crate": "arcen-y",
        "owner": "Shared/Architecture",
        "validation": ["y-test"],
        "consumers": [
            {
                "package": "arcen-a",
                "dependency_kind": "normal",
                "contracts": ["crate"],
                "paths": [A_AUTO],
            },
            {
                "package": "arcen-b",
                "dependency_kind": "dev",
                "contracts": ["crate"],
                "paths": [
                    path(
                        "windows-auto",
                        ["hosts/b/tests/y.rs"],
                        ["windows-b"],
                        ["b-test"],
                        "Windows Host",
                    )
                ],
            },
            {
                "package": "arcen-inst",
                "dependency_kind": "embedded",
                "contracts": ["crate"],
                "via": ["arcen-a"],
                "paths": [INSTALLER],
            },
        ],
    },
}


def write(root: Path, name: str, text: str) -> None:
    target = root / name
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text)


class Fixture:
    """A small workspace: shared x <- shared y; hosts a and b; an installer."""

    def __init__(self) -> None:
        self.root = Path(tempfile.mkdtemp())
        write(self.root, "scripts/ci/shared-contracts.toml", CONTRACTS)
        write(self.root, checker.CATALOGUE_PATH, CATALOGUE)
        for name, text in MANIFESTS.items():
            write(self.root, name, textwrap.dedent(text))
        for name, text in SOURCES.items():
            write(self.root, name, text)
        self.registers = copy.deepcopy(REGISTERS)
        self.save()
        checker.run(self.root, workspace=self.workspace(), write_report=True)

    def save(self) -> None:
        for name, doc in self.registers.items():
            write(self.root, name, json.dumps(doc, indent=2))

    def workspace(self) -> checker.Workspace:
        return checker.workspace_from_manifests(checker.fs_reader(self.root))

    def failures(self, write_report: bool = True) -> list[str]:
        self.save()
        return checker.run(
            self.root, workspace=self.workspace(), write_report=write_report
        ).failures

    def record(self, register: str, package: str, kind: str) -> dict:
        return next(
            r
            for r in self.registers[register]["consumers"]
            if r["package"] == package and r["dependency_kind"] == kind
        )

    def drop(self, register: str, package: str, kind: str) -> None:
        self.registers[register]["consumers"].remove(self.record(register, package, kind))

    def git(self, *args: str) -> str:
        return subprocess.run(
            ["git", "-C", str(self.root), *args], capture_output=True, text=True, check=True
        ).stdout.strip()

    def commit(self, message: str) -> str:
        self.save()
        if not (self.root / ".git").exists():
            self.git("init", "-q", "-b", "main")
            self.git("config", "user.email", "ci@example.internal")
            self.git("config", "user.name", "ci")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", message)
        return self.git("rev-parse", "HEAD")


EMPTY = checker.Workspace({}, {}, [])


class ProductionRulesTest(unittest.TestCase):
    """The forbid, ratchet and default-config rules in shared-contracts.toml."""

    def tree(self, files: dict[str, str], baseline: str = "") -> Path:
        root = Path(tempfile.mkdtemp())
        write(root, "scripts/ci/shared-contracts.toml", CONTRACTS)
        write(root, "baseline.txt", baseline)
        for name, text in files.items():
            write(root, name, textwrap.dedent(text))
        return root

    def test_a_forbidden_pattern_in_production_fails_but_not_in_tests(self):
        root = self.tree(
            {
                "hosts/a/src/lib.rs": "unsafe fn x() { let _ = transaction.apply(e); }",
                "hosts/b/src/lib.rs": """
                    unsafe fn y() {}
                    #[cfg(test)]
                    mod tests { fn t() { let _ = transaction.apply(e); } }
                """,
            }
        )
        failures = checker.run(root, workspace=EMPTY).failures
        self.assertEqual(len(failures), 1, failures)
        self.assertIn("hosts/a/src/lib.rs:1", failures[0])

    def test_the_ratchet_refuses_new_portable_host_code_and_allows_shrinking(self):
        files = {
            "hosts/a/src/lib.rs": "unsafe fn x() {}",
            "hosts/a/src/policy.rs": PORTABLE,
        }
        failures = checker.run(self.tree(files), workspace=EMPTY).failures
        self.assertEqual(len(failures), 1, failures)
        self.assertIn("hosts/a/src/policy.rs", failures[0])

        listed = self.tree(files, baseline="hosts/a/src/policy.rs 4\nhosts/gone.rs 9\n")
        report = checker.run(listed, workspace=EMPTY)
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
        report = checker.run(root, workspace=EMPTY)
        self.assertFalse([f for f in report.failures if "default config" in f], report.failures)
        (root / "b.json").write_text('{"audio":{"enabled":false},"tls":{},"platform":{}}')
        report = checker.run(root, workspace=EMPTY)
        self.assertTrue(
            any("default config b.json differs" in f and "audio" in f for f in report.failures),
            report.failures,
        )

    def test_os_code_is_not_portable(self):
        root = self.tree(
            {"hosts/a/src/native.rs": 'fn a() { std::process::Command::new("x"); }\n' * 4}
        )
        self.assertEqual(checker.run(root, workspace=EMPTY).failures, [])

    def test_a_justified_exception_passes_and_an_unexplained_one_fails(self):
        root = self.tree(
            {
                "hosts/a/src/lib.rs": """
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
        failures = checker.run(root, workspace=EMPTY).failures
        self.assertEqual(len(failures), 1, failures)
        self.assertIn("hosts/b/src/lib.rs", failures[0])

    def test_consumer_rules_in_the_toml_are_rejected(self):
        root = self.tree({})
        (root / "scripts/ci/shared-contracts.toml").write_text(
            CONTRACTS
            + '[[consumer]]\nid = "x"\nsymbol = "X"\nproducts = ["hosts/a"]\nreason = "r"\n'
        )
        failures = checker.run(root, workspace=EMPTY).failures
        self.assertTrue(any("moved to the production_uses" in f for f in failures), failures)

    def test_test_modules_are_stripped_with_nested_braces(self):
        source = (
            "fn keep() {}\n#[cfg(test)]\nmod tests {\n fn t() { if x { y } }\n}\nfn after() {}\n"
        )
        stripped = checker.strip_test_code(source)
        self.assertIn("fn keep", stripped)
        self.assertIn("fn after", stripped)
        self.assertNotIn("fn t()", stripped)


class RegisterTest(unittest.TestCase):
    def setUp(self):
        self.fx = Fixture()

    def assertFailure(self, needle: str, failures: list[str]) -> None:
        self.assertTrue(any(needle in f for f in failures), "\n".join(failures))

    def test_a_complete_register_passes(self):
        self.assertEqual(self.fx.failures(write_report=False), [])

    def test_a_host_without_a_production_use_fails(self):
        write(
            self.fx.root,
            "hosts/b/src/lib.rs",
            textwrap.dedent(
                """
                use arcen_x::SessionLifecycles;
                // SessionLifecycles used to live here.
                /* SessionLifecycles */
                unsafe fn y() { arcen_x::policy::rule(); log("SessionLifecycles"); }
                #[cfg(test)]
                mod tests { fn t() { SessionLifecycles::new(); } }
                """
            ),
        )
        failures = self.fx.failures()
        self.assertEqual(len(failures), 1, failures)
        self.assertIn("hosts/b has no production use of SessionLifecycles", failures[0])

    def test_a_missing_consumer_fails(self):
        self.fx.drop("shared/x/consumers.json", "arcen-b", "normal")
        self.assertFailure("missing consumer arcen-b (normal)", self.fx.failures())

    def test_a_missing_dev_consumer_fails(self):
        self.fx.drop("shared/y/consumers.json", "arcen-b", "dev")
        self.assertFailure("missing consumer arcen-b (dev)", self.fx.failures())

    def test_a_stale_consumer_fails(self):
        self.fx.registers["shared/y/consumers.json"]["consumers"].append(
            {
                "package": "arcen-b",
                "dependency_kind": "build",
                "contracts": ["crate"],
                "paths": [B_AUTO],
            }
        )
        self.assertFailure("consumer arcen-b (build) is stale", self.fx.failures())

    def test_target_and_feature_specific_declarations_must_match_cargo(self):
        record = self.fx.record("shared/x/consumers.json", "arcen-b", "normal")
        del record["targets"]
        record["features"] = []
        self.fx.record("shared/x/consumers.json", "arcen-a", "normal")["forwarded_features"] = {}
        failures = self.fx.failures()
        self.assertFailure(
            "targets is ['*'] in the register but ['cfg(windows)'] in Cargo", failures
        )
        self.assertFailure("features is [] in the register but ['simd'] in Cargo", failures)
        self.assertFailure("forwarded_features is {} in the register", failures)

    def test_source_inclusion_must_be_registered(self):
        self.fx.drop("shared/x/consumers.json", "arcen-a", "source")
        self.assertFailure("missing consumer arcen-a (source)", self.fx.failures())

    def test_a_stale_source_inclusion_fails(self):
        self.fx.record("shared/x/consumers.json", "arcen-a", "source")["includes"].append(
            "shared/x/src/policy.rs"
        )
        self.assertFailure("shared/x/src/policy.rs is no longer included", self.fx.failures())

    def test_embedding_must_be_registered_wherever_the_embedded_binary_reaches(self):
        self.fx.drop("shared/x/consumers.json", "arcen-inst", "embedded")
        self.assertFailure("missing consumer arcen-inst (embedded via arcen-a)", self.fx.failures())
        self.fx.drop("shared/y/consumers.json", "arcen-inst", "embedded")
        self.assertFailure("arcen-inst embeds a binary", self.fx.failures())

    def test_an_embedding_record_for_a_binary_that_does_not_use_the_crate_is_stale(self):
        self.fx.record("shared/y/consumers.json", "arcen-inst", "embedded")["via"].append("arcen-b")
        self.assertFailure("embedded arcen-b does not depend on arcen-y", self.fx.failures())

    def test_a_product_reaching_shared_code_only_through_a_helper_needs_a_path(self):
        root = self.fx.root
        cargo = (
            (root / "Cargo.toml")
            .read_text()
            .replace('"packaging/inst"]', '"packaging/inst", "hosts/c"]')
        )
        write(root, "Cargo.toml", cargo)
        write(
            root,
            "hosts/c/Cargo.toml",
            '[package]\nname = "arcen-c"\n[dependencies]\narcen-a = { path = "../a" }\n',
        )
        write(root, "hosts/c/src/lib.rs", "unsafe fn c() { arcen_a::x(); }\n")
        self.assertFailure(
            "arcen-c reaches shared crates through arcen-a but no consumers.json names a path",
            self.fx.failures(),
        )
        record = {
            "package": "arcen-c",
            "dependency_kind": "indirect",
            "contracts": ["crate"],
            "via": ["arcen-b"],
            "paths": [
                path("linux-c", ["hosts/c/src/lib.rs"], ["linux-a"], ["a-test"], "Linux Host")
            ],
        }
        self.fx.registers["shared/x/consumers.json"]["consumers"].append(record)
        self.assertFailure(
            "arcen-c does not reach arcen-x through the non-shared package arcen-b",
            self.fx.failures(),
        )
        record["via"] = ["arcen-a"]
        self.assertEqual(self.fx.failures(), [])
        base = self.fx.commit("c registered")
        write(root, "shared/x/src/policy.rs", "pub fn rule() { /* changed */ }\n")
        head = self.fx.commit("x changed")
        impact = checker.impact_between(root, base, head)[0]
        self.assertIn("arcen-c", impact.affected_products)
        self.assertIn(("arcen-x", "arcen-c", "linux-c"), impact.requirements["a-test"].reasons)

    def test_duplicate_and_conflicting_path_ids_fail(self):
        record = self.fx.record("shared/x/consumers.json", "arcen-a", "normal")
        record["paths"].append(copy.deepcopy(A_AUTO))
        self.assertFailure("duplicate path ID in one consumer record", self.fx.failures())
        record["paths"].pop()
        self.fx.record("shared/y/consumers.json", "arcen-a", "normal")["paths"][0]["owner"] = (
            "Windows Host"
        )
        self.assertFailure("path linux-xorg-auto has conflicting owners", self.fx.failures())

    def test_unknown_validation_and_build_profile_ids_fail(self):
        record = self.fx.record("shared/x/consumers.json", "arcen-a", "normal")
        record["paths"][0] = dict(record["paths"][0], validation=["a-test", "a-lab-typo"])
        record["paths"][0]["build_profiles"] = ["linux-a", "linux-zz"]
        failures = self.fx.failures()
        self.assertFailure("unknown validation ID 'a-lab-typo'", failures)
        self.assertFailure("unknown build profile 'linux-zz'", failures)
        self.fx.registers["shared/x/consumers.json"]["validation"] = ["x-tests"]
        self.assertFailure("unknown validation ID 'x-tests'", self.fx.failures())

    def test_entry_points_exist_and_show_the_dependency(self):
        record = self.fx.record("shared/x/consumers.json", "arcen-a", "normal")
        record["paths"][0] = dict(record["paths"][0], entry_points=["hosts/a/src/gone.rs"])
        self.assertFailure("entry point hosts/a/src/gone.rs does not exist", self.fx.failures())
        write(self.fx.root, "hosts/a/src/other.rs", "unsafe fn o() {}\n")
        record["paths"][0]["entry_points"] = ["hosts/a/src/other.rs"]
        self.assertFailure("no Rust entry point refers to arcen_x", self.fx.failures())

    def test_every_shared_crate_needs_a_valid_register(self):
        (self.fx.root / "shared/y/consumers.json").unlink()
        del self.fx.registers["shared/y/consumers.json"]
        self.assertFailure("shared crate arcen-y has no consumer register", self.fx.failures())
        self.fx.registers["shared/y/consumers.json"] = copy.deepcopy(
            REGISTERS["shared/y/consumers.json"]
        )
        self.fx.registers["shared/y/consumers.json"]["schema_version"] = 2
        self.fx.registers["shared/y/consumers.json"]["consumers"][0]["contracts"] = ["nope"]
        failures = self.fx.failures()
        self.assertFailure("schema_version must be 1", failures)
        self.assertFailure("contract 'nope' is not a arcen-y module", failures)
        self.fx.save()
        (self.fx.root / "shared/y/consumers.json").write_text('{"crate": 1, "crate": 2}')
        report = checker.run(self.fx.root, workspace=self.fx.workspace())
        self.assertFailure("duplicate key 'crate'", report.failures)

    def test_the_generated_report_must_be_current(self):
        self.fx.record("shared/x/consumers.json", "arcen-a", "normal")["contracts"] = [
            "crate",
            "policy",
        ]
        self.assertFailure("shared-consumers.md is stale", self.fx.failures(write_report=False))
        self.assertEqual(self.fx.failures(write_report=True), [])
        text = (self.fx.root / checker.REPORT_PATH).read_text()
        self.assertIn("## `arcen-x`", text)
        self.assertIn("`linux-xorg-auto` (Linux Host)", text)
        self.assertIn("including registered embedding and source edges): `arcen-inst`", text)

    def test_the_catalogue_allows_only_reviewed_commands(self):
        catalogue = CATALOGUE.replace(
            'argv = ["python3", "{script}"]', 'argv = ["sh", "-c", "{shell}"]'
        ) + textwrap.dedent(
            """
            [validation.sneaky]
            description = "x"
            kind = "hardware"
            target_os = "linux"
            template = "cargo-test"
            package = "arcen-a"
            owner = "Linux Host"
            """
        )
        write(self.fx.root, checker.CATALOGUE_PATH, catalogue)
        failures = self.fx.failures()
        self.assertFailure("unsupported token '{shell}'", failures)
        self.assertFailure("validation sneaky is hardware; its evidence is recorded", failures)

    def test_commands_come_only_from_catalogue_templates(self):
        catalogue, errors = checker.load_catalogue(checker.fs_reader(self.fx.root))
        self.assertEqual(errors, [])
        self.assertEqual(
            catalogue.command("b-test"),
            ["cargo", "test", "--locked", "--target", "x86_64-pc-windows-msvc", "-p", "arcen-b"],
        )
        self.assertEqual(catalogue.command("a-lab"), [])

    def test_the_manifest_reader_agrees_with_cargo_metadata(self):
        root = "/w"
        metadata = {
            "workspace_root": root,
            "workspace_members": ["x", "a"],
            "packages": [
                {
                    "id": "x",
                    "name": "arcen-x",
                    "manifest_path": f"{root}/shared/x/Cargo.toml",
                    "features": {},
                    "dependencies": [],
                },
                {
                    "id": "a",
                    "name": "arcen-a",
                    "manifest_path": f"{root}/hosts/a/Cargo.toml",
                    "features": {},
                    "dependencies": [
                        {
                            "name": "arcen-x",
                            "kind": None,
                            "target": None,
                            "optional": False,
                            "features": [],
                            "path": f"{root}/shared/x",
                            "rename": "renamed",
                        },
                        {
                            "name": "serde",
                            "kind": None,
                            "target": None,
                            "optional": False,
                            "features": [],
                        },
                    ],
                },
            ],
        }
        files = {
            "Cargo.toml": '[workspace]\nmembers = ["shared/x", "hosts/a"]\n',
            "shared/x/Cargo.toml": '[package]\nname = "arcen-x"\n',
            "hosts/a/Cargo.toml": (
                '[package]\nname = "arcen-a"\n[dependencies]\n'
                'renamed = { package = "arcen-x", path = "../../shared/x" }\nserde = "1"\n'
            ),
        }
        from_metadata = checker.workspace_from_metadata(metadata)
        from_manifests = checker.workspace_from_manifests(files.get)
        self.assertEqual(checker.compare_workspaces(from_metadata, from_manifests), [])
        self.assertEqual(
            from_manifests.dependencies, [checker.Dependency("arcen-a", "arcen-x", "normal")]
        )
        metadata["packages"][1]["dependencies"][0]["kind"] = "dev"
        self.assertTrue(
            checker.compare_workspaces(checker.workspace_from_metadata(metadata), from_manifests)
        )


class ImpactTest(unittest.TestCase):
    def setUp(self):
        self.fx = Fixture()
        self.base = self.fx.commit("base")

    def impact(self) -> checker.Impact:
        head = self.fx.commit("head")
        return checker.impact_between(self.fx.root, self.base, head)[0]

    def test_changing_a_component_used_by_two_paths_requires_both(self):
        write(self.fx.root, "shared/x/src/policy.rs", "pub fn rule() { /* changed */ }\n")
        impact = self.impact()
        self.assertEqual(sorted(impact.changed), ["arcen-x"])
        required = set(impact.requirements)
        self.assertTrue({"a-test", "a-lab", "b-test", "b-lab"} <= required, required)
        self.assertIn(
            ("arcen-x", "arcen-a", "linux-xorg-auto"), impact.requirements["a-lab"].reasons
        )
        self.assertIn(("arcen-x", "arcen-b", "windows-auto"), impact.requirements["b-lab"].reasons)

    def test_indirect_consumers_and_embedders_are_included(self):
        write(self.fx.root, "shared/x/src/policy.rs", "pub fn rule() { /* changed */ }\n")
        impact = self.impact()
        self.assertEqual(impact.affected_shared, {"arcen-x", "arcen-y"})
        self.assertEqual(impact.affected_products, {"arcen-a", "arcen-b", "arcen-inst"})
        self.assertTrue({"x-test", "y-test", "inst-build"} <= set(impact.requirements))

    def test_a_change_to_a_dependent_crate_does_not_require_its_dependency(self):
        write(
            self.fx.root, "shared/y/src/lib.rs", "pub fn y() { arcen_x::policy::rule(); }\n// c\n"
        )
        impact = self.impact()
        self.assertEqual(impact.affected_shared, {"arcen-y"})
        self.assertNotIn("x-test", impact.requirements)
        self.assertNotIn("b-lab", impact.requirements)
        self.assertIn("b-test", impact.requirements)

    def test_a_compiled_in_file_affects_its_includer_only_when_it_changes(self):
        # b keeps no Cargo edge to x or y; it only compiles in x's fixture.
        write(self.fx.root, "hosts/b/Cargo.toml", '[package]\nname = "arcen-b"\n')
        write(self.fx.root, "hosts/b/src/lib.rs", "unsafe fn y() {}\n")
        (self.fx.root / "hosts/b/tests/y.rs").unlink()
        write(
            self.fx.root,
            "hosts/b/src/fixture.rs",
            'unsafe fn v() -> &str { include_str!("../../../shared/x/fixtures/v.json") }\n',
        )
        self.fx.drop("shared/x/consumers.json", "arcen-b", "normal")
        self.fx.drop("shared/y/consumers.json", "arcen-b", "dev")
        self.fx.registers["shared/x/consumers.json"]["consumers"].append(
            {
                "package": "arcen-b",
                "dependency_kind": "source",
                "contracts": ["crate"],
                "includes": ["shared/x/fixtures/v.json"],
                "paths": [
                    path(
                        "windows-auto",
                        ["hosts/b/src/fixture.rs"],
                        ["windows-b"],
                        ["b-test"],
                        "Windows Host",
                    )
                ],
            }
        )
        self.assertEqual(self.fx.failures(), [])
        self.base = self.fx.commit("b only includes a fixture")
        write(self.fx.root, "shared/x/src/policy.rs", "pub fn rule() { /* changed */ }\n")
        impact = self.impact()
        self.assertNotIn("arcen-b", impact.affected_products)
        self.assertNotIn("b-test", impact.requirements)
        self.base = self.fx.git("rev-parse", "HEAD")
        write(self.fx.root, "shared/x/fixtures/v.json", '{"v": 2}\n')
        impact = self.impact()
        self.assertIn("arcen-b", impact.affected_products)
        self.assertIn(("arcen-x", "arcen-b", "windows-auto"), impact.requirements["b-test"].reasons)

    def test_a_shared_asset_change_counts_as_a_crate_change(self):
        write(self.fx.root, "shared/x/fixtures/v.json", '{"v": 2}\n')
        self.assertIn("arcen-x", self.impact().changed)

    def test_documentation_only_changes_need_only_register_checks(self):
        write(self.fx.root, "shared/x/README.md", "# x\n\nMore words.\n")
        impact = self.impact()
        self.assertEqual(impact.changed, {})
        self.assertEqual(sorted(impact.requirements), ["registry"])

    def test_deleting_a_dependency_keeps_the_existing_obligation(self):
        manifest = textwrap.dedent(MANIFESTS["hosts/b/Cargo.toml"]).replace(
            '[target.\'cfg(windows)\'.dependencies]\narcen-x = { path = "../../shared/x", features = ["simd"] }\n',
            "",
        )
        write(self.fx.root, "hosts/b/Cargo.toml", manifest)
        self.fx.drop("shared/x/consumers.json", "arcen-b", "normal")
        write(self.fx.root, "shared/x/src/policy.rs", "pub fn rule() { /* changed */ }\n")
        impact = self.impact()
        self.assertIn("b-lab", impact.requirements)
        self.assertIn(("arcen-x", "arcen-b", "windows-auto"), impact.requirements["b-lab"].reasons)
        self.assertTrue(any("register changed" in item for item in impact.review))

    def test_a_retired_validation_needs_owner_review(self):
        write(
            self.fx.root,
            checker.CATALOGUE_PATH,
            CATALOGUE.replace("[validation.b-lab]", "[validation.b-lab-renamed]"),
        )
        for register in self.fx.registers.values():
            for record in register["consumers"]:
                for p in record.get("paths", []):
                    p["validation"] = [
                        "b-lab-renamed" if v == "b-lab" else v for v in p["validation"]
                    ]
        write(self.fx.root, "shared/x/src/policy.rs", "pub fn rule() { /* changed */ }\n")
        impact = self.impact()
        self.assertIn("b-lab", impact.retired)
        self.assertIn("b-lab-renamed", impact.requirements)
        self.assertTrue(any("validation b-lab was required" in item for item in impact.review))
        self.assertTrue(any("validation catalogue changed" in item for item in impact.review))

    def test_a_retired_validation_keeps_the_gate_closed_until_acknowledged(self):
        write(
            self.fx.root,
            checker.CATALOGUE_PATH,
            CATALOGUE.replace("[validation.b-lab]", "[validation.b-lab-renamed]"),
        )
        for register in self.fx.registers.values():
            for record in register["consumers"]:
                for p in record.get("paths", []):
                    p["validation"] = [v for v in p["validation"] if v != "b-lab"]
        write(self.fx.root, "shared/x/src/policy.rs", "pub fn rule() { /* changed */ }\n")
        impact, snap = checker.impact_between(self.fx.root, self.base, self.fx.commit("head"))
        passed = [
            {
                "revision": impact.head,
                "validation": v,
                "target": snap.catalogue.resolved(v).get("rust_target")
                or (
                    "linux"
                    if snap.catalogue.resolved(v)["target_os"] == "any"
                    else snap.catalogue.resolved(v)["target_os"]
                ),
                "features": snap.catalogue.resolved(v)["features"],
                "result": "passed",
            }
            for v in impact.requirements
        ]
        _, missing = checker.render_impact(impact, snap.catalogue, passed, False)
        self.assertEqual(
            missing, ["b-lab: retired without owner acknowledgement (--acknowledge-retired b-lab)"]
        )
        text, missing = checker.render_impact(impact, snap.catalogue, passed, False, ["b-lab"])
        self.assertEqual(missing, [])
        self.assertIn("[acknowledged by owner] b-lab", text)
        document = checker.impact_json(impact, snap.catalogue, passed, ["b-lab"])
        self.assertEqual(document["retired"][0]["acknowledged"], True)

    def gate_after(self, impact, catalogue, acknowledged=()):
        """What keeps the gate closed when every required validation passed."""
        passed = []
        for validation_id in impact.requirements:
            validation = catalogue.resolved(validation_id)
            target = validation.get("rust_target") or validation["target_os"]
            passed.append(
                {
                    "revision": impact.head,
                    "validation": validation_id,
                    "target": "linux" if target == "any" else target,
                    "features": validation["features"],
                    "result": "passed",
                }
            )
        return checker.render_impact(impact, catalogue, passed, False, acknowledged)[1]

    def test_a_catalogue_and_register_only_retirement_keeps_the_gate_closed(self):
        write(
            self.fx.root,
            checker.CATALOGUE_PATH,
            CATALOGUE.replace("[validation.b-lab]", "[validation.b-lab-gone]"),
        )
        for register in self.fx.registers.values():
            for record in register["consumers"]:
                for p in record.get("paths", []):
                    p["validation"] = [v for v in p["validation"] if v != "b-lab"]
        impact, snap = checker.impact_between(self.fx.root, self.base, self.fx.commit("drop b-lab"))
        self.assertEqual(impact.changed, {})
        self.assertEqual(sorted(impact.requirements), ["registry"])
        self.assertIn(("arcen-x", "arcen-b", "windows-auto"), impact.retired["b-lab"])
        self.assertEqual(
            self.gate_after(impact, snap.catalogue),
            ["b-lab: retired without owner acknowledgement (--acknowledge-retired b-lab)"],
        )
        self.assertEqual(self.gate_after(impact, snap.catalogue, ["b-lab"]), [])

    def test_dropping_a_validation_from_a_path_only_is_a_retirement(self):
        record = self.fx.record("shared/x/consumers.json", "arcen-a", "normal")
        record["paths"][0] = dict(record["paths"][0], validation=["a-test"])
        record = self.fx.record("shared/y/consumers.json", "arcen-a", "normal")
        record["paths"][0] = dict(record["paths"][0], validation=["a-test"])
        impact, snap = checker.impact_between(self.fx.root, self.base, self.fx.commit("no a-lab"))
        self.assertEqual(sorted(impact.retired), ["a-lab"])
        self.assertTrue(self.gate_after(impact, snap.catalogue))
        self.assertEqual(self.gate_after(impact, snap.catalogue, ["a-lab"]), [])

    def test_a_catalogue_only_weakening_keeps_the_gate_closed(self):
        write(
            self.fx.root,
            checker.CATALOGUE_PATH,
            CATALOGUE.replace(
                '[validation.b-lab]\ndescription = "b on hardware"',
                '[validation.b-lab]\ndescription = "b on hardware"\nmandatory = false',
            ),
        )
        impact, snap = checker.impact_between(self.fx.root, self.base, self.fx.commit("weaken"))
        self.assertEqual(sorted(impact.requirements), ["registry"])
        self.assertEqual(impact.weakened, {"b-lab": ["mandatory True -> False"]})
        self.assertEqual(
            self.gate_after(impact, snap.catalogue),
            ["b-lab: weakened without owner acknowledgement (--acknowledge-retired b-lab)"],
        )
        self.assertEqual(self.gate_after(impact, snap.catalogue, ["b-lab"]), [])
        # Making a validation mandatory is not a weakening.
        self.base = self.fx.git("rev-parse", "HEAD")
        write(self.fx.root, checker.CATALOGUE_PATH, CATALOGUE)
        impact, _ = checker.impact_between(self.fx.root, self.base, self.fx.commit("strengthen"))
        self.assertEqual(impact.weakened, {})

    def test_weakening_a_validation_the_change_requires_still_needs_acknowledgement(self):
        write(
            self.fx.root,
            checker.CATALOGUE_PATH,
            CATALOGUE.replace(
                '[validation.b-lab]\ndescription = "b on hardware"',
                '[validation.b-lab]\ndescription = "b on hardware"\nmandatory = false',
            ),
        )
        write(self.fx.root, "shared/x/src/policy.rs", "pub fn rule() { /* changed */ }\n")
        impact, snap = checker.impact_between(self.fx.root, self.base, self.fx.commit("weaken+src"))
        self.assertIn("b-lab", impact.requirements)
        self.assertTrue(checker.evaluate(impact, snap.catalogue, "b-lab", None)["mandatory"])
        self.assertEqual(impact.weakened, {"b-lab": ["mandatory True -> False"]})
        self.assertEqual(
            self.gate_after(impact, snap.catalogue),
            ["b-lab: weakened without owner acknowledgement (--acknowledge-retired b-lab)"],
        )
        self.assertEqual(self.gate_after(impact, snap.catalogue, ["b-lab"]), [])

    def cli(self, *args: str) -> tuple[int, dict, str]:
        result = subprocess.run(
            [
                sys.executable,
                str(Path(checker.__file__)),
                "--root",
                str(self.fx.root),
                "--impact",
                "--base",
                self.base,
                "--format",
                "json",
                *args,
            ],
            capture_output=True,
            text=True,
        )
        return result.returncode, json.loads(result.stdout), result.stderr

    def test_json_output_always_parses(self):
        write(self.fx.root, "shared/x/src/policy.rs", "pub fn rule() { /* changed */ }\n")
        self.fx.commit("head")
        code, document, _ = self.cli()
        self.assertEqual(code, 0)
        self.assertIsNone(document["gate"])
        code, document, stderr = self.cli("--gate")
        self.assertEqual(code, 1)
        self.assertFalse(document["gate"]["ok"])
        self.assertIn("shared consumer gate", stderr)
        write(self.fx.root, "untracked.rs", "fn u() {}\n")
        code, document, stderr = self.cli("--run", "--gate")
        self.assertEqual(code, 1)
        self.assertIn("uncommitted changes", document["run_refusal"])
        self.assertIn("--run refused", stderr)
        code, document, stderr = self.cli("--acknowledge-retired", "no-such-validation")
        self.assertEqual(code, 1)
        self.assertEqual(document["unmatched_acknowledgements"], ["no-such-validation"])
        self.assertIn("names no retired or weakened validation", stderr)

    def test_json_output_parses_when_run_executes_validations(self):
        write(self.fx.root, "shared/x/README.md", "# x\n\nChanged.\n")
        head = self.fx.commit("documentation")
        code, document, stderr = self.cli("--run", "--gate")
        self.assertEqual(code, 0, stderr)
        self.assertIsNone(document["run_refusal"])
        self.assertEqual(document["gate"], {"ok": True, "failures": []})
        self.assertEqual(document["head"], head)
        self.assertIn("stand-in register check output", stderr)

    def test_a_weakened_validation_is_judged_by_its_base_definition(self):
        catalogue = CATALOGUE.replace(
            'build_profile = "windows-b"\ntemplate = "recorded"',
            'build_profile = "windows-b"\ntemplate = "recorded"\nmandatory = false',
        ).replace(
            '[validation.b-test]\ndescription = "b tests"',
            '[validation.b-test]\ndescription = "b tests"\nrust_target = "aarch64-pc-windows-msvc"',
        )
        write(self.fx.root, checker.CATALOGUE_PATH, catalogue)
        write(self.fx.root, "shared/x/src/policy.rs", "pub fn rule() { /* changed */ }\n")
        impact, snap = checker.impact_between(self.fx.root, self.base, self.fx.commit("head"))
        catalogue = snap.catalogue
        self.assertFalse(catalogue.resolved("b-lab")["mandatory"])
        self.assertTrue(checker.evaluate(impact, catalogue, "b-lab", None)["mandatory"])
        _, missing = checker.render_impact(impact, catalogue, [], False)
        self.assertTrue(any(m.startswith("b-lab: not_run") for m in missing), missing)

        def record(target):
            return {
                "revision": impact.head,
                "validation": "b-test",
                "target": target,
                "result": "passed",
            }

        head_only = [record("aarch64-pc-windows-msvc")]
        result = checker.evaluate(impact, catalogue, "b-test", head_only)
        self.assertEqual(result["status"], "not_run")
        self.assertIn("base definition", result["why"])
        both = head_only + [record("x86_64-pc-windows-msvc")]
        self.assertEqual(checker.evaluate(impact, catalogue, "b-test", both)["status"], "passed")
        produced = checker.run_local_validations(
            self.fx.root, impact, catalogue, lambda argv, cwd: 0
        )
        note = {r["validation"]: r.get("note") for r in produced["results"]}["b-test"]
        self.assertIn("definition changed since the base", note)

    def test_a_renamed_crate_keeps_both_names_obligations(self):
        for name in (
            "Cargo.toml",
            "shared/x/Cargo.toml",
            "shared/y/Cargo.toml",
            "hosts/a/Cargo.toml",
        ):
            text = (self.fx.root / name).read_text().replace("arcen-x", "arcen-z")
            write(self.fx.root, name, text)
        manifest = textwrap.dedent(MANIFESTS["hosts/b/Cargo.toml"]).replace(
            "[target.'cfg(windows)'.dependencies]\n"
            'arcen-x = { path = "../../shared/x", features = ["simd"] }\n',
            "",
        )
        write(self.fx.root, "hosts/b/Cargo.toml", manifest)
        register = self.fx.registers["shared/x/consumers.json"]
        register["crate"] = "arcen-z"
        self.fx.drop("shared/x/consumers.json", "arcen-b", "normal")
        write(self.fx.root, "shared/x/src/policy.rs", "pub fn rule() { /* changed */ }\n")
        impact = self.impact()
        for name in ("arcen-x", "arcen-z"):
            self.assertIn("source shared/x/src/policy.rs", impact.changed[name])
        self.assertIn(("arcen-x", "arcen-b", "windows-auto"), impact.requirements["b-lab"].reasons)
        again = checker.impact_between(self.fx.root, self.base, "HEAD")[0]
        self.assertEqual(again.requirements, impact.requirements)

    def test_markdown_compiled_in_anywhere_in_the_crate_is_source(self):
        write(
            self.fx.root,
            "shared/x/src/policy.rs",
            '#[doc = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"))]\n'
            "pub fn rule() {}\n",
        )
        write(
            self.fx.root,
            "shared/y/build.rs",
            'const _N: &str = include_str!("NOTES.md");\nfn main() {}\n',
        )
        write(self.fx.root, "shared/y/NOTES.md", "notes\n")
        write(self.fx.root, "shared/y/OTHER.md", "other\n")
        self.base = self.fx.commit("compile in markdown")
        write(self.fx.root, "shared/x/README.md", "# x\n\nChanged.\n")
        write(self.fx.root, "shared/y/NOTES.md", "changed notes\n")
        write(self.fx.root, "shared/y/OTHER.md", "changed other\n")
        impact = self.impact()
        self.assertIn("source shared/x/README.md", impact.changed["arcen-x"])
        self.assertIn("source shared/y/NOTES.md", impact.changed["arcen-y"])
        self.assertEqual(impact.doc_only, {"arcen-y": {"shared/y/OTHER.md"}})

    def test_a_lockfile_change_reaches_the_shared_crates_that_use_it(self):
        lock = '[[package]]\nname = "{n}"\nversion = "1.0.0"\ndependencies = [{d}]\n'
        base = "\n".join(
            [
                lock.format(n="arcen-x", d='"leaf"'),
                lock.format(n="arcen-y", d='"arcen-x"'),
                lock.format(n="arcen-z", d=""),
                '[[package]]\nname = "leaf"\nversion = "1.0.0"\nchecksum = "a"\n',
            ]
        )
        head = base.replace('checksum = "a"', 'checksum = "b"')
        changed = checker.lock_changed_shared(base, head, {"arcen-x", "arcen-y", "arcen-z"})
        self.assertEqual(changed, {"arcen-x", "arcen-y"})


class EvidenceTest(unittest.TestCase):
    def setUp(self):
        self.fx = Fixture()
        self.base = self.fx.commit("base")
        write(self.fx.root, "shared/x/src/policy.rs", "pub fn rule() { /* changed */ }\n")
        self.head = self.fx.commit("head")
        self.impact, snap = checker.impact_between(self.fx.root, self.base, self.head)
        self.catalogue = snap.catalogue

    def evidence(self, results: list[dict], revision: str | None = None) -> list[dict]:
        path = self.fx.root / "evidence.json"
        path.write_text(
            json.dumps({"schema_version": 1, "revision": revision or self.head, "results": results})
        )
        records, errors = checker.load_evidence([path])
        self.assertEqual(errors, [])
        return records

    def missing(self, evidence: list[dict] | None) -> list[str]:
        return checker.render_impact(self.impact, self.catalogue, evidence, False)[1]

    def test_absent_evidence_never_counts_as_passed(self):
        missing = self.missing(None)
        self.assertTrue(any(item.startswith("a-lab: not_run") for item in missing), missing)
        self.assertEqual(
            checker.validation_status(self.catalogue, "a-lab", self.head, []),
            ("not_run", "no evidence"),
        )

    def test_evidence_for_another_revision_target_or_feature_set_does_not_count(self):
        passed = {"validation": "b-test", "target": "x86_64-pc-windows-msvc", "result": "passed"}
        records = self.evidence([passed], revision=self.base)
        status, why = checker.validation_status(self.catalogue, "b-test", self.head, records)
        self.assertEqual(status, "not_run")
        self.assertIn("evidence is for revision", why)
        records = self.evidence([dict(passed, target="windows")])
        self.assertEqual(
            checker.validation_status(self.catalogue, "b-test", self.head, records)[0], "not_run"
        )
        records = self.evidence([dict(passed, features=["simd"])])
        self.assertEqual(
            checker.validation_status(self.catalogue, "b-test", self.head, records)[0], "not_run"
        )
        records = self.evidence([passed])
        self.assertEqual(
            checker.validation_status(self.catalogue, "b-test", self.head, records)[0], "passed"
        )

    def test_blocked_and_failed_results_are_not_passes(self):
        records = self.evidence(
            [
                {"validation": "a-lab", "target": "linux", "result": "blocked"},
                {"validation": "b-lab", "target": "x86_64-pc-windows-msvc", "result": "passed"},
                {"validation": "b-lab", "target": "x86_64-pc-windows-msvc", "result": "failed"},
            ]
        )
        missing = self.missing(records)
        self.assertTrue(any(item.startswith("a-lab: blocked") for item in missing), missing)
        self.assertTrue(any(item.startswith("b-lab: failed") for item in missing), missing)

    def test_the_gate_passes_only_when_every_mandatory_validation_passed(self):
        results = []
        for validation_id in self.impact.requirements:
            validation = self.catalogue.resolved(validation_id)
            target = validation.get("rust_target") or validation["target_os"]
            results.append(
                {
                    "validation": validation_id,
                    "target": "linux" if target == "any" else target,
                    "features": validation["features"],
                    "result": "passed",
                }
            )
        self.assertEqual(self.missing(self.evidence(results)), [])
        self.assertTrue(self.missing(self.evidence(results[1:])))

    def test_malformed_evidence_is_an_error(self):
        path = self.fx.root / "bad.json"
        path.write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "revision": self.head,
                    "results": [{"validation": "a-lab", "result": "ok"}],
                }
            )
        )
        _, errors = checker.load_evidence([path])
        self.assertTrue(any("result must be one of" in e for e in errors), errors)

    def test_local_runs_refuse_a_tree_that_is_not_exactly_the_head(self):
        def results():
            produced = checker.run_local_validations(
                self.fx.root, self.impact, self.catalogue, lambda argv, cwd: 0
            )
            self.assertEqual(produced["revision"], self.head)
            return {r["validation"]: r for r in produced["results"]}

        self.assertEqual(results()["x-test"]["result"], "passed")
        (self.fx.root / ".git/info/exclude").write_text("ignored.log\n")
        write(self.fx.root, "ignored.log", "build noise\n")
        self.assertEqual(results()["x-test"]["result"], "passed")
        write(self.fx.root, "shared/x/src/policy.rs", "pub fn rule() { /* dirty */ }\n")
        dirty = results()["x-test"]
        self.assertEqual(dirty["result"], "not_run")
        self.assertIn("uncommitted changes", dirty["note"])
        self.fx.git("checkout", "-q", "--", "shared/x/src/policy.rs")
        write(self.fx.root, "untracked.rs", "fn u() {}\n")
        self.assertEqual(results()["x-test"]["result"], "not_run")
        (self.fx.root / "untracked.rs").unlink()
        self.fx.git("checkout", "-q", self.base)
        moved = results()["x-test"]
        self.assertEqual(moved["result"], "not_run")
        self.assertIn("not the head revision", moved["note"])

    def test_local_runs_execute_only_catalogue_argv_for_this_host(self):
        calls = []

        def runner(argv, cwd):
            calls.append(argv)
            return 0

        produced = checker.run_local_validations(self.fx.root, self.impact, self.catalogue, runner)
        self.assertEqual(produced["revision"], self.head)
        by_id = {r["validation"]: r for r in produced["results"]}
        self.assertEqual(by_id["a-lab"]["result"], "not_run")
        self.assertEqual(by_id["x-test"]["result"], "passed")
        self.assertIn(["cargo", "test", "--locked", "-p", "arcen-x"], calls)
        self.assertTrue(all(isinstance(argv, list) for argv in calls))
        host = checker.host_os()
        self.assertEqual(by_id["b-test"]["result"], "passed" if host == "windows" else "not_run")


if __name__ == "__main__":
    unittest.main()
