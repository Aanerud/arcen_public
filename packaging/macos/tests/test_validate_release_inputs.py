import copy
import datetime as dt
import importlib.util
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "validate_release_inputs", ROOT / "validate_release_inputs.py"
)
VALIDATOR = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(VALIDATOR)
FIXTURES = pathlib.Path(__file__).parent / "fixtures"


class ReleaseInputValidationTests(unittest.TestCase):
    def setUp(self):
        self.profile = VALIDATOR.load_plist(FIXTURES / "valid-profile.plist")
        self.entitlements = VALIDATOR.load_plist(ROOT / "Deck.entitlements")
        self.now = dt.datetime(2026, 7, 23, tzinfo=dt.timezone.utc)

    def assert_profile_rejected(self, mutate):
        profile = copy.deepcopy(self.profile)
        mutate(profile)
        with self.assertRaises(VALIDATOR.ValidationError):
            VALIDATOR.validate_profile(profile, self.entitlements, now=self.now)

    def compile_cms_verifier(self, output):
        subprocess.run(
            [
                "xcrun",
                "clang",
                "-Os",
                str(ROOT / "verify-provisioning-cms.c"),
                "-framework",
                "Security",
                "-framework",
                "CoreFoundation",
                "-o",
                str(output),
            ],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

    def test_valid_synthetic_release_metadata(self):
        VALIDATOR.validate_profile(self.profile, self.entitlements, now=self.now)
        VALIDATOR.validate_signature(
            (FIXTURES / "valid-signature.txt").read_text(encoding="utf-8")
        )

    def test_rejects_wrong_team(self):
        self.assert_profile_rejected(
            lambda profile: profile.update(TeamIdentifier=["ATTACKER00"])
        )

    def test_rejects_wrong_application_identifier(self):
        self.assert_profile_rejected(
            lambda profile: profile["Entitlements"].update(
                {"com.apple.application-identifier": "NWR7ZH8L7U.attacker.example"}
            )
        )

    def test_rejects_expired_profile(self):
        self.assert_profile_rejected(
            lambda profile: profile.update(
                ExpirationDate=dt.datetime(2020, 1, 1, tzinfo=dt.timezone.utc)
            )
        )

    def test_rejects_missing_requested_entitlement(self):
        self.assert_profile_rejected(
            lambda profile: profile["Entitlements"].pop(
                "com.apple.developer.sustained-execution"
            )
        )

    def test_release_profile_rejects_get_task_allow_true(self):
        # get-task-allow=true is the well-documented signal of a Development
        # (debuggable) profile. A release build must refuse to embed one even
        # if every other check (team/app-id/entitlement-superset) passes.
        self.assert_profile_rejected(
            lambda profile: profile["Entitlements"].update({"get-task-allow": True})
        )

    def test_release_profile_accepts_missing_get_task_allow(self):
        # The real external Developer ID profile has no get-task-allow key at
        # all (equivalent to false); this must remain valid for release.
        self.assertNotIn("get-task-allow", self.profile["Entitlements"])
        VALIDATOR.validate_profile(
            self.profile, self.entitlements, now=self.now, profile_class="release"
        )

    def test_development_profile_class_requires_get_task_allow(self):
        dev_profile = VALIDATOR.load_plist(FIXTURES / "valid-dev-profile.plist")
        VALIDATOR.validate_profile(
            dev_profile,
            self.entitlements,
            now=self.now,
            profile_class="development",
        )
        # The release fixture has no get-task-allow, so it must not satisfy
        # a development-class check.
        with self.assertRaises(VALIDATOR.ValidationError):
            VALIDATOR.validate_profile(
                self.profile,
                self.entitlements,
                now=self.now,
                profile_class="development",
            )

    def test_rejects_unknown_profile_class(self):
        with self.assertRaises(VALIDATOR.ValidationError):
            VALIDATOR.validate_profile(
                self.profile,
                self.entitlements,
                now=self.now,
                profile_class="ad-hoc",
            )

    def test_entitlements_omit_no_op_sandbox_device_and_network_keys(self):
        # Deck is not sandboxed (no com.apple.security.app-sandbox key), so
        # App-Sandbox-only device/network entitlements are no-ops and must not
        # be requested. Input Monitoring is TCC, not a documented entitlement,
        # and is not required by default AppKit tablet capture.
        for no_op_key in (
            "com.apple.security.app-sandbox",
            "com.apple.security.network.client",
            "com.apple.security.device.audio-input",
            "com.apple.security.device.input-monitoring",
        ):
            self.assertNotIn(no_op_key, self.entitlements)

    def test_entitlements_retain_only_profile_authorized_protected_keys(self):
        self.assertEqual(
            self.entitlements.get("com.apple.developer.sustained-execution"), True
        )
        self.assertIn(
            "com.apple.developer.associated-domains", self.entitlements
        )

    def test_profile_wildcard_authorizes_specific_associated_domains(self):
        self.profile["Entitlements"]["com.apple.developer.associated-domains"] = ["*"]
        VALIDATOR.validate_profile(self.profile, self.entitlements, now=self.now)

    def test_profile_scalar_wildcard_authorizes_specific_associated_domains(self):
        self.profile["Entitlements"]["com.apple.developer.associated-domains"] = "*"
        VALIDATOR.validate_profile(self.profile, self.entitlements, now=self.now)

    def test_profile_prefix_wildcard_does_not_authorize_another_domain(self):
        self.profile["Entitlements"]["com.apple.developer.associated-domains"] = [
            "applinks:internal.*"
        ]
        with self.assertRaises(VALIDATOR.ValidationError):
            VALIDATOR.validate_profile(self.profile, self.entitlements, now=self.now)

    def test_valid_synthetic_development_signature(self):
        VALIDATOR.validate_signature(
            (FIXTURES / "valid-dev-signature.txt").read_text(encoding="utf-8"),
            identity_class="apple-development",
        )

    def test_rejects_non_developer_id_or_wrong_team_signature(self):
        valid = (FIXTURES / "valid-signature.txt").read_text(encoding="utf-8")
        for hostile in (
            valid.replace("Developer ID Application", "Apple Development"),
            valid.replace("NWR7ZH8L7U", "ATTACKER00"),
            valid.replace("Identifier=deck.arcen.tech", "Identifier=evil.example"),
        ):
            with self.subTest(hostile=hostile):
                with self.assertRaises(VALIDATOR.ValidationError):
                    VALIDATOR.validate_signature(hostile)

    def test_release_and_development_certificate_classes_are_not_interchangeable(self):
        release_signature = (FIXTURES / "valid-signature.txt").read_text(
            encoding="utf-8"
        )
        dev_signature = (FIXTURES / "valid-dev-signature.txt").read_text(
            encoding="utf-8"
        )
        # A development-signed binary must never pass release (Developer ID)
        # validation, and a Developer-ID-signed binary must never pass
        # development validation.
        with self.assertRaises(VALIDATOR.ValidationError):
            VALIDATOR.validate_signature(
                dev_signature, identity_class="developer-id"
            )
        with self.assertRaises(VALIDATOR.ValidationError):
            VALIDATOR.validate_signature(
                release_signature, identity_class="apple-development"
            )

    def test_rejects_unknown_identity_class(self):
        valid = (FIXTURES / "valid-signature.txt").read_text(encoding="utf-8")
        with self.assertRaises(VALIDATOR.ValidationError):
            VALIDATOR.validate_signature(valid, identity_class="mac-app-store")

    def test_release_script_rejects_untrusted_signed_cms_before_decode_or_use(self):
        script = (ROOT / "build-deck-app.sh").read_text(encoding="utf-8")
        trust = '"$CMS_VERIFIER" "$PROFILE_SNAPSHOT"'
        decode = (
            '/usr/bin/security cms -D -i "$PROFILE_SNAPSHOT" '
            '-o "$PROFILE_METADATA"'
        )
        embed = (
            'cp "$PROFILE_SNAPSHOT" "$APP/Contents/embedded.provisionprofile"'
        )
        self.assertIn(trust, script)
        self.assertLess(script.index(trust), script.index(decode))
        self.assertLess(script.index(trust), script.index(embed))
        self.assertIn(
            "profile CMS signature or Apple trust chain is invalid",
            script,
        )
        trust_block = script[script.index(trust) : script.index(decode)]
        self.assertIn("exit 1", trust_block)
        self.assertIn(">/dev/null 2>&1", trust_block)
        verifier = (ROOT / "verify-provisioning-cms.c").read_text(encoding="utf-8")
        self.assertIn("CMSDecoderCopySignerStatus", verifier)
        self.assertRegex(
            verifier,
            r"CMSDecoderCopySignerStatus\(\s*decoder,\s*0,\s*policy,\s*false,",
        )
        self.assertIn("signer_status != kCMSSignerValid", verifier)
        self.assertIn("SecTrustEvaluateWithError", verifier)
        self.assertIn("kReviewedRootDigests", verifier)
        self.assertIn("Mac OS X Provisioning Profile Signing", verifier)
        self.assertIn("kReviewedProvisioningIntermediateDigests", verifier)
        self.assertIn("has_reviewed_provisioning_chain", verifier)

    def test_script_offers_distinct_dev_sign_and_release_modes(self):
        script = (ROOT / "build-deck-app.sh").read_text(encoding="utf-8")
        # A distinct, explicit development-signing mode exists with its own
        # env vars; it must not overlap with release env vars or trigger
        # notarization, and neither mode may auto-discover a keychain
        # identity (no `security find-identity` / `find-identity` calls).
        self.assertIn("--dev-sign", script)
        self.assertIn("ARCEN_DEV_PROVISIONING_PROFILE", script)
        self.assertIn("ARCEN_DEV_CODESIGN_IDENTITY", script)
        self.assertIn(
            "--release and --dev-sign are mutually exclusive", script
        )
        self.assertNotIn("find-identity", script)
        self.assertIn(
            'run_signed_assembly "dev" "$DEV_PROFILE" "$DEV_SIGN_ID" '
            '"apple-development" "development"',
            script,
        )
        self.assertIn(
            'run_signed_assembly "release" "$PROFILE" "$SIGN_ID" '
            '"developer-id" "release"',
            script,
        )
        self.assertIn('--identity-class "$IDENTITY_CLASS"', script)
        # An explicit profile class ("release" rejects get-task-allow=true;
        # "development" requires it) must be threaded through both validator
        # invocations, not just the post-sign signature check.
        self.assertEqual(script.count('--profile-class "$PROFILE_CLASS"'), 2)
        # Notarization/staple/Gatekeeper only run for release, not dev-sign.
        mode_branch = script[script.index('if [ "$MODE" = "release" ]') :]
        self.assertIn("notarytool submit", mode_branch)
        self.assertIn("stapler staple", mode_branch)
        self.assertIn("spctl --assess", mode_branch)
        else_branch = mode_branch[mode_branch.index("else") :]
        self.assertIn("not notarized", else_branch)

    @unittest.skipUnless(sys.platform == "darwin", "requires macOS Security.framework")
    def test_native_verifier_compiles_and_distinguishes_invalid_cms(self):
        with tempfile.TemporaryDirectory(prefix="arcen-cms-test-") as temporary:
            verifier = pathlib.Path(temporary) / "arcen-cms-verifier"
            self.compile_cms_verifier(verifier)
            result = subprocess.run(
                [str(verifier), str(FIXTURES / "valid-profile.plist")],
                check=False,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            self.assertEqual(result.returncode, 3)

    @unittest.skipUnless(sys.platform == "darwin", "requires macOS Security.framework")
    def test_native_verifier_rejects_external_untrusted_cms(self):
        untrusted = os.environ.get("ARCEN_TEST_UNTRUSTED_PROVISIONING_PROFILE")
        if not untrusted:
            self.skipTest("external untrusted CMS fixture was not supplied")
        with tempfile.TemporaryDirectory(prefix="arcen-cms-test-") as temporary:
            verifier = pathlib.Path(temporary) / "arcen-cms-verifier"
            self.compile_cms_verifier(verifier)
            result = subprocess.run(
                [str(verifier), untrusted],
                check=False,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            self.assertEqual(
                result.returncode,
                4,
                "fixture must be a cryptographically valid CMS rejected at signer trust",
            )


if __name__ == "__main__":
    unittest.main()
