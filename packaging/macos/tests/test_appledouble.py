"""macOS packages and zips ship without AppleDouble ("._") entries.

On macOS 26 every file a build writes carries `com.apple.provenance`, which
cannot be removed, and both `pkgbuild` and `ditto -c -k` turn it into a "._"
companion for every file. strip_appledouble.py and zip-app.sh remove them.
"""

import importlib.util
import pathlib
import shutil
import struct
import subprocess
import sys
import tempfile
import unittest


MACOS = pathlib.Path(__file__).resolve().parents[1]
REPO = MACOS.parents[1]
SPEC = importlib.util.spec_from_file_location("strip_appledouble", MACOS / "strip_appledouble.py")
STRIP = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(STRIP)

# A companion pkgbuild wrote for pier.json on macOS 26: Finder info empty, no
# resource fork, one attribute, com.apple.provenance.
PROVENANCE_ONLY = bytes.fromhex(
    "00051607000200004d6163204f5320582020202020202020000200000009000000320000"
    "007100000002000000a30000000000000000000000000000000000000000000000000000"
    "0000000000000000000000004154545200000000000000a3000000980000000b00000000"
    "000000000000000000000001000000980000000b000015636f6d2e6170706c652e70726f"
    "76656e616e63650001020017c222a46b821678"
)


def appledouble(attributes, resource_fork=b"", finder_info=bytes(32)):
    """Build an AppleDouble file in copyfile(3)'s layout."""
    entries = b""
    cursor = 0
    for name, _ in attributes:
        raw = name.encode() + b"\0"
        entry = struct.pack(">IIHB", 0, 0, 0, len(raw)) + raw
        entry += bytes(-len(entry) % 4)
        entries += entry
    header_offset = 26 + 2 * 12 + 32 + 2
    data_start = header_offset + 36 + len(entries)
    values = b"".join(value for _, value in attributes)
    finder = bytearray(finder_info + b"\0\0")
    attr_header = b"ATTR" + struct.pack(
        ">IIII12xHH", 0, data_start + len(values), data_start, len(values), 0, len(attributes)
    )
    patched = bytearray(entries)
    position = 0
    for name, value in attributes:
        raw_len = len(name) + 1
        struct.pack_into(">II", patched, position, data_start + cursor, len(value))
        cursor += len(value)
        position = (position + 11 + raw_len + 3) & ~3
    finder += attr_header + patched + values
    finder_length = len(finder)
    fork_offset = 26 + 2 * 12 + finder_length
    head = struct.pack(">II16sH", 0x00051607, 0x00020000, b"Mac OS X        ", 2)
    head += struct.pack(">III", 9, 50, finder_length)
    head += struct.pack(">III", 2, fork_offset, len(resource_fork))
    return head + bytes(finder) + resource_fork


def odc_entry(name, body=b"", mode=0o100644):
    raw_name = name.encode() + b"\0"
    header = b"070707" + b"".join(
        f"{value:0{width}o}".encode()
        for value, width in (
            (0, 6), (1, 6), (mode, 6), (0, 6), (0, 6), (1, 6), (0, 6), (0, 11),
            (len(raw_name), 6), (len(body), 11),
        )
    )
    return header + raw_name + body


def odc(*entries):
    return b"".join(entries) + odc_entry("TRAILER!!!", mode=0)


class AppleDoubleParsingTests(unittest.TestCase):
    def test_reads_a_real_provenance_companion(self):
        self.assertEqual(STRIP.appledouble_attributes(PROVENANCE_ONLY), ["com.apple.provenance"])

    def test_reads_built_companions(self):
        data = appledouble([("com.apple.provenance", b"\x01" * 11), ("x.y", b"z")])
        self.assertEqual(STRIP.appledouble_attributes(data), ["com.apple.provenance", "x.y"])

    def test_refuses_a_resource_fork(self):
        with self.assertRaises(STRIP.StripError):
            STRIP.appledouble_attributes(appledouble([], resource_fork=b"fork"))

    def test_refuses_finder_info(self):
        with self.assertRaises(STRIP.StripError):
            STRIP.appledouble_attributes(appledouble([], finder_info=b"TEXT" + bytes(28)))

    def test_refuses_other_attributes(self):
        with self.assertRaises(STRIP.StripError):
            STRIP.check_droppable("x", appledouble([("com.apple.quarantine", b"q")]))


class CpioStripTests(unittest.TestCase):
    def test_drops_provenance_companions_and_keeps_everything_else(self):
        archive = odc(
            odc_entry(".", mode=0o40755),
            odc_entry("./a", b"payload"),
            odc_entry("./._a", PROVENANCE_ONLY),
            odc_entry("./d", mode=0o40755),
            odc_entry("./._d", PROVENANCE_ONLY),
        )
        data, dropped, kept = STRIP.strip_cpio(archive, "Payload")
        self.assertEqual(dropped, ["./._a", "./._d"])
        self.assertEqual(kept, 3)
        self.assertEqual(
            data,
            odc(odc_entry(".", mode=0o40755), odc_entry("./a", b"payload"), odc_entry("./d", mode=0o40755)),
        )

    def test_refuses_a_companion_without_its_file(self):
        with self.assertRaises(STRIP.StripError):
            STRIP.strip_cpio(odc(odc_entry("./._lonely", PROVENANCE_ONLY)), "Payload")

    def test_refuses_a_companion_with_content(self):
        archive = odc(odc_entry("./a"), odc_entry("./._a", appledouble([("tech.arcen.x", b"1")])))
        with self.assertRaises(STRIP.StripError):
            STRIP.strip_cpio(archive, "Payload")


