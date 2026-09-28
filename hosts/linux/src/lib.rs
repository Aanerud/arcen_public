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
    arcen_protocol::build_identity::this_build("arcen-pier-linux", VERSION)
}

pub(crate) use eventlog::LifecycleEmitter;

/// The kernel's handle on this process's own executable. It stays valid after
/// an in-place upgrade unlinks the file the process started from, and exec'ing
/// it runs exactly that build.
const PROC_SELF_EXE: &str = "/proc/self/exe";

/// The binary helpers are spawned from: this process's own build.
///
/// After an in-place upgrade without a restart, the running service's
/// original file is gone (`current_exe` reads `... (deleted)`). Returning
/// `None` then made every new login fail as "session-launcher binary is
/// unavailable" until someone restarted the service, while the installer
/// said the previous build was still serving. `/proc/self/exe` keeps the
/// service and every helper it starts on one consistent build until the
/// restart that picks up the new one.
pub(crate) fn current_pier_exe() -> Option<std::path::PathBuf> {
    running_pier_exe(
        std::env::current_exe().ok(),
        std::path::Path::new(PROC_SELF_EXE).is_file(),
    )
}

fn running_pier_exe(
    current: Option<std::path::PathBuf>,
    proc_self_exe_is_file: bool,
) -> Option<std::path::PathBuf> {
    match current {
        Some(path) if path.is_file() => Some(path),
        _ if proc_self_exe_is_file => Some(std::path::PathBuf::from(PROC_SELF_EXE)),
        _ => None,
    }
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
    binary == std::path::Path::new(PROC_SELF_EXE)
        || std::env::current_exe()
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

#[cfg(test)]
mod running_exe_tests {
    use super::{is_current_pier_exe, running_pier_exe, PROC_SELF_EXE};
    use std::path::{Path, PathBuf};

    #[test]
    fn an_upgraded_service_keeps_spawning_its_own_build() {
        let replaced = PathBuf::from("/opt/arcen/bin/arcen-pier (deleted)");
        assert_eq!(
            running_pier_exe(Some(replaced.clone()), true),
            Some(PathBuf::from(PROC_SELF_EXE))
        );
        assert_eq!(running_pier_exe(Some(replaced), false), None);
        assert_eq!(running_pier_exe(None, false), None);
        let present = std::env::current_exe().expect("test binary");
        assert_eq!(running_pier_exe(Some(present.clone()), true), Some(present));
    }

    #[test]
    fn the_kernel_handle_is_the_pier_itself() {
        assert!(is_current_pier_exe(Path::new(PROC_SELF_EXE)));
        assert!(!is_current_pier_exe(Path::new("/usr/bin/true")));
    }
}
