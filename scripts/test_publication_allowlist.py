# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS
"""Tests for the canonical-URL allowlist of the publication checks.

The account name is assembled at run time so this file never contains it as
text the hygiene gate would match.
"""

import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPTS / "ci"))

import publication_allowlist as allowlist  # noqa: E402

ACCOUNT = "Aan" + "erud"
REPO_NAME = "arcen" + "_public"
RULE = ACCOUNT.lower()
CANONICAL = allowlist.CANONICAL_URL


# Shapes that once slipped through because the whole URL, path included, was
# blanked: the account name or an address hidden in the path or query.
BYPASSES = [
    f"{CANONICAL}/tree/main/{ACCOUNT}-private-notes",
    f"{CANONICAL}/tree/main?x=name.{RULE}@mail.example",
    f"{CANONICAL}/compare/v1...{ACCOUNT}:other-repo:main",
    f"{CANONICAL}/tree/main/Users/{RULE}/x",
]


def hit(text: str) -> bool:
    return bool(allowlist.remaining_hits([f"f.md:1:{text}"], RULE))


class CanonicalUrlTests(unittest.TestCase):
    def test_the_canonical_url_and_its_allowed_paths_pass(self):
        for text in [
            f"Source: {CANONICAL}",
            f"see {CANONICAL}.",
            f"[Releases]({CANONICAL}/releases) and",
            f"[0.17.0]: {CANONICAL}/releases/tag/v0.17.0",
            f"{CANONICAL}/releases/latest",
            f'pub const BASE: &str = "{CANONICAL}/tree/main/";',
            f"[Unreleased]: {CANONICAL}/compare/v0.18.0...HEAD",
            f'"{CANONICAL}/tree/main/docs/operations/error-codes.md#colour-check"',
            f"[CHANGELOG]({CANONICAL}/blob/v0.19.0/CHANGELOG.md#0190--2026-11-12)",
            f"{CANONICAL}/blob/v0.19.0-rc1/README.md",
            CANONICAL.removeprefix("https://"),
        ]:
            self.assertFalse(hit(text), text)

    def test_every_other_reference_to_the_account_still_fails(self):
        for text in [
            ACCOUNT,
            f"Author: {ACCOUNT}",
            f"someone.{RULE}@mail.example",
            f"https://github.com/{ACCOUNT}/another-repo",
            f"https://github.com/{ACCOUNT}",
            f"{ACCOUNT}/{REPO_NAME}",
            f"{CANONICAL}/blob/main/README.md",
            f"{CANONICAL}/blob/v0.19.0",
            f"{CANONICAL}/blob/feature/README.md",
            f"{CANONICAL}/issues/1",
            f"{CANONICAL}.git",
            f"{CANONICAL}_fork",
            f"https://evil.example/github.com/{ACCOUNT}/{REPO_NAME}",
            f"{CANONICAL} and also {ACCOUNT}",
            *BYPASSES,
        ]:
            self.assertTrue(hit(text), text)

    def test_blanking_keeps_offsets_and_the_path(self):
        text = f"x {CANONICAL}/tree/main/docs/a.md y"
        blanked = allowlist.strip_canonical(text)
        self.assertEqual(len(blanked), len(text))
        self.assertIn("/tree/main/docs/a.md", blanked)
        self.assertNotIn(REPO_NAME, blanked)

    def test_query_and_extra_punctuation_are_not_the_canonical_url(self):
        for text in [f"{CANONICAL}/tree/main?x=1", f"{CANONICAL}/compare/a...b:c",
                     f"{CANONICAL}/releases/tag/v1@x", f"{CANONICAL}/tree/dev/x"]:
            self.assertEqual(allowlist.strip_canonical(text), text, text)


class HygieneGateTests(unittest.TestCase):
    """Runs the real gate over a scratch repository with a scratch denylist."""

    def setUp(self):
        self.root = Path(tempfile.mkdtemp(dir=SCRIPTS))
        subprocess.run(["git", "init", "-q", str(self.root)], check=True)
        self.denylist = self.root.parent / f"{self.root.name}-denylist.txt"
        self.denylist.write_text(f"personal account name is present|{RULE}\n")

    def tearDown(self):
        shutil.rmtree(self.root, ignore_errors=True)
        self.denylist.unlink(missing_ok=True)

    def gate_flags(self, content: str) -> bool:
        (self.root / "doc.md").write_text(content + "\n")
        env = {**os.environ, "ARCEN_PUBLICATION_DENYLIST": str(self.denylist)}
        result = subprocess.run(
            ["bash", str(SCRIPTS / "ci" / "check-publication-hygiene.sh"), str(self.root)],
            cwd=self.root, env=env, capture_output=True, text=True,
        )
        return "FAIL: personal account name is present" in result.stderr

    def test_the_gate_allows_the_canonical_url(self):
        self.assertFalse(self.gate_flags(f"Source: {CANONICAL}/releases"))

    def test_the_gate_rejects_the_account_name_an_email_and_another_repo(self):
        self.assertTrue(self.gate_flags(f"by {ACCOUNT}"))
        self.assertTrue(self.gate_flags(f"mail someone.{RULE}@mail.example"))
        self.assertTrue(self.gate_flags(f"https://github.com/{ACCOUNT}/another-repo"))

    def test_the_gate_rejects_names_and_addresses_hidden_in_the_url_path(self):
        for text in BYPASSES:
            self.assertTrue(self.gate_flags(f"see {text}"), text)


if __name__ == "__main__":
    unittest.main()
