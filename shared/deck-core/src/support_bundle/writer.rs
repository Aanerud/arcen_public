// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Writes the bundle: bounded sources, redaction, schema-v1 manifest, and
//! an atomic `.zip` in the chosen folder.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub use arcen_observability::support_bundle::SupportBundleError;
use arcen_observability::support_bundle::{
    self as bundle, BundleLimits, BundleSealer, Collector, OutputOption, PublicSummary,
    SummaryDetails, SummaryFact, open_source,
};
use arcen_telemetry::{
    BundleComponent, BundlePath, BundlePseudonymKey, BundlePseudonymizer, BundleSource,
    BundleTruncation, CanonicalJsonlTransformLimits, NoticeCode, NoticeKind, RedactionReason,
    RedactionRecord, SupportBundleManifest, TruncationReason, transform_canonical_jsonl,
};

use super::redact::{LocalIdentity, Scrubber, redact_deck_json};
use super::{CrashReportKind, SupportFacts};
use crate::directories::DeckDirectories;
use crate::settings::LogLevel;

/// Daily log files kept: today's and the six before it.
pub const LOG_FILES_KEPT: usize = 7;
/// Crash reports kept, newest first.
pub const CRASH_REPORTS_KEPT: usize = 5;
/// Every Deck's limits: the tail of one daily log, all logs together, one
/// configuration file and the whole payload ([`BundleLimits::DECK`]).
const LIMITS: BundleLimits = BundleLimits::DECK;
/// One crash report.
const MAX_CRASH_REPORT_BYTES: u64 = 16 * 1024 * 1024;
/// The Deck's daily log files (`arcen_deck_core::logging`).
const LOG_FILE_PREFIX: &str = "arcen-client.log";
/// The configuration files a Deck keeps (`crate::directories`).
const CONFIG_FILES: [&str; 3] = ["settings.json", "connections.json", "trusted_pins.json"];

/// What [`create`] needs.
#[derive(Debug)]
pub struct SupportBundleRequest {
    /// Where the `.zip` goes (created if missing; its permissions are left
    /// alone).
    pub output_dir: PathBuf,
    /// `arcen-deck-macos` / `arcen-deck-windows`, version, OS, arch.
    pub component: BundleComponent,
    /// The config and log folders this Deck uses.
    pub directories: DeckDirectories,
    /// The log level the Deck runs at now.
    pub log_level: LogLevel,
    /// Everything that is not a file.
    pub facts: SupportFacts,
    /// This computer's account, removed from the bundle.
    pub identity: LocalIdentity,
    /// The bundle's time (archive name, manifest, ZIP entry times).
    pub generated_at: SystemTime,
    /// The per-bundle pseudonym key ([`Self::fresh_key`]).
    pub pseudonym_key: BundlePseudonymKey,
    /// Include the Deck's crash reports. Off unless the user asks (the
    /// panel's checkbox, `--include-crash-dumps`): a crash report's stack can
    /// hold what the Deck was handling, a sign-in password included, which
    /// no redaction can find.
    pub include_crash_reports: bool,
}

impl SupportBundleRequest {
    /// A fresh random pseudonym key.
    ///
    /// # Errors
    ///
    /// The system has no entropy source.
    pub fn fresh_key() -> Result<BundlePseudonymKey, getrandom::Error> {
        let mut key = BundlePseudonymKey::zeroed();
        getrandom::getrandom(key.entropy_buffer())?;
        Ok(key)
    }
}

/// A written bundle.
#[derive(Debug, Clone)]
pub struct BundleOutcome {
    /// The `.zip`.
    pub path: PathBuf,
    /// What it holds.
    pub manifest: SupportBundleManifest,
    /// What its public summary states ([`summary_details`]).
    pub summary_details: SummaryDetails,
}

impl BundleOutcome {
    /// The bundle's public summary, which a user may paste into a public
    /// issue: the manifest's counts and codes, the last session's numbers,
    /// and the archive's SHA-256, never a log line, setting, path or name.
    ///
    /// # Errors
    ///
    /// The archive could not be read to fingerprint it.
    pub fn public_summary(&self) -> Result<PublicSummary, SupportBundleError> {
        let archive = bundle::fingerprint_file(&self.path)?;
        Ok(PublicSummary::new(
            &self.manifest,
            Some(archive),
            &self.summary_details,
        ))
    }
}

/// A written bundle that was sealed to a recipient's key ([`create_sealed`]).
#[derive(Debug, Clone)]
pub struct SealedBundleOutcome {
    /// The sealed file (`arcen-deck-support-<UTC time>.zip.<extension>`).
    pub path: PathBuf,
    /// What the sealed archive holds.
    pub manifest: SupportBundleManifest,
    /// Its public summary, with the plaintext archive's SHA-256.
    pub summary: PublicSummary,
}