class BuildScriptTests(unittest.TestCase):
    def test_pier_pkg_strips_the_component_and_checks_the_product(self):
        lines = (MACOS / "build-pier-pkg.sh").read_text(encoding="utf-8").splitlines()
        build = next(i for i, line in enumerate(lines) if line == "pkgbuild \\")
        strip = next(i for i, line in enumerate(lines) if 'strip_appledouble.py" "$PRODUCT/$COMPONENT_PKG"' in line)
        product = next(i for i, line in enumerate(lines) if line == "productbuild \\")
        check = next(i for i, line in enumerate(lines) if 'strip_appledouble.py" --check "$PKG"' in line)
        notarize = next(i for i, line in enumerate(lines) if 'notarytool submit "$PKG"' in line)
        self.assertLess(build, strip)
        self.assertLess(strip, product)
        self.assertLess(product, check)
        self.assertLess(check, notarize)

    def test_deck_release_zip_uses_zip_app(self):
        text = (MACOS / "build-deck-app.sh").read_text(encoding="utf-8")
        self.assertIn('"$HERE/zip-app.sh" "$APP" "$RELEASE_ZIP"', text)
        self.assertNotIn('ditto -c -k --keepParent "$APP" "$RELEASE_ZIP"', text)


@unittest.skipUnless(sys.platform == "darwin" and shutil.which("pkgbuild"), "needs macOS packaging tools")
class MacToolTests(unittest.TestCase):
    def setUp(self):
        scratch = REPO / "dist"
        scratch.mkdir(exist_ok=True)
        self.workdir = tempfile.TemporaryDirectory(prefix="appledouble.", dir=scratch)
        self.addCleanup(self.workdir.cleanup)
        self.root = pathlib.Path(self.workdir.name)

    def bundle(self):
        app = self.root / "root/Applications/Probe.app/Contents"
        (app / "MacOS").mkdir(parents=True)
        (app / "MacOS/probe").write_text("#!/bin/sh\n", encoding="utf-8")
        (app / "Info.plist").write_text("<plist/>\n", encoding="utf-8")
        scripts = self.root / "scripts"
        scripts.mkdir()
        (scripts / "postinstall").write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        (scripts / "postinstall").chmod(0o755)
        return app.parent

    def pkgbuild(self):
        package = self.root / "probe.pkg"
        subprocess.run(
            ["pkgbuild", "--root", str(self.root / "root"), "--scripts", str(self.root / "scripts"),
             "--identifier", "tech.arcen.test.appledouble", "--version", "1",
             "--install-location", "/", "--ownership", "recommended", str(package)],
            check=True, capture_output=True,
        )
        return package

    def run_strip(self, *args):
        return subprocess.run(
            [sys.executable, str(MACOS / "strip_appledouble.py"), *args],
            capture_output=True, text=True,
        )

    def test_stripped_component_has_no_companions_and_a_matching_bom(self):
        self.bundle()
        package = self.pkgbuild()
        before = self.run_strip("--check", str(package))
        result = self.run_strip(str(package))
        self.assertEqual(result.returncode, 0, result.stderr)
        # Where the build's files carry com.apple.provenance (macOS 26),
        # pkgbuild did write companions, so this proves the strip, not a no-op.
        if before.returncode:
            self.assertNotIn("removed 0 ", result.stdout)
        self.assertEqual(self.run_strip("--check", str(package)).returncode, 0)
        payload = subprocess.run(
            ["pkgutil", "--payload-files", str(package)], check=True, capture_output=True, text=True
        ).stdout.splitlines()
        self.assertIn("./Applications/Probe.app/Contents/MacOS/probe", payload)
        expanded = self.root / "expanded"
        subprocess.run(["pkgutil", "--expand", str(package), str(expanded)], check=True)
        bom = subprocess.run(
            ["lsbom", "-s", str(expanded / "Bom")], check=True, capture_output=True, text=True
        ).stdout.splitlines()
        self.assertEqual(sorted(bom), sorted(payload))
        self.assertIn(f'numberOfFiles="{len(payload)}"', (expanded / "PackageInfo").read_text())
        self.assertEqual(sorted(p.name for p in (expanded / "Scripts").iterdir()), ["postinstall"])

    def test_refuses_a_companion_with_content(self):
        app = self.bundle()
        subprocess.run(["xattr", "-w", "tech.arcen.test", "1", str(app / "Contents/Info.plist")], check=True)
        package = self.pkgbuild()
        result = self.run_strip(str(package))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("tech.arcen.test", result.stderr)

    def test_zip_app_writes_no_companions(self):
        app = self.bundle()
        archive = self.root / "probe.zip"
        subprocess.run([str(MACOS / "zip-app.sh"), str(app), str(archive)], check=True)
        names = subprocess.run(
            ["zipinfo", "-1", str(archive)], check=True, capture_output=True, text=True
        ).stdout.splitlines()
        self.assertIn("Probe.app/Contents/MacOS/probe", names)
        self.assertFalse([n for n in names if "/._" in n or n.startswith(("._", "__MACOSX/"))])


if __name__ == "__main__":
    unittest.main()
