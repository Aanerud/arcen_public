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
                source_revision: Some("bf81787c05a0"),
                feature_profile: Some("quic-default"),
                signing_state: Some("unsigned"),
                debug: false,
            },
            Some("ab".repeat(32)),
        );
        assert_eq!(identity.build_id, "20260928T0930Z-v0.13.0");
        assert_eq!(identity.source_revision, "bf81787c05a0");
        assert_eq!(identity.signing_state.as_deref(), Some("unsigned"));
        assert_eq!(
            identity.artifact_sha256.as_deref(),
            Some("ab".repeat(32).as_str())
        );
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