/// What a Deck may state in public ([`PublicSummary`]): the release
/// artefact's SHA-256 (never the build ID or source revision), the OS, the
/// GPU and decoder the probe found and what it proved, the last session's
/// preset, counters and worst health, and whether crash reports are in.
/// Session telemetry lines are `name=value`; only the names below are read,
/// and only as integers or health states. The preset is read only when the
/// Negotiated Truth line starts with a preset's own name.
#[must_use]
pub fn summary_details(facts: &SupportFacts, include_crash_reports: bool) -> SummaryDetails {
    const COUNTS: [&str; 8] = [
        "session_seconds",
        "frames_received",
        "frames_decoded",
        "frames_presented",
        "frames_dropped",
        "average_fps",
        "average_rtt_ms",
        "reconnects",
    ];
    const PRESETS: [&str; 4] = ["Auto", "Speed", "Grading", "HDR"];
    let mut summary = vec![
        SummaryFact::count("displays", facts.displays.len() as u64),
        SummaryFact::count(
            "crash_reports_found",
            facts.details.crash_reports.len() as u64,
        ),
        SummaryFact::flag("crash_reports_included", include_crash_reports),
        SummaryFact::flag("last_session_recorded", facts.session.is_some()),
    ];
    let mut preset = None;
    if let Some(session) = facts.session.as_ref() {
        for line in &session.telemetry {
            let Some((name, value)) = line.split_once('=') else {
                continue;
            };
            if let Some(key) = COUNTS.iter().find(|key| **key == name) {
                if let Ok(value) = value.parse::<u64>() {
                    summary.push(SummaryFact::count(key, value));
                }
            } else if name == "worst_health" {
                let state = match value {
                    "Ok" => "ok",
                    "Degraded" => "degraded",
                    "Critical" => "critical",
                    "none" => "none",
                    _ => continue,
                };
                summary.push(SummaryFact::token("worst_health", state));
            }
        }
        summary.push(SummaryFact::flag(
            "decoder_refused_stream",
            session.decoder_refusal.is_some(),
        ));
        preset = session.negotiated_truth.first().and_then(|line| {
            PRESETS
                .iter()
                .find(|preset| {
                    line.strip_prefix(**preset)
                        .is_some_and(|rest| rest.starts_with(':') || rest.starts_with(' '))
                })
                .copied()
        });
    }
    let decode = facts.decode.as_ref();
    SummaryDetails {
        artifact_sha256: facts
            .build
            .as_ref()
            .and_then(|build| build.artifact_sha256.clone()),
        os: Some(facts.platform_summary.clone()).filter(|os| !os.is_empty()),
        gpus: decode
            .map(|decode| vec![decode.decoder.clone()])
            .unwrap_or_default(),
        decoder: decode.map(|decode| format!("{:?}", decode.capabilities.backend)),
        decode_profiles: decode
            .map(|decode| {
                let proved = &decode.capabilities;
                vec![
                    ("h264", proved.h264),
                    ("h265", proved.h265),
                    ("av1", proved.av1),
                    ("main10", proved.main10),
                    ("yuv444", proved.yuv444),
                ]
            })
            .unwrap_or_default(),
        preset,
        codes: Vec::new(),
        facts: summary,
    }
}

/// Gathers the facts through `services` and writes a bundle into
/// `output_dir`: what Settings › Logs › "Create support bundle…" and the
/// `support-bundle` command run. Slow (decoder probe, network probe of the
/// last host, file reads); call it off the UI thread with the displays the
/// UI thread read.
///
/// # Errors
///
/// No entropy for the pseudonym key, or the bundle could not be written.
pub fn run(
    services: &dyn crate::DeckServices,
    displays: Vec<arcen_protocol::messages::ClientMonitor>,
    component: BundleComponent,
    log_level: LogLevel,
    output_dir: PathBuf,
    include_crash_reports: bool,
) -> Result<BundleOutcome, SupportBundleError> {
    let directories = services.directories();
    let last_target = bundle_target(
        super::snapshot::last_session_target(),
        directories.config.as_deref(),
    );
    let facts = SupportFacts::collect(
        services,
        displays,
        last_target
            .as_ref()
            .map(|(host, port)| (host.as_str(), *port)),
    );
    let pseudonym_key = SupportBundleRequest::fresh_key()
        .map_err(|error| SupportBundleError::Output(format!("no random source: {error}")))?;
    let outcome = create(SupportBundleRequest {
        output_dir,
        component,
        directories,
        log_level,
        facts,
        identity: LocalIdentity::from_env(),
        generated_at: SystemTime::now(),
        pseudonym_key,
        include_crash_reports,
    })?;
    tracing::info!(
        target: crate::log_target::UI,
        bundle = %outcome
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        entries = outcome.manifest.entries.len(),
        notices = outcome.manifest.notices.len(),
        "support bundle created"
    );
    Ok(outcome)
}

