// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Deck's support bundle: one `.zip` a user can hand to whoever helps
//! them, and that we read ourselves, built the same way on every Deck.
//!
//! The shell gathers what only the operating system knows
//! ([`SupportFacts::collect`] through [`crate::DeckServices`], plus the
//! display list it reads on its UI thread) and calls [`create`] off the UI
//! thread. Everything else — which files, how they are bounded, how they
//! are redacted, the manifest — is here.
//!
//! The archive is written by the collector every Pier and Deck shares
//! (`arcen_observability::support_bundle`) and follows the schema-v1
//! contract (`arcen_telemetry::support_bundle`, `docs/operations/support-bundles.md`):
//! `manifest.json` hashes every entry and lists every omission and
//! redaction. What goes in:
//!
//! | Entry | Source |
//! | --- | --- |
//! | `deck/about.json` | build identity, OS, log level, folders (home as `~`) |
//! | `logs/arcen-client.log.<date>` | the newest [`LOG_FILES_KEPT`] daily logs, canonical records only, identities pseudonymised |
//! | `config/{settings,connections,trusted_pins}.json` | redacted ([`redact`]); passwords are never saved, usernames pseudonymised, pin fingerprints kept |
//! | `diagnostics/{displays,decode,network,platform,native-usb-component}.json` | displays and colour facts (no EDID or serial), the decoder probe, the path to the last host, GPU/adapters and platform facts, the Native tablet component |
//! | `session/last-session.json` | the last session's Stream Statistics, Negotiated Truth, health and telemetry ([`snapshot`]) |
//! | `crash/*` | the newest [`CRASH_REPORTS_KEPT`] Deck crash reports (Windows minidumps, macOS `.ips`), account name removed |
//!
//! Never included: passwords, private keys, clipboard contents, typed text.

pub mod redact;
pub mod snapshot;
#[cfg(feature = "support-bundle")]
mod writer;

use std::path::PathBuf;

#[cfg(feature = "support-bundle")]
pub use writer::{
    BundleOutcome, CRASH_REPORTS_KEPT, LOG_FILES_KEPT, SealedBundleOutcome, SupportBundleError,
    SupportBundleRequest, bundle_target, create, create_sealed, default_folder,
    last_selected_target, reference_manifest, run, run_command, saved_log_level, summary_details,
    write_bundle,
};

use crate::decode_report::DecodeReport;
use crate::network::{NetworkProbeResult, OutgoingInterface};
use snapshot::SessionSnapshot;

/// What kind of crash report a platform found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashReportKind {
    /// A Windows Error Reporting minidump (`.dmp`, binary).
    WindowsMinidump,
    /// A macOS diagnostic report (`.ips`, JSON text).
    MacDiagnosticReport,
}

/// One crash report the platform found for this Deck.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashReportSource {
    /// Where it is.
    pub path: PathBuf,
    /// What it is.
    pub kind: CrashReportKind,
}

/// What only the platform can say, for [`crate::DeckServices::support_details`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SupportDetails {
    /// Platform facts as JSON: GPUs/adapters (name, vendor, driver), OS
    /// build, crash-report registration, the USB component's service.
    /// Redacted like every other document.
    pub platform: serde_json::Value,
    /// This Deck's crash reports, any order; the bundle keeps the newest.
    pub crash_reports: Vec<CrashReportSource>,
}

/// The path to the host the Deck last connected to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkFacts {
    /// `host:port` of the last session's host, else of the last selected
    /// connection ([`bundle_target`]).
    pub target: String,
    /// Interface kind, scope, link speed, MTU (and Wi-Fi name, which the
    /// bundle pseudonymises).
    pub probe: NetworkProbeResult,
}

/// Everything the bundle reports that is not a file.
#[derive(Debug, Clone, Default)]
pub struct SupportFacts {
    /// This build.
    pub build: Option<arcen_protocol::messages::BuildIdentityMsg>,
    /// `macOS 26.1 arm64`, `Windows 11 26100 x64`.
    pub platform_summary: String,
    /// The displays and their colour facts (read on the UI thread).
    pub displays: Vec<arcen_protocol::messages::ClientMonitor>,
    /// The decoder probe.
    pub decode: Option<DecodeReport>,
    /// The path to the last host, when there is one.
    pub network: Option<NetworkFacts>,
    /// The outgoing interface toward the last host.
    pub outgoing: Option<OutgoingInterface>,
    /// The Native tablet component, `None` without one.
    pub usb_component: Option<arcen_usb_bridge::NativeUsbComponentState>,
    /// Platform facts and crash reports.
    pub details: SupportDetails,
    /// The last session's statistics.
    pub session: Option<SessionSnapshot>,
}

impl SupportFacts {
    /// Asks `services` for everything but the displays (pass those from the
    /// UI thread). Slow calls (the decoder probe, a network probe of the
    /// last host, the USB component's state) run here, so call it off the
    /// UI thread.
    #[must_use]
    pub fn collect(
        services: &dyn crate::DeckServices,
        displays: Vec<arcen_protocol::messages::ClientMonitor>,
        last_target: Option<(&str, u16)>,
    ) -> Self {
        let network = last_target.map(|(host, port)| NetworkFacts {
            target: format!("{host}:{port}"),
            probe: services.probe_network(host, port),
        });
        let outgoing = last_target.and_then(|(host, port)| {
            use std::net::ToSocketAddrs;
            (host, port)
                .to_socket_addrs()
                .ok()
                .and_then(|mut addresses| addresses.next())
                .map(|address| services.outgoing_interface(address.ip()))
        });
        Self {
            build: Some(services.build_identity()),
            platform_summary: services.platform_summary(),
            displays,
            decode: Some(services.decode_report()),
            network,
            outgoing,
            usb_component: services.native_usb_component_state(),
            details: services.support_details(),
            session: snapshot::last_session_snapshot(),
        }
    }
}
