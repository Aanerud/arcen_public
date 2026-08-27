#!/usr/bin/env python3
"""Validate decoded, nonsensitive macOS release metadata."""

from __future__ import annotations

import argparse
import datetime as dt
import plistlib
import re
from pathlib import Path
from typing import Any


TEAM_ID = "NWR7ZH8L7U"
BUNDLE_ID = "deck.arcen.tech"
APPLICATION_ID = f"{TEAM_ID}.{BUNDLE_ID}"


class ValidationError(ValueError):
    pass


def load_plist(path: Path) -> dict[str, Any]:
    with path.open("rb") as stream:
        value = plistlib.load(stream)
    if not isinstance(value, dict):
        raise ValidationError(f"{path.name} must contain a plist dictionary")
    return value


def _is_superset(allowed: Any, requested: Any) -> bool:
    if isinstance(requested, list):
        candidates = allowed if isinstance(allowed, list) else [allowed]
        return all(
            any(
                candidate == value
                or (
                    isinstance(candidate, str)
                    and isinstance(value, str)
                    and candidate.endswith("*")
                    and value.startswith(candidate[:-1])
                )
                for candidate in candidates
            )
            for value in requested
        )
    if isinstance(requested, dict):
        return isinstance(allowed, dict) and all(
            key in allowed and _is_superset(allowed[key], value)
            for key, value in requested.items()
        )
    if requested is True:
        return allowed is True
    return allowed == requested


# Two distinct signing modes need two distinct, non-interchangeable
# provisioning-profile classes, independent of the certificate-authority check
# in validate_signature below. Apple provisioning profiles carry a
# `get-task-allow` entitlement that is `true` only for Development profiles
# (it permits a debugger to attach via task_for_pid) and must be absent or
# `false` for Developer ID / distribution profiles (Apple requires this for
# notarization eligibility). This is a defense-in-depth check on top of the
# CMS/team/app/entitlement-superset checks above, not a replacement for them.
_PROFILE_CLASSES = ("release", "development")


def validate_profile(
    profile: dict[str, Any],
    requested_entitlements: dict[str, Any],
    *,
    now: dt.datetime | None = None,
    profile_class: str = "release",
) -> None:
    if profile_class not in _PROFILE_CLASSES:
        raise ValidationError(f"unknown profile class {profile_class!r}")

    teams = profile.get("TeamIdentifier")
    if not isinstance(teams, list) or teams != [TEAM_ID]:
        raise ValidationError("provisioning profile team identifier does not match")

    expiry = profile.get("ExpirationDate")
    if not isinstance(expiry, dt.datetime):
        raise ValidationError("provisioning profile has no valid expiration date")
    current = now or dt.datetime.now(dt.timezone.utc)
    if expiry.tzinfo is None:
        expiry = expiry.replace(tzinfo=dt.timezone.utc)
    if expiry <= current:
        raise ValidationError("provisioning profile is expired")

    allowed = profile.get("Entitlements")
    if not isinstance(allowed, dict):
        raise ValidationError("provisioning profile has no entitlement dictionary")
    if allowed.get("com.apple.application-identifier") != APPLICATION_ID:
        raise ValidationError("provisioning profile application identifier does not match")
    if allowed.get("com.apple.developer.team-identifier") != TEAM_ID:
        raise ValidationError("provisioning profile entitlement team does not match")
    for key, requested in requested_entitlements.items():
        if key not in allowed or not _is_superset(allowed[key], requested):
            raise ValidationError(
                f"provisioning profile does not authorize requested entitlement {key}"
            )

    get_task_allow = allowed.get("get-task-allow", False)
    if profile_class == "release" and get_task_allow is True:
        raise ValidationError(
            "release provisioning profile has get-task-allow=true; this is a "
            "Development profile, not a Developer ID distribution profile"
        )
    if profile_class == "development" and get_task_allow is not True:
        raise ValidationError(
            "development provisioning profile is missing get-task-allow=true; "
            "this does not look like an Apple Development profile"
        )


# Two distinct signing modes produce two distinct, non-interchangeable
# certificate classes. "developer-id" is Release/Security's Developer ID
# Application chain (Developer ID Certification Authority -> Apple Root CA),
# used only for notarized release builds. "apple-development" is an Apple
# Development identity chain (Apple Worldwide Developer Relations
# Certification Authority -> Apple Root CA), used only for the unnotarized
# --dev-sign build. A release build must never be accepted as a development
# signature or vice versa.
_IDENTITY_CLASSES: dict[str, dict[str, str]] = {
    "developer-id": {
        "authority_prefix": "Developer ID Application:",
        "intermediate": "Authority=Developer ID Certification Authority",
    },
    "apple-development": {
        "authority_prefix": "Apple Development:",
        "intermediate": "Authority=Apple Worldwide Developer Relations Certification Authority",
    },
}


def validate_signature(metadata: str, *, identity_class: str = "developer-id") -> None:
    if identity_class not in _IDENTITY_CLASSES:
        raise ValidationError(f"unknown identity class {identity_class!r}")
    rules = _IDENTITY_CLASSES[identity_class]

    lines = {line.strip() for line in metadata.splitlines() if line.strip()}
    required = {
        f"Identifier={BUNDLE_ID}",
        f"TeamIdentifier={TEAM_ID}",
        rules["intermediate"],
        "Authority=Apple Root CA",
    }
    missing = sorted(required - lines)
    if missing:
        raise ValidationError(
            f"signed application metadata is missing {', '.join(missing)}"
        )
    authority_prefix = f"Authority={rules['authority_prefix']}"
    matching = [line for line in lines if line.startswith(authority_prefix)]
    if len(matching) != 1 or not re.fullmatch(
        rf"{re.escape(authority_prefix)} .+ \({TEAM_ID}\)",
        matching[0],
    ):
        raise ValidationError(
            f"signing certificate is not a {rules['authority_prefix'].rstrip(':')} identity for the expected team"
        )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--profile", required=True, type=Path)
    parser.add_argument("--entitlements", required=True, type=Path)
    parser.add_argument("--signature", type=Path)
    parser.add_argument(
        "--profile-class",
        choices=sorted(_PROFILE_CLASSES),
        default="release",
        help=(
            "expected provisioning-profile class: 'release' for --release "
            "builds (default; rejects get-task-allow=true) or 'development' "
            "for --dev-sign builds (requires get-task-allow=true)"
        ),
    )
    parser.add_argument(
        "--identity-class",
        choices=sorted(_IDENTITY_CLASSES),
        default="developer-id",
        help=(
            "expected signing certificate class: 'developer-id' for --release "
            "notarized builds (default) or 'apple-development' for --dev-sign "
            "unnotarized builds"
        ),
    )
    args = parser.parse_args()

    validate_profile(
        load_plist(args.profile),
        load_plist(args.entitlements),
        profile_class=args.profile_class,
    )
    if args.signature:
        validate_signature(
            args.signature.read_text(encoding="utf-8"),
            identity_class=args.identity_class,
        )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, plistlib.InvalidFileException, ValidationError) as error:
        raise SystemExit(f"release metadata validation failed: {error}") from None