/// `support-bundle [--out DIR]` on either Deck: the same bundle as
/// Settings › Logs › "Create support bundle…", without the GUI, at the
/// saved log level. Prints the bundle's path; returns the exit code.
#[must_use]
pub fn run_command(
    arguments: &[String],
    services: &dyn crate::DeckServices,
    displays: Vec<arcen_protocol::messages::ClientMonitor>,
    component: BundleComponent,
) -> i32 {
    let options = match bundle::parse_output_option(arguments) {
        Ok(OutputOption::Write(options)) => options,
        Ok(OutputOption::Help) => {
            println!("usage: arcen-deck support-bundle {}", bundle::USAGE_OPTIONS);
            return 0;
        }
        Err(error) => {
            eprintln!("{error}");
            return 2;
        }
    };
    let log_level = services
        .directories()
        .config
        .as_deref()
        .and_then(saved_log_level)
        .unwrap_or_else(|| crate::settings::ClientSettings::default().log_level);
    let Some(output) = options
        .output_directory
        .or_else(|| default_folder(services))
    else {
        eprintln!("no folder to save in: pass --out DIR");
        return 2;
    };
    match run(
        services,
        displays,
        component,
        log_level,
        output,
        options.include_crash_dumps,
    ) {
        Ok(outcome) => {
            println!(
                "support bundle: {} ({} entries, {} notices, {} redactions)",
                outcome.path.display(),
                outcome.manifest.entries.len(),
                outcome.manifest.notices.len(),
                outcome.manifest.redactions.len()
            );
            0
        }
        Err(error) => {
            eprintln!("support bundle failed: {error}");
            1
        }
    }
}

/// The folder a bundle goes to unless the user picks one: the platform's
/// ([`crate::DeckServices::default_support_folder`], the Desktop), else the
/// log folder.
#[must_use]
pub fn default_folder(services: &dyn crate::DeckServices) -> Option<PathBuf> {
    services
        .default_support_folder()
        .or_else(|| services.directories().logs)
}

/// Writes the bundle into `request.output_dir` as
/// `arcen-deck-support-<UTC time>.zip` (never a hostname), through a
/// same-folder partial file that is synced and renamed only when complete.
///
/// # Errors
///
/// The folder or archive could not be written.
pub fn create(request: SupportBundleRequest) -> Result<BundleOutcome, SupportBundleError> {
    std::fs::create_dir_all(&request.output_dir).map_err(|error| {
        SupportBundleError::Output(format!(
            "could not create {}: {error}",
            request.output_dir.display()
        ))
    })?;
    let stamp = bundle::utc_stamp(unix_seconds(request.generated_at));
    let output_dir = request.output_dir.clone();
    let summary_details = summary_details(&request.facts, request.include_crash_reports);
    let (path, manifest) = bundle::write_archive(
        &output_dir,
        |suffix| format!("arcen-deck-support-{stamp}{suffix}.zip"),
        |file| write_bundle(file, request),
    )?;
    Ok(BundleOutcome {
        path,
        manifest,
        summary_details,
    })
}

/// [`create`], sealed: the same bundle, published only as
/// `arcen-deck-support-<UTC time>.zip.<extension>` after `sealer` encrypted
/// it (the plaintext archive exists only as a private partial file while it
/// is written), with its public summary.
///
/// # Errors
///
/// The folder, archive or sealed file could not be written.
pub fn create_sealed(
    request: SupportBundleRequest,
    sealer: &dyn BundleSealer,
) -> Result<SealedBundleOutcome, SupportBundleError> {
    std::fs::create_dir_all(&request.output_dir).map_err(|error| {
        SupportBundleError::Output(format!(
            "could not create {}: {error}",
            request.output_dir.display()
        ))
    })?;
    let stamp = bundle::utc_stamp(unix_seconds(request.generated_at));
    let output_dir = request.output_dir.clone();
    let summary_details = summary_details(&request.facts, request.include_crash_reports);
    let published = bundle::write_sealed_archive(
        &output_dir,
        |suffix| format!("arcen-deck-support-{stamp}{suffix}.zip"),
        sealer,
        |file| write_bundle(file, request),
    )?;
    let summary = PublicSummary::new(&published.value, Some(published.archive), &summary_details);
    Ok(SealedBundleOutcome {
        path: published.path,
        manifest: published.value,
        summary,
    })
}

