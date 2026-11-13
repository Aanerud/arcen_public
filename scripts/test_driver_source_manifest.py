# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS
"""Tests for scripts/ci/driver_source_manifest.py."""

import hashlib
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent / "ci"))

import driver_source_manifest as manifest  # noqa: E402


class DriverSourceManifestTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(dir=Path(__file__).resolve().parent)
        self.dir = Path(self.temp.name)
        (self.dir / "driver.cpp").write_bytes(b"int main() {}\n")
        (self.dir / "sub").mkdir()
        (self.dir / "sub" / "x.h").write_bytes(b"#pragma once\n")
        (self.dir / "obj").mkdir()
        (self.dir / "obj" / "build.log").write_bytes(b"ignored\n")
        manifest.write(self.dir)

    def tearDown(self):
        self.temp.cleanup()

    def test_a_written_manifest_matches_the_powershell_rules(self):
        data = json.loads((self.dir / "source-manifest.json").read_text())
        self.assertEqual(data["schema_version"], 1)
        self.assertEqual(sorted(data["files"]), ["driver.cpp", "sub/x.h"])
        self.assertEqual(
            data["files"]["driver.cpp"],
            hashlib.sha256(b"int main() {}\n").hexdigest().upper(),
        )
        self.assertEqual(manifest.problems(self.dir), [])

    def test_an_edited_file_fails_the_check(self):
        (self.dir / "driver.cpp").write_bytes(b"// header\nint main() {}\n")
        self.assertEqual(len(manifest.problems(self.dir)), 1)
        self.assertEqual(manifest.main(["--check", str(self.dir)]), 1)

    def test_missing_and_extra_files_fail_the_check(self):
        (self.dir / "sub" / "x.h").unlink()
        (self.dir / "new.h").write_bytes(b"\n")
        found = manifest.problems(self.dir)
        self.assertTrue(any("does not exist" in p for p in found))
        self.assertTrue(any("new.h is not in the manifest" in p for p in found))

    def test_the_repository_manifests_are_current(self):
        for directory in manifest.default_directories():
            self.assertEqual(manifest.problems(directory), [], directory)


if __name__ == "__main__":
    unittest.main()
