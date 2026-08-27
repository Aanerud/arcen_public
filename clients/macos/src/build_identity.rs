use arcen_protocol::messages::BuildIdentityMsg;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::sync::OnceLock;

fn artifact_sha256() -> Option<String> {
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

#[must_use]
pub fn current() -> BuildIdentityMsg {
    BuildIdentityMsg {
        product: "arcen-deck-macos".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        build_id: option_env!("ARCEN_BUILD_ID")
            .unwrap_or("development")
            .to_string(),
        source_revision: option_env!("ARCEN_SOURCE_REVISION")
            .unwrap_or("unknown")
            .to_string(),
        build_profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
        .to_string(),
        feature_profile: option_env!("ARCEN_FEATURE_PROFILE")
            .unwrap_or("quic-default")
            .to_string(),
        artifact_sha256: artifact_sha256(),
        signing_state: option_env!("ARCEN_SIGNING_STATE").map(str::to_string),
    }
}