fn unix_seconds(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Writes the archive into `writer` and returns it with the manifest. The
/// manifest depends only on the request (fixed time and key give a fixed
/// manifest), which the golden test relies on.
///
/// # Errors
///
/// Writing failed or a schema bound was exceeded.
pub fn write_bundle<W: Write + Seek>(
    writer: W,
    request: SupportBundleRequest,
) -> Result<(W, SupportBundleManifest), SupportBundleError> {
    let generated = unix_seconds(request.generated_at);
    let mut bundle = Bundle {
        collector: Collector::new(
            writer,
            request.component.clone(),
            generated,
            BundlePseudonymizer::new(request.pseudonym_key),
            LIMITS,
        ),
        scrubber: Scrubber::new(&request.identity),
    };
    bundle.about(
        &request.component,
        &request.facts,
        &request.directories,
        request.log_level,
        generated,
    )?;
    bundle.logs(request.directories.logs.as_deref())?;
    bundle.config(request.directories.config.as_deref())?;
    bundle.diagnostics(&request.facts)?;
    bundle
        .collector
        .crash_report_choice(request.include_crash_reports, true)?;
    if request.include_crash_reports {
        bundle.crash_reports(&request.facts)?;
    }
    for excluded in [
        "runtime/clipboard",
        "runtime/credentials",
        "runtime/typed-text",
    ] {
        bundle.notice(
            excluded,
            NoticeKind::Omitted,
            NoticeCode::SensitiveRuntimeExcluded,
        )?;
    }
    let (writer, manifest, _) = bundle.collector.finish()?;
    Ok((writer, manifest))
}

/// The manifest of a Deck's bundle made from nothing: no folders, no
/// facts, a fixed time and key, `component` as the product names itself,
/// with its version and architecture (and `deck/about.json`, which carries
/// them) normalised. Each Deck pins this in its own golden test, so a change
/// to what its bundle reports or omits shows up as a reviewed diff.
///
/// # Errors
///
/// The bundle could not be written.
pub fn reference_manifest(component: BundleComponent) -> Result<String, SupportBundleError> {
    let (_, manifest) = write_bundle(
        std::io::Cursor::new(Vec::new()),
        SupportBundleRequest {
            output_dir: PathBuf::new(),
            component,
            directories: DeckDirectories {
                config: None,
                logs: None,
            },
            log_level: LogLevel::from_index(0),
            facts: SupportFacts::default(),
            identity: LocalIdentity::default(),
            generated_at: UNIX_EPOCH + std::time::Duration::from_secs(1_794_726_245),
            pseudonym_key: BundlePseudonymKey::from_bytes([0x11; 32]),
            include_crash_reports: false,
        },
    )?;
    let mut value = serde_json::to_value(&manifest)
        .map_err(|error| SupportBundleError::Archive(error.to_string()))?;
    value["component"]["version"] = "<version>".into();
    value["component"]["arch"] = "<arch>".into();
    if let Some(entries) = value["entries"].as_array_mut() {
        for entry in entries {
            if entry["path"] == "deck/about.json" {
                entry["sha256"] = "<version-dependent>".into();
                entry["included_size_bytes"] = 0.into();
                entry["original_size_bytes"] = 0.into();
            }
        }
    }
    let mut text = serde_json::to_string_pretty(&value)
        .map_err(|error| SupportBundleError::Archive(error.to_string()))?;
    text.push('\n');
    Ok(text)
}

/// The shared collector plus what only a Deck removes: this computer's
/// account name and home folder.
struct Bundle<W: Write + Seek> {
    collector: Collector<W>,
    scrubber: Scrubber,
}

impl<W: Write + Seek> Bundle<W> {
    fn notice(
        &mut self,
        source: &str,
        kind: NoticeKind,
        code: NoticeCode,
    ) -> Result<(), SupportBundleError> {
        self.collector.notice(source, kind, code)
    }

    /// Adds one entry, or an omission notice when the total cap would be
    /// passed.
    fn add(
        &mut self,
        path: &str,
        source: BundleSource,
        original_size: u64,
        bytes: &[u8],
        truncation: Option<BundleTruncation>,
    ) -> Result<bool, SupportBundleError> {
        self.collector
            .add_entry(path, source, original_size, bytes, truncation)
    }

    /// Redacts `document`, then adds it pretty-printed.
    fn add_json(
        &mut self,
        path: &str,
        source: BundleSource,
        original_size: Option<u64>,
        mut document: serde_json::Value,
    ) -> Result<(), SupportBundleError> {
        let bundle_path = BundlePath::new(path)?;
        let records = redact_deck_json(
            &bundle_path,
            &mut document,
            self.collector.pseudonymizer(),
            &self.scrubber,
        )?;
        let mut bytes = serde_json::to_vec_pretty(&document)
            .map_err(|error| SupportBundleError::Archive(error.to_string()))?;
        bytes.push(b'\n');
        self.collector
            .add_document(path, source, original_size, &bytes, records)?;
        Ok(())
    }

    fn about(
        &mut self,
        component: &BundleComponent,
        facts: &SupportFacts,
        directories: &DeckDirectories,
        log_level: LogLevel,
        generated: u64,
    ) -> Result<(), SupportBundleError> {
        let folder = |path: Option<&Path>| {
            path.map(|path| {
                self.scrubber
                    .scrub_text(&path.display().to_string())
                    .into_owned()
            })
        };
        let document = serde_json::json!({
            "component": component.name,
            "version": component.version,
            "os": component.os,
            "arch": component.arch,
            "platform": facts.platform_summary,
            "build": facts.build.as_ref().map(serde_json::to_value).transpose()
                .map_err(|error| SupportBundleError::Archive(error.to_string()))?,
            "log_level": { "level": log_level.index(), "name": log_level.label() },
            "folders": {
                "config": folder(directories.config.as_deref()),
                "logs": folder(directories.logs.as_deref()),
                "config_overridden": std::env::var_os(crate::directories::CONFIG_DIR_ENV).is_some(),
                "logs_overridden": std::env::var_os(crate::directories::LOG_DIR_ENV).is_some(),
            },
            "generated_at": bundle::rfc3339(generated),
            "privacy": "Passwords, private keys, clipboard contents and typed text are never \
                        included. Usernames, network names, host names and addresses are \
                        per-bundle pseudonyms; this computer's home folder and account name are \
                        removed. Crash reports only when asked for.",
        });
        self.add_json("deck/about.json", BundleSource::Diagnostics, None, document)
    }

    fn logs(&mut self, directory: Option<&Path>) -> Result<(), SupportBundleError> {
        const SOURCE: &str = "logs";
        let Some(directory) = directory else {
            return self.notice(SOURCE, NoticeKind::Unavailable, NoticeCode::SourceNotFound);
        };
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) => {
                let (kind, code) = bundle::io_notice(&error);
                return self.notice(SOURCE, kind, code);
            }
        };
        let mut names: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| is_log_file_name(name))
            .collect();
        // Newest first: dated names sort by date, the undated legacy name last.
        names.sort_by(|left, right| right.cmp(left));
        if names.len() > LOG_FILES_KEPT {
            self.notice(SOURCE, NoticeKind::Advisory, NoticeCode::LogCandidateLimit)?;
            names.truncate(LOG_FILES_KEPT);
        }
        if names.is_empty() {
            return self.notice(SOURCE, NoticeKind::Unavailable, NoticeCode::SourceNotFound);
        }
        let mut budget = LIMITS.max_log_bytes;
        for name in names {
            self.log_file(directory, &name, &mut budget)?;
        }
        Ok(())
    }

    /// One daily log: its tail, canonical records only, identities
    /// pseudonymised, the account name removed.
    fn log_file(
        &mut self,
        directory: &Path,
        name: &str,
        budget: &mut u64,
    ) -> Result<(), SupportBundleError> {
        let entry = format!("logs/{name}");
        if *budget == 0 {
            return self.notice(&entry, NoticeKind::Truncated, NoticeCode::LogPayloadLimit);
        }
        let (mut file, size) = match open_regular(&directory.join(name)) {
            Ok(opened) => opened,
            Err((kind, code)) => return self.notice(&entry, kind, code),
        };
        // The newest records matter most: read the tail of a long day.
        let input_limit = LIMITS
            .max_log_file_input_bytes
            .unwrap_or(LIMITS.max_log_bytes);
        let offset = size.saturating_sub(input_limit);
        if offset > 0 && file.seek(SeekFrom::Start(offset)).is_err() {
            return self.notice(
                &entry,
                NoticeKind::Unavailable,
                NoticeCode::SourceUnavailable,
            );
        }
        let mut raw = Vec::new();
        if (&mut file).take(input_limit).read_to_end(&mut raw).is_err() {
            return self.notice(
                &entry,
                NoticeKind::Unavailable,
                NoticeCode::SourceUnavailable,
            );
        }
        // This computer's home folder becomes `~` before the shared
        // transform pseudonymises the account of any other home folder.
        let raw = self.scrubber.scrub_bytes(&raw);
        let mut transformed = Vec::new();
        let Ok(report) = transform_canonical_jsonl(
            raw.as_ref(),
            &mut transformed,
            self.collector.pseudonymizer(),
            CanonicalJsonlTransformLimits {
                max_input_bytes: raw.len() as u64,
                max_output_bytes: *budget,
                discard_initial_fragment: offset > 0,
            },
        ) else {
            return self.notice(
                &entry,
                NoticeKind::Unavailable,
                NoticeCode::SourceUnavailable,
            );
        };
        // String values only: keys and the canonical shape stay intact.
        let scrubbed = self.scrubber.scrub_jsonl(&transformed);
        *budget = budget.saturating_sub(scrubbed.len() as u64);
        let truncated = offset > 0 || report.output_limit_reached;
        if !scrubbed.is_empty() {
            let added = self.add(
                &entry,
                BundleSource::Log,
                size,
                &scrubbed,
                truncated.then_some(BundleTruncation {
                    original_offset: offset,
                    reason: if report.output_limit_reached {
                        TruncationReason::GlobalLimit
                    } else {
                        TruncationReason::PerSourceLimit
                    },
                }),
            )?;
            if added {
                // The same records a Pier lists for its logs.
                let entry_path = BundlePath::new(&entry)?;
                for key_path in &report.redacted_key_paths {
                    self.collector.redaction(RedactionRecord {
                        entry_path: entry_path.clone(),
                        key_path: key_path.clone(),
                        reason: RedactionReason::IdentityPseudonymized,
                    })?;
                }
            }
        }
        if truncated {
            self.notice(&entry, NoticeKind::Truncated, NoticeCode::LogPayloadLimit)?;
        }
        self.collector.record_log_transform(&entry, &report)
    }

    fn config(&mut self, directory: Option<&Path>) -> Result<(), SupportBundleError> {
        for name in CONFIG_FILES {
            let entry = format!("config/{name}");
            let Some(directory) = directory else {
                self.notice(&entry, NoticeKind::Unavailable, NoticeCode::SourceNotFound)?;
                continue;
            };
            let (file, size) = match open_regular(&directory.join(name)) {
                Ok(opened) => opened,
                Err((kind, code)) => {
                    self.notice(&entry, kind, code)?;
                    continue;
                }
            };
            if size > LIMITS.max_document_bytes {
                self.notice(&entry, NoticeKind::Truncated, NoticeCode::SourceTooLarge)?;
                continue;
            }
            let mut bytes = Vec::new();
            if file
                .take(LIMITS.max_document_bytes)
                .read_to_end(&mut bytes)
                .is_err()
            {
                self.notice(
                    &entry,
                    NoticeKind::Unavailable,
                    NoticeCode::SourceUnavailable,
                )?;
                continue;
            }
            match serde_json::from_slice::<serde_json::Value>(&bytes) {
                Ok(document) => {
                    self.add_json(&entry, BundleSource::Configuration, Some(size), document)?;
                }
                Err(_) => self.notice(&entry, NoticeKind::Invalid, NoticeCode::SourceInvalid)?,
            }
        }
        Ok(())
    }

    fn diagnostics(&mut self, facts: &SupportFacts) -> Result<(), SupportBundleError> {
        let displays: Vec<serde_json::Value> = facts
            .displays
            .iter()
            .map(|monitor| {
                // EDID and serial number identify a physical panel; the
                // rest is what a colour or layout question needs.
                serde_json::json!({
                    "id": monitor.id,
                    "name": monitor.name,
                    "primary": monitor.is_primary,
                    "x": monitor.x,
                    "y": monitor.y,
                    "width_px": monitor.width_px,
                    "height_px": monitor.height_px,
                    "scale": monitor.scale,
                    "refresh_hz": monitor.refresh_hz,
                    "width_mm": monitor.width_mm,
                    "height_mm": monitor.height_mm,
                    "vendor": monitor.vendor,
                    "model": monitor.model,
                    "serial_present": monitor.serial != 0,
                    "edid_bytes": monitor.edid.len() / 2,
                    "color": monitor.color.as_ref().map(serde_json::to_value).transpose()
                        .unwrap_or(None),
                })
            })
            .collect();
        self.add_json(
            "diagnostics/displays.json",
            BundleSource::Diagnostics,
            None,
            serde_json::Value::Array(displays),
        )?;
        match &facts.decode {
            Some(report) => self.add_json(
                "diagnostics/decode.json",
                BundleSource::Diagnostics,
                None,
                report.to_json(),
            )?,
            None => self.notice(
                "diagnostics/decode.json",
                NoticeKind::Unavailable,
                NoticeCode::DiagnosticUnavailable,
            )?,
        }
        self.network(facts)?;
        if facts.details.platform.is_null() {
            self.notice(
                "diagnostics/platform.json",
                NoticeKind::Unavailable,
                NoticeCode::DiagnosticUnavailable,
            )?;
        } else {
            self.add_json(
                "diagnostics/platform.json",
                BundleSource::Diagnostics,
                None,
                facts.details.platform.clone(),
            )?;
        }
        match &facts.usb_component {
            Some(state) => self.add_json(
                "diagnostics/native-usb-component.json",
                BundleSource::Diagnostics,
                None,
                serde_json::json!({
                    "state": format!("{state:?}"),
                    "ready": state.is_ready(),
                }),
            )?,
            None => self.notice(
                "diagnostics/native-usb-component.json",
                NoticeKind::Advisory,
                NoticeCode::ServiceNotInstalled,
            )?,
        }
        match &facts.session {
            Some(snapshot) => self.add_json(
                "session/last-session.json",
                BundleSource::RuntimeState,
                None,
                snapshot.to_json(),
            )?,
            None => self.notice(
                "session/last-session.json",
                NoticeKind::Advisory,
                NoticeCode::SourceNotFound,
            )?,
        }
        Ok(())
    }

    fn network(&mut self, facts: &SupportFacts) -> Result<(), SupportBundleError> {
        if facts.network.is_some() || facts.outgoing.is_some() {
            let document = serde_json::json!({
                "target": facts.network.as_ref().map(|network| network.target.clone()),
                "path": facts.network.as_ref()
                    .and_then(|network| network.probe.snapshot.as_ref())
                    .map(serde_json::to_value).transpose().unwrap_or(None),
                "network_identity_permission": facts.network.as_ref()
                    .map(|network| format!("{:?}", network.probe.permission)),
                "outgoing_interface": facts.outgoing.as_ref().map(|outgoing| serde_json::json!({
                    "description": outgoing.description,
                    "mtu": outgoing.mtu,
                })),
            });
            self.add_json(
                "diagnostics/network.json",
                BundleSource::Diagnostics,
                None,
                document,
            )?;
        } else {
            self.notice(
                "diagnostics/network.json",
                NoticeKind::Advisory,
                NoticeCode::SourceNotFound,
            )?;
        }
        Ok(())
    }

    fn crash_reports(&mut self, facts: &SupportFacts) -> Result<(), SupportBundleError> {
        let mut found: Vec<(SystemTime, &super::CrashReportSource)> = facts
            .details
            .crash_reports
            .iter()
            .filter_map(|source| {
                let metadata = std::fs::symlink_metadata(&source.path).ok()?;
                metadata
                    .file_type()
                    .is_file()
                    .then(|| (metadata.modified().unwrap_or(UNIX_EPOCH), source))
            })
            .collect();
        if found.is_empty() {
            return self.notice("crash", NoticeKind::Advisory, NoticeCode::SourceNotFound);
        }
        found.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| left.1.path.cmp(&right.1.path))
        });
        if found.len() > CRASH_REPORTS_KEPT {
            self.notice("crash", NoticeKind::Advisory, NoticeCode::SourceTooLarge)?;
            found.truncate(CRASH_REPORTS_KEPT);
        }
        let mut used = Vec::new();
        for (_, source) in found {
            let name = self.crash_entry_name(&source.path, &used);
            used.push(name.clone());
            let entry = format!("crash/{name}");
            let (file, size) = match open_regular(&source.path) {
                Ok(opened) => opened,
                Err((kind, code)) => {
                    self.notice(&entry, kind, code)?;
                    continue;
                }
            };
            if size > MAX_CRASH_REPORT_BYTES {
                self.notice(&entry, NoticeKind::Truncated, NoticeCode::SourceTooLarge)?;
                continue;
            }
            let mut bytes = Vec::new();
            if file
                .take(MAX_CRASH_REPORT_BYTES)
                .read_to_end(&mut bytes)
                .is_err()
            {
                self.notice(
                    &entry,
                    NoticeKind::Unavailable,
                    NoticeCode::SourceUnavailable,
                )?;
                continue;
            }
            match source.kind {
                CrashReportKind::WindowsMinidump => {
                    if !is_stack_only_minidump(&bytes) {
                        self.notice(
                            &entry,
                            NoticeKind::Omitted,
                            NoticeCode::SensitiveRuntimeExcluded,
                        )?;
                        continue;
                    }
                    self.scrubber.scrub_binary(&mut bytes);
                    self.add(&entry, BundleSource::Diagnostics, size, &bytes, None)?;
                }
                CrashReportKind::MacDiagnosticReport => {
                    let (text, records) = self.diagnostic_report(&entry, &bytes)?;
                    if self.add(
                        &entry,
                        BundleSource::Diagnostics,
                        size,
                        text.as_bytes(),
                        None,
                    )? {
                        for record in records {
                            self.collector.redaction(record)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// A macOS `.ips` report is a JSON header line and a JSON body: both
    /// are redacted like any document; anything else is scrubbed as text.
    fn diagnostic_report(
        &self,
        entry: &str,
        bytes: &[u8],
    ) -> Result<(String, Vec<arcen_telemetry::RedactionRecord>), SupportBundleError> {
        let text = String::from_utf8_lossy(bytes);
        let bundle_path = BundlePath::new(entry)?;
        let (header, body) = text.split_once('\n').unwrap_or((&text, ""));
        let parsed = (
            serde_json::from_str::<serde_json::Value>(header),
            serde_json::from_str::<serde_json::Value>(body),
        );
        if let (Ok(mut header), Ok(mut body)) = parsed {
            let mut records = redact_deck_json(
                &bundle_path,
                &mut header,
                self.collector.pseudonymizer(),
                &self.scrubber,
            )?;
            // The body's key paths are recorded under `/body`.
            let mut wrapped = serde_json::json!({ "body": body.take() });
            records.extend(redact_deck_json(
                &bundle_path,
                &mut wrapped,
                self.collector.pseudonymizer(),
                &self.scrubber,
            )?);
            let body = wrapped
                .get_mut("body")
                .map(serde_json::Value::take)
                .unwrap_or_default();
            let rendered = format!(
                "{}\n{}\n",
                serde_json::to_string(&header).unwrap_or_default(),
                serde_json::to_string_pretty(&body).unwrap_or_default()
            );
            return Ok((rendered, records));
        }
        Ok((self.scrubber.scrub_text(&text).into_owned(), Vec::new()))
    }

    /// The report's file name, restricted to safe characters, without the
    /// account name, and unique within the bundle.
    fn crash_entry_name(&self, path: &Path, used: &[String]) -> String {
        let original = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let scrubbed = self.scrubber.scrub_text(&original);
        let mut name: String = scrubbed
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '_'
                }
            })
            .take(120)
            .collect();
        if name.is_empty() || name.starts_with('.') {
            name.insert_str(0, "report");
        }
        let base = name.clone();
        let mut index = 1;
        while used.contains(&name) {
            name = format!("{index}-{base}");
            index += 1;
        }
        name
    }
}

/// Whether `bytes` is a minidump (`MDMP`) whose `MINIDUMP_HEADER.Flags`
/// (offset 24) ask for no process memory beyond thread stacks: a dump
/// with full, private or write-copy memory could hold a password or typed
/// text and is left out.
fn is_stack_only_minidump(bytes: &[u8]) -> bool {
    const WITH_FULL_MEMORY: u64 = 0x0000_0002;
    const WITH_INDIRECTLY_REFERENCED_MEMORY: u64 = 0x0000_0040;
    const WITH_PRIVATE_READ_WRITE_MEMORY: u64 = 0x0000_0200;
    const WITH_PRIVATE_WRITE_COPY_MEMORY: u64 = 0x0001_0000;
    const MEMORY: u64 = WITH_FULL_MEMORY
        | WITH_INDIRECTLY_REFERENCED_MEMORY
        | WITH_PRIVATE_READ_WRITE_MEMORY
        | WITH_PRIVATE_WRITE_COPY_MEMORY;
    let Some(flags) = bytes
        .get(24..32)
        .and_then(|flags| <[u8; 8]>::try_from(flags).ok())
    else {
        return false;
    };
    bytes.starts_with(b"MDMP") && u64::from_le_bytes(flags) & MEMORY == 0
}

fn is_log_file_name(name: &str) -> bool {
    name == LOG_FILE_PREFIX
        || name
            .strip_prefix(LOG_FILE_PREFIX)
            .and_then(|rest| rest.strip_prefix('.'))
            .is_some_and(|date| {
                date.len() == 10
                    && date.bytes().enumerate().all(|(index, byte)| match index {
                        4 | 7 => byte == b'-',
                        _ => byte.is_ascii_digit(),
                    })
            })
}

/// Opens a regular file through the shared collector's safe open
/// ([`open_source`]), with its notice when it cannot be read.
fn open_regular(path: &Path) -> Result<(File, u64), (NoticeKind, NoticeCode)> {
    open_source(path)
        .map(|(file, opened)| (file, opened.len()))
        .map_err(|error| error.notice())
}

/// The last selected saved connection's host and port, from
/// `connections.json` in `config_dir`, for the network probe.
#[must_use]
pub fn last_selected_target(config_dir: &Path) -> Option<(String, u16)> {
    let (file, size) = open_regular(&config_dir.join("connections.json")).ok()?;
    if size > LIMITS.max_document_bytes {
        return None;
    }
    let document: serde_json::Value =
        serde_json::from_reader(file.take(LIMITS.max_document_bytes)).ok()?;
    let selected = document.get("last_selected_connection_id")?.as_str()?;
    let connection = document
        .get("connections")?
        .as_array()?
        .iter()
        .find(|connection| {
            connection.get("id").and_then(serde_json::Value::as_str) == Some(selected)
        })?;
    let host = connection.get("host")?.as_str()?.trim();
    let port = connection
        .get("port")
        .and_then(serde_json::Value::as_u64)
        .and_then(|port| u16::try_from(port).ok())
        .unwrap_or(crate::quick_connect::DEFAULT_PORT);
    (!host.is_empty()).then(|| (host.to_owned(), port))
}

/// The host a bundle reports the network path to: the last session of this
/// process (also one made without saving the connection, such as
/// `--connect`), else the saved, selected connection in `config_dir`.
#[must_use]
pub fn bundle_target(
    session: Option<(String, u16)>,
    config_dir: Option<&Path>,
) -> Option<(String, u16)> {
    session.or_else(|| config_dir.and_then(last_selected_target))
}

/// The log level saved in `settings.json` in `config_dir` (what the GUI
/// runs at), for a bundle made from the command line.
#[must_use]
pub fn saved_log_level(config_dir: &Path) -> Option<LogLevel> {
    let (file, size) = open_regular(&config_dir.join("settings.json")).ok()?;
    if size > LIMITS.max_document_bytes {
        return None;
    }
    let document: serde_json::Value =
        serde_json::from_reader(file.take(LIMITS.max_document_bytes)).ok()?;
    let level = document.get("log_level")?.as_u64()?;
    u8::try_from(level).ok().map(LogLevel::from_index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_names_and_times_are_utc() {
        let at = UNIX_EPOCH + std::time::Duration::from_secs(1_794_726_245);
        assert_eq!(bundle::utc_stamp(unix_seconds(at)), "20261115T070405Z");
    }

    #[test]
    fn only_stack_only_minidumps_are_kept() {
        let dump = |flags: u64| {
            let mut bytes = b"MDMP\x93\xa7\0\0".to_vec();
            bytes.resize(24, 0);
            bytes.extend(flags.to_le_bytes());
            bytes
        };
        assert!(is_stack_only_minidump(&dump(
            0x0000_0001 | 0x0000_0004 | 0x0000_0100
        )));
        assert!(!is_stack_only_minidump(&dump(0x0000_0002)));
        assert!(!is_stack_only_minidump(&dump(0x0000_0200)));
        assert!(!is_stack_only_minidump(b"MDMP"));
        assert!(!is_stack_only_minidump(&[0_u8; 40]));
    }

    #[test]
    fn only_the_decks_daily_logs_are_candidates() {
        assert!(is_log_file_name("arcen-client.log.2026-11-15"));
        assert!(is_log_file_name("arcen-client.log"));
        for name in [
            "arcen-client.log.2026-11-15.bak",
            "arcen-client.log.20261115",
            "disk-guardian.out.log",
            "arcen-client.log.2026-1x-15",
        ] {
            assert!(!is_log_file_name(name), "{name}");
        }
    }
}
