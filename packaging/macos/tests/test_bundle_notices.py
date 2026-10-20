"""macOS bundles ship the AGPL licence and the third-party notices.

The static tests read the build scripts, so they run anywhere. The artefact
tests open a built package or Deck bundle; point ARCEN_PIER_PKG or
ARCEN_DECK_APP at one to run them:

    ARCEN_PIER_PKG=dist/macos/ArcenPier-<version>.pkg \
    ARCEN_DECK_APP="dist/macos/Arcen Deck.app" \
        python3 -m unittest discover -s packaging/macos/tests
"""

import os
import pathlib
import shutil
import subprocess
import tempfile
import unittest


MACOS = pathlib.Path(__file__).resolve().parents[1]
REPO = MACOS.parents[1]
APP_SCRIPT = MACOS / "build-pier-app.sh"
PKG_SCRIPT = MACOS / "build-pier-pkg.sh"
DECK_SCRIPT = MACOS / "build-deck-app.sh"
NOTICES = ("LICENSE", "legal/THIRD_PARTY_NOTICES.md")
PAYLOAD_RESOURCES = "Applications/Arcen Pier.app/Contents/Resources"


def first_line(lines, needle, start=0):
    for index in range(start, len(lines)):
        if needle in lines[index]:
            return index
    raise AssertionError(f"{needle!r} not found")


class NoticeSourcesTests(unittest.TestCase):
    def test_licence_is_the_agpl(self):
        self.assertIn(
            "GNU AFFERO GENERAL PUBLIC LICENSE",
            (REPO / "LICENSE").read_text(encoding="utf-8"),
        )

    def test_third_party_notices_exist(self):
        self.assertTrue((REPO / "legal/THIRD_PARTY_NOTICES.md").is_file())


class PierAppScriptTests(unittest.TestCase):
    def setUp(self):
        self.lines = APP_SCRIPT.read_text(encoding="utf-8").splitlines()

    def test_copies_both_notices_into_the_pier_bundle(self):
        loop = first_line(self.lines, "for notice in LICENSE legal/THIRD_PARTY_NOTICES.md")
        copy = first_line(self.lines, 'cp "$REPO/$notice" "$APP/Contents/Resources/', loop)
        self.assertLess(copy - loop, 4, "the copy belongs to the notice loop")

    def test_copies_notices_before_signing(self):
        copy = first_line(self.lines, 'cp "$REPO/$notice" "$APP/Contents/Resources/')
        signing = first_line(self.lines, "codesign ")
        self.assertLess(copy, signing, "notices must be inside the signed seal")


class PierPkgScriptTests(unittest.TestCase):
    def setUp(self):
        self.lines = PKG_SCRIPT.read_text(encoding="utf-8").splitlines()

    def test_payload_notices_are_verified_against_source(self):
        loop = first_line(self.lines, "for notice in LICENSE legal/THIRD_PARTY_NOTICES.md")
        check = first_line(self.lines, 'cmp -s "$REPO/$notice" "$RESOURCES/', loop)
        self.assertLess(check - loop, 3)

    def test_verification_precedes_signing_and_packaging(self):
        check = first_line(self.lines, 'cmp -s "$REPO/$notice"')
        self.assertLess(check, first_line(self.lines, "codesign "))
        self.assertLess(check, first_line(self.lines, "pkgbuild \\"))

    def test_payload_resources_are_the_installed_pier_bundle(self):
        self.assertIn(
            'RESOURCES="$ROOT/Applications/Arcen Pier.app/Contents/Resources"',
            self.lines,
        )


class DeckAppScriptTests(unittest.TestCase):
    """One assembly path serves both architectures: `APP` is `Arcen Deck.app`
    for arm64 and `Arcen Deck Intel.app` for x86_64."""

    def setUp(self):
        self.lines = DECK_SCRIPT.read_text(encoding="utf-8").splitlines()

    def test_both_architectures_share_one_bundle_path(self):
        self.assertIn('    APP_NAME="Arcen Deck"', self.lines)
        self.assertIn('    APP_NAME="Arcen Deck Intel"', self.lines)
        self.assertIn('APP="$OUT/$APP_NAME.app"', self.lines)

    def test_deck_ships_licence_and_third_party_notices(self):
        for notice in NOTICES:
            name = pathlib.PurePath(notice).name
            self.assertIn(
                f'cp "$REPO/{notice}" "$APP/Contents/Resources/{name}"', self.lines
            )

    def test_deck_notices_precede_signing(self):
        signing = first_line(self.lines, "codesign --sign")
        for notice in NOTICES:
            copy = first_line(self.lines, f'cp "$REPO/{notice}" "$APP/Contents/Resources/')
            self.assertLess(copy, signing, "notices must be inside the signed seal")


@unittest.skipUnless(os.environ.get("ARCEN_DECK_APP"), "set ARCEN_DECK_APP to a built Deck bundle")
class BuiltDeckBundleTests(unittest.TestCase):
    def test_bundle_notices_match_the_source(self):
        resources = pathlib.Path(os.environ["ARCEN_DECK_APP"]) / "Contents/Resources"
        for notice in NOTICES:
            installed = resources / pathlib.PurePath(notice).name
            self.assertEqual(installed.read_bytes(), (REPO / notice).read_bytes(), str(installed))


@unittest.skipUnless(os.environ.get("ARCEN_PIER_PKG"), "set ARCEN_PIER_PKG to a built package")
class BuiltPackagePayloadTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if shutil.which("pkgutil") is None:
            raise unittest.SkipTest("pkgutil is macOS-only")
        cls.pkg = pathlib.Path(os.environ["ARCEN_PIER_PKG"]).resolve()
        if not cls.pkg.is_file():
            raise AssertionError(f"{cls.pkg} does not exist")
        scratch = REPO / "dist"
        scratch.mkdir(exist_ok=True)
        cls.workdir = tempfile.TemporaryDirectory(prefix="pkg-notices.", dir=scratch)
        cls.expanded = pathlib.Path(cls.workdir.name) / "expanded"
        subprocess.run(
            ["pkgutil", "--expand-full", str(cls.pkg), str(cls.expanded)],
            check=True,
            capture_output=True,
        )
        components = sorted(cls.expanded.glob("*.pkg/Payload"))
        if len(components) != 1:
            raise AssertionError(f"expected one component payload, found {components}")
        cls.payload = components[0]

    @classmethod
    def tearDownClass(cls):
        cls.workdir.cleanup()

    def test_payload_lists_the_notices(self):
        listed = subprocess.run(
            ["pkgutil", "--payload-files", str(self.pkg)],
            check=True,
            capture_output=True,
            text=True,
        ).stdout.splitlines()
        for notice in NOTICES:
            name = pathlib.PurePath(notice).name
            self.assertIn(f"./{PAYLOAD_RESOURCES}/{name}", listed)

    def test_payload_notices_match_the_source(self):
        for notice in NOTICES:
            installed = self.payload / PAYLOAD_RESOURCES / pathlib.PurePath(notice).name
            self.assertEqual(
                installed.read_bytes(),
                (REPO / notice).read_bytes(),
                f"{installed} differs from {notice}",
            )


if __name__ == "__main__":
    unittest.main()
