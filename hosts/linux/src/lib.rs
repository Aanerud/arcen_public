//! Arcen native Linux host — Rust control plane.
//!
//! This crate is the staged replacement for the Python `server/*.py` on the
//! **Linux host path** (see the session plan). It is being built side-by-side
//! with the working Python host: it comes up on a non-conflicting port during
//! bring-up and only takes over the default port at cutover, so the Mac client
//! can connect end-to-end (both chroma modes) at every stage.
//!
//! Stage 0 laid the logging foundation and runnable skeleton. Stage 1 adds the
//! CLI config, the TLS
//! WebSocket server, `arcen-capenc` supervision + Annex-B framing, and the
//! byte-compatible frame relay with drop-oldest backpressure. Auth, resolution
//! ingest, native display control, and input arrive in later stages.

#![allow(dead_code)]

pub mod bounded_io;
pub mod cli;
pub mod clipboard;
pub mod config;
#[cfg(target_os = "linux")]
pub mod cursor_watcher;
pub mod deskside;
pub mod display;
pub mod eventlog;
pub mod host_cert;
pub mod input;
pub mod logging;
pub mod media;
pub mod microphone_input;
pub mod net;
pub mod netinfo;
pub mod observability;
pub mod session;
pub mod session_admission;
pub mod support_bundle;
#[cfg(target_os = "linux")]
pub mod usb_bridge;

/// Crate/agent version, surfaced in `--version` and the startup banner.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Canonical location of the corresponding source.
pub const SOURCE_URL: &str = "https://github.com/Aanerud/arcen_public";

/// AGPL-3.0 section 13 source offer.
///
/// Arcen is remote-access software, so users routinely interact with a Pier
/// **over a network** rather than by running it themselves. Section 13 requires
/// that those users be offered the corresponding source, so the offer is
/// surfaced by the program itself (`--version` and the startup banner) rather
/// than living only in a file in the repository. An operator running a modified
/// Pier inherits that obligation; keeping the notice in the binary is what makes
/// it reachable.
pub const SOURCE_OFFER: &str =
    "Arcen is free software under the GNU AGPL-3.0. It comes with ABSOLUTELY NO WARRANTY. \
     You may redistribute it under the terms of that licence. If you run a modified version \
     that others connect to over a network, you must offer them its corresponding source.";

pub(crate) fn build_identity() -> arcen_protocol::messages::BuildIdentityMsg {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    use std::sync::OnceLock;

    static ARTIFACT_HASH: OnceLock<Option<String>> = OnceLock::new();
    let artifact_sha256 = ARTIFACT_HASH
        .get_or_init(|| {
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
        .clone();
    arcen_protocol::messages::BuildIdentityMsg {
        product: "arcen-pier-linux".to_string(),
        version: VERSION.to_string(),
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
        artifact_sha256,
        signing_state: option_env!("ARCEN_SIGNING_STATE").map(str::to_string),
    }
}

pub(crate) use eventlog::LifecycleEmitter;

pub(crate) fn current_pier_exe() -> Option<std::path::PathBuf> {
    std::env::current_exe().ok().filter(|path| path.is_file())
}

pub(crate) fn command_for_helper(
    binary: &std::path::Path,
    subcommand: &'static str,
) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(binary);
    if is_current_pier_exe(binary) {
        command.arg(subcommand);
    }
    command
}

fn is_current_pier_exe(binary: &std::path::Path) -> bool {
    std::env::current_exe()
        .ok()
        .is_some_and(|current| paths_same_file(&current, binary))
}

fn paths_same_file(left: &std::path::Path, right: &std::path::Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// Validates and emits one lifecycle event.
///
/// This never returns an error and never affects the caller's own outcome:
/// an unexpected schema-validation failure (which should not happen for the
/// field sets built by this crate) is logged once and native delivery is
/// skipped for that event; a native sink failure is handled the same way
/// inside [`LifecycleEmitter::emit`].
pub(crate) fn emit_lifecycle_event(
    emitter: &LifecycleEmitter,
    kind: arcen_telemetry::LifecycleEventKind,
    correlation_id: arcen_telemetry::CorrelationId,
    fields: arcen_telemetry::StructuredFields,
) {
    match arcen_telemetry::ValidatedLifecycleEvent::new(kind, correlation_id, fields) {
        Ok(event) => emitter.emit(&event),
        Err(error) => tracing::debug!(
            target: logging::target::HEALTH,
            %error,
            event_id = kind.id(),
            "lifecycle event schema validation failed; native delivery skipped"
        ),
    }
}

/// Validates and emits one lifecycle event with an explicit top-level
/// [`arcen_observability::LifecycleContext`] (real authenticated `user`
/// and/or `peer_addr` for session/auth events), instead of `emit_lifecycle_event`'s
/// always-`None` identity default. `context.sid` must match `correlation_id`;
/// callers build both from the same session log id.
///
/// This never returns an error and never affects the caller's own outcome,
/// matching [`emit_lifecycle_event`]: a schema-validation failure is logged
/// once and native delivery is skipped for that event; a native sink
/// failure is handled the same way inside [`LifecycleEmitter::emit_context`].
pub(crate) fn emit_lifecycle_event_with_context(
    emitter: &LifecycleEmitter,
    kind: arcen_telemetry::LifecycleEventKind,
    context: arcen_observability::LifecycleContext,
    fields: arcen_telemetry::StructuredFields,
) {
    match arcen_telemetry::ValidatedLifecycleEvent::new(kind, context.sid.clone(), fields) {
        Ok(event) => emitter.emit_context(&event, context),
        Err(error) => tracing::debug!(
            target: logging::target::HEALTH,
            %error,
            event_id = kind.id(),
            "lifecycle event schema validation failed; native delivery skipped"
        ),
    }
}
