import importlib.util
import pathlib
import sys
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "verify_quic_product_binary",
    ROOT / "scripts" / "verify_quic_product_binary.py",
)
VERIFIER = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
sys.modules[SPEC.name] = VERIFIER
SPEC.loader.exec_module(VERIFIER)


class QuicProductBinaryTests(unittest.TestCase):
    def verify_bytes(self, content):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "product.bin"
            path.write_bytes(content)
            VERIFIER.verify(path)

    def test_accepts_quic_only_binary(self):
        self.verify_bytes(b"transport:quic-v1\0quic://pier:18444")

    def test_rejects_each_dormant_wss_marker(self):
        for marker in VERIFIER.FORBIDDEN:
            with self.subTest(marker=marker):
                with self.assertRaisesRegex(ValueError, "dormant WSS marker"):
                    self.verify_bytes(b"prefix\0" + marker + b"\0suffix")

    def test_rejects_marker_crossing_read_boundary(self):
        marker = VERIFIER.FORBIDDEN[0]
        prefix = b"x" * (VERIFIER.CHUNK_BYTES - len(marker) // 2)
        with self.assertRaisesRegex(ValueError, "dormant WSS marker"):
            self.verify_bytes(prefix + marker)


if __name__ == "__main__":
    unittest.main()
