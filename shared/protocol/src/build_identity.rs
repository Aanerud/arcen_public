// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The build identity every Arcen product advertises in its hello.
//!
//! Release builds set `ARCEN_BUILD_ID`, `ARCEN_SOURCE_REVISION`,
//! `ARCEN_FEATURE_PROFILE` and `ARCEN_SIGNING_STATE` for the whole build, so
//! reading them here gives every product the same answer. Each product used
//! to carry its own copy of this function, and the macOS Pier had none, so a
//! Deck could not tell which build of it was serving.

use crate::messages::BuildIdentityMsg;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::sync::OnceLock;

/// The identity of the running build of `product`.
///
/// `version` is the caller's `CARGO_PKG_VERSION`. The artifact hash is that
/// of the executable actually running, computed once per process; it is
/// `None` when the executable cannot be read.
#[must_use]
pub fn this_build(product: &str, version: &str) -> BuildIdentityMsg {
    from_environment(
        product,
        version,
        BuildEnvironment {
            build_id: option_env!("ARCEN_BUILD_ID"),
            source_revision: option_env!("ARCEN_SOURCE_REVISION"),
            feature_profile: option_env!("ARCEN_FEATURE_PROFILE"),
            signing_state: option_env!("ARCEN_SIGNING_STATE"),
            debug: cfg!(debug_assertions),
        },
        running_executable_sha256(),
    )
}

/// What the build system stamped into this build.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuildEnvironment<'a> {
    pub build_id: Option<&'a str>,
    pub source_revision: Option<&'a str>,
    pub feature_profile: Option<&'a str>,
    pub signing_state: Option<&'a str>,
    pub debug: bool,
}

/// Builds the identity from explicit inputs, with the documented defaults for
/// anything the build did not stamp.
#[must_use]
pub fn from_environment(
    product: &str,
    version: &str,
    environment: BuildEnvironment<'_>,
    artifact_sha256: Option<String>,
) -> BuildIdentityMsg {
    BuildIdentityMsg {
        product: product.to_string(),
        version: version.to_string(),
        build_id: environment.build_id.unwrap_or("development").to_string(),
        source_revision: environment.source_revision.unwrap_or("unknown").to_string(),
        build_profile: if environment.debug {
            "debug"
        } else {
            "release"
        }
        .to_string(),
        feature_profile: environment
            .feature_profile
            .unwrap_or("quic-default")
            .to_string(),
        artifact_sha256,
        signing_state: environment.signing_state.map(str::to_string),
    }
}

/// Prefix of every build ID stamped by an official TUTTIFRUTTI AS build.
pub const OFFICIAL_BUILD_ID_PREFIX: &str = "official-";

/// Whether `build_id` has the shape of an official build ID:
/// `official-<YYYYMMDDTHHMMSSZ>-v<version>-<12 hex digits of the public
/// source commit>`, as `scripts/release/official-build-id.sh` stamps it.
///
/// The shape tells an official build apart from a development or community
/// build that did not imitate it; it is not proof. Proof is the code
/// signature of TUTTIFRUTTI AS on the artefact and its `artifact_sha256`
/// appearing in the official release's `SHA256SUMS`.
#[must_use]
pub fn has_official_build_id_shape(build_id: &str) -> bool {
    let Some(rest) = build_id.strip_prefix(OFFICIAL_BUILD_ID_PREFIX) else {
        return false;
    };
    let mut parts = rest.splitn(2, '-');
    let (Some(stamp), Some(rest)) = (parts.next(), parts.next()) else {
        return false;
    };
    let stamp_ok = stamp.len() == 16
        && stamp.as_bytes()[8] == b'T'
        && stamp.as_bytes()[15] == b'Z'
        && stamp[..8].bytes().all(|b| b.is_ascii_digit())
        && stamp[9..15].bytes().all(|b| b.is_ascii_digit());
    let Some((tag, revision)) = rest.rsplit_once('-') else {
        return false;
    };
    let tag_ok = tag.len() > 1
        && tag.starts_with('v')
        && tag[1..].bytes().next().is_some_and(|b| b.is_ascii_digit());
    let revision_ok = revision.len() == 12
        && revision
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    stamp_ok && tag_ok && revision_ok
}

fn running_executable_sha256() -> Option<String> {
    static HASH: OnceLock<Option<String>> = OnceLock::new();
    HASH.get_or_init(|| {
        let mut file = std::fs::File::open(std::env::current_exe().ok()?).ok()?;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer).ok()?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Some(format!("{:x}", hasher.finalize()))
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unstamped_build_says_so() {
        let identity = from_environment(
            "arcen-pier-macos",
            "0.13.0",
            BuildEnvironment::default(),
            None,
        );
        assert_eq!(identity.product, "arcen-pier-macos");
        assert_eq!(identity.build_id, "development");
        assert_eq!(identity.source_revision, "unknown");
        assert_eq!(identity.build_profile, "release");
        assert_eq!(identity.feature_profile, "quic-default");
        assert_eq!(identity.signing_state, None);
    }

    #[test]
    fn a_release_build_carries_what_was_stamped() {
        let identity = from_environment(
            "arcen-pier-linux",
            "0.13.0",
            BuildEnvironment {
                build_id: Some("20260928T0930Z-v0.13.0"),
                source_revision: Some("0123456789ab"),
                feature_profile: Some("quic-default"),
                signing_state: Some("unsigned"),
                debug: false,
            },
            Some("ab".repeat(32)),
        );
        assert_eq!(identity.build_id, "20260928T0930Z-v0.13.0");
        assert_eq!(identity.source_revision, "0123456789ab");
        assert_eq!(identity.signing_state.as_deref(), Some("unsigned"));
        assert_eq!(
            identity.artifact_sha256.as_deref(),
            Some("ab".repeat(32).as_str())
        );
    }

    #[test]
    fn official_build_ids_are_told_apart_by_their_shape() {
        assert!(has_official_build_id_shape(
            "official-20261109T120000Z-v0.19.0-0123456789ab"
        ));
        assert!(has_official_build_id_shape(
            "official-20261109T120000Z-v0.19.0-rc.1-0123456789ab"
        ));
        for other in [
            "development",
            "20261020T220304Z-0.17.0-0a1b2c3d",
            "official-20261109T1200Z-v0.19.0-0123456789ab",
            "official-20261109T120000Z-0.19.0-0123456789ab",
            "official-20261109T120000Z-v0.19.0-0123456789AB",
            "official-20261109T120000Z-v0.19.0-0123456789",
            "official-20261109T120000Z-0123456789ab",
        ] {
            assert!(!has_official_build_id_shape(other), "{other}");
        }
    }

    #[test]
    fn the_running_build_hashes_its_own_executable() {
        let identity = this_build("arcen-protocol-tests", env!("CARGO_PKG_VERSION"));
        let hash = identity
            .artifact_sha256
            .expect("test executable is readable");
        assert_eq!(hash.len(), 64);
        assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()));
    }
}
