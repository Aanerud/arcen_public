//! The Grading and HDR part of `arcen-pier wayland qualify`.
//!
//! Decides from the stack's facts whether a wide composition route exists
//! (the shared `fidelity_route`: none before Mutter 50, which added HDR
//! screen sharing in mutter!4928), and on a stack that has one proves it the
//! way a Grading or HDR session will use it, all on the host:
//!
//! 1. a session started on the route (`force-hdr`: the compositor's outputs
//!    in a BT.2100 colour state), its outputs recorded again for wide
//!    capture by the agent;
//! 2. capenc's own ramp client showing a PQ-tagged ten-bit ramp, and capenc
//!    reading it back through the wide capture (`fidelity-probe`): the
//!    distinct codes and their error prove native-wide samples for that
//!    format and memory, or eight-bit pixels in a wide container;
//! 3. the same with an untagged (sRGB) picture: its white patch measures
//!    where the composition puts SDR white (BT.2408's 203 nits expected);
//! 4. a real Grading and HDR epoch admitted and encoded on the proven facts.
//!
//! Every gap is recorded with a stable reason, and the Pier serves Grading
//! and HDR only from what this proved.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use arcen_media::video::compositor_fidelity::{
    fidelity_route, CompositorColourFacts, CompositorRelease, FidelityGap, FidelityRoute,
};
use arcen_media::video::{pipeline_contract, PipelineId};
use arcen_protocol::messages::{ClientMonitor, CursorMode};
use arcen_session::desktop_provider::{CapabilityFact, DesktopProvider};
use arcen_session::wayland_qualification::{FidelityProof, RampProof};
use tokio::io::{AsyncBufReadExt, BufReader};

use super::{step, QualificationUser};
use crate::media::capenc::{self, CapencConfig, EncoderSelection};
use crate::session::launcher::{AuthenticatedLauncher, LauncherConfig, SessionLauncher};
use crate::session::wayland::{output_client, WaylandAgentReady};

/// Mutter added its `DisplayConfig` colour modes and the forced-HDR switch
/// in GNOME 48.
const MUTTER_COLOUR_MANAGEMENT_MAJOR: u32 = 48;
const RAMP_SECONDS: u32 = 40;
const RAMP_READY_TIMEOUT: Duration = Duration::from_secs(15);
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// What the fidelity qualification proved.
pub(super) struct FidelityOutcome {
    pub grading: CapabilityFact,
    pub hdr: CapabilityFact,
    pub proof: FidelityProof,
}

impl FidelityOutcome {
    fn gap(gap: FidelityGap, route: Option<FidelityRoute>, ramps: Vec<RampProof>) -> Self {
        Self {
            grading: CapabilityFact::Missing,
            hdr: CapabilityFact::Missing,
            proof: FidelityProof {
                route: route.map(|route| route.token().to_owned()),
                gap: Some(gap.token().to_owned()),
                detail: Some(gap.to_string()),
                ramps,
                sdr_white_nits: None,
            },
        }
    }
}

/// The route the stack's compositor offers, from its version.
fn route_for(compositor_version: &str) -> Result<FidelityRoute, FidelityGap> {
    let compositor = CompositorRelease::parse_mutter(compositor_version);
    let colour_management = matches!(
        compositor,
        CompositorRelease::Mutter { major, .. } if major >= MUTTER_COLOUR_MANAGEMENT_MAJOR
    );
    fidelity_route(CompositorColourFacts {
        compositor,
        colour_management,
        // No released Mutter offers an HDR colour mode on a virtual
        // monitor; the forced-HDR route is the one to prove.
        outputs_offer_bt2100: false,
    })
}

/// One parsed `FIDELITY` line.
#[derive(Debug, Clone, PartialEq)]
struct Measurement {
    ramp: RampProof,
    white_nits: f64,
}

fn parse_measurement(line: &str) -> Option<Measurement> {
    let rest = line.split("[capenc] FIDELITY ").nth(1)?;
    let field = |name: &str| {
        rest.split_whitespace()
            .find_map(|token| token.strip_prefix(name)?.strip_prefix('='))
    };
    Some(Measurement {
        ramp: RampProof {
            format: field("format")?.to_owned(),
            memory: field("memory")?.to_owned(),
            expected_codes: field("ramp_expected")?.parse().ok()?,
            distinct_codes: field("ramp_distinct")?.parse().ok()?,
            max_error_codes: field("ramp_max_error")?.parse().ok()?,
            native_wide: field("precision")? == "native-wide",
            channels_verified: field("channels") == Some("ok"),
        },
        white_nits: field("white_nits")?.parse().ok()?,
    })
}

/// Opens a qualification session on `route`.
async fn open_session(
    config: &crate::cli::Config,
    user: &QualificationUser,
    (width, height, refresh_hz): (u32, u32, u32),
    route: FidelityRoute,
) -> Result<SessionLauncher, String> {
    let launcher_config = LauncherConfig {
        binary: config
            .resolve_session_launcher_binary()
            .ok_or("the session launcher cannot be resolved")?,
        pam_service: config.pam_service.clone(),
        display: config.session_display.clone(),
        gpu_head: config.session_gpu_head.clone(),
        xorg_binary: config.xorg_bin.clone(),
        xorg_config_template: config.xorg_config_template.clone(),
        runtime_root: config.session_runtime_root.clone(),
        agent_binary: config
            .resolve_session_agent_binary()
            .ok_or("the session agent cannot be resolved")?,
        desktop: config.desktop_session.clone(),
        deskside: crate::deskside::LinuxDesksideConfig::default(),
        provider: DesktopProvider::Wayland,
        audio: crate::session::agent::AgentAudioArguments {
            local_playback: config.local_playback,
            pactl: config.pactl_bin.clone(),
        },
        // The temporary qualification account never sees a first-login
        // assistant over the desktop being measured.
        first_login_setup: arcen_session::desktop_session::first_login::FirstLoginSetup::Skip,
        // X11 applications are proven by the first output's own session.
        x11_apps: false,
    };
    let permit = Arc::new(tokio::sync::Semaphore::new(1))
        .acquire_owned()
        .await
        .map_err(|error| error.to_string())?;
    let mut pending = AuthenticatedLauncher::authenticate(
        &launcher_config,
        &user.name,
        zeroize::Zeroizing::new(user.password.as_str().to_owned()),
        "127.0.0.1",
        permit,
        crate::eventlog::random_correlation_id(),
    )
    .await
    .map_err(|error| format!("PAM service {}: {error}", config.pam_service))?;
    pending.set_session_display(Some(crate::session::launcher::SessionDisplayMsg::served(
        &ClientMonitor {
            refresh_hz,
            ..ClientMonitor::default()
        },
        width,
        height,
    )));
    pending.set_wayland_fidelity_route(Some(route.token()));
    pending
        .open()
        .await
        .map_err(|error| format!("headless session did not start: {error}"))
}

/// A ten-bit capenc configuration for `pipeline` on the session's wide
/// stream, carrying `fidelity_args`.
fn capenc_config(
    config: &crate::cli::Config,
    launcher: &SessionLauncher,
    ready: &WaylandAgentReady,
    wide_node: u32,
    pipeline: PipelineId,
    fidelity_args: Vec<String>,
) -> Result<CapencConfig, String> {
    let binary = config
        .resolve_capenc_binary()
        .ok_or("the Pier executable cannot be resolved for capenc")?;
    let mut execution = launcher.execution();
    let handoff = ready
        .primary_handoff()
        .and_then(|handoff| handoff.with_wide_node(Some(wide_node)))
        .map_err(|error| error.to_string())?;
    execution.environment = execution
        .environment
        .clone()
        .with_wayland_session(ready, &handoff);
    let stream = &ready.streams[0];
    let mut capenc = config.capenc_config_for_size(
        binary,
        Some(execution),
        crate::eventlog::random_correlation_id(),
        CursorMode::Host,
        stream.width,
        stream.height,
    );
    let contract = pipeline_contract(pipeline);
    capenc.codec = contract.colour.codec.token().to_owned();
    capenc.encoder = EncoderSelection::NativeNvenc;
    capenc.fps = contract.max_fps;
    capenc.yuv444 = true;
    capenc.bit_depth = contract.colour.bit_depth;
    capenc.color_range = contract.colour.range;
    capenc.color_matrix = contract.colour.matrix;
    capenc.transfer = contract.colour.transfer;
    capenc.color_primaries = contract.colour.primaries;
    capenc.desktop_encoding = arcen_media::video::DesktopSignalEncoding::Rec2100Pq;
    capenc.video_selection = contract.selection;
    capenc.codec_pinned = false;
    capenc.variant_pinned = false;
    capenc.intent = contract.intent;
    capenc.motion_priority = contract.priority;
    capenc.qp_map = arcen_media::video::QpMapPolicy::Off;
    capenc.encoder_max_bitrate_bps = contract.encoder_ceiling_bps();
    capenc.capture_rate = None;
    capenc.gpu_import_verified = false;
    capenc.wayland_fidelity_args = fidelity_args;
    Ok(capenc)
}

/// Shows the ramp client's picture and measures it through the wide
/// capture.
async fn measure(capenc_config: &CapencConfig, pq: bool) -> Result<Measurement, String> {
    let mut client = capenc::wayland_ramp_client_command(capenc_config, pq, RAMP_SECONDS)
        .map_err(|error| error.to_string())?;
    client
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut client = client
        .spawn()
        .map_err(|error| format!("ramp client: {error}"))?;
    let stderr = client.stderr.take().ok_or("ramp client stderr")?;
    let mut lines = BufReader::new(stderr).lines();
    let ready = tokio::time::timeout(RAMP_READY_TIMEOUT, async {
        while let Ok(Some(line)) = lines.next_line().await {
            if line.starts_with("RAMP-READY ") {
                return Ok(line);
            }
            if line.contains("ERROR") {
                return Err(line);
            }
        }
        Err("the ramp client ended before showing its picture".to_owned())
    })
    .await
    .map_err(|_| "the ramp client did not show its picture in time".to_owned())??;
    tracing::debug!(target: crate::logging::target::SESSION, %ready, "fidelity ramp shown");
    // The compositor composes the new surface on its next frame.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let mut probe =
        capenc::wayland_fidelity_probe_command(capenc_config).map_err(|error| error.to_string())?;
    probe
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(PROBE_TIMEOUT, probe.output())
        .await
        .map_err(|_| "the fidelity probe timed out".to_owned())?
        .map_err(|error| format!("fidelity probe: {error}"))?;
    drop(client.stdin.take());
    let _ = tokio::time::timeout(Duration::from_secs(5), client.wait()).await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr.lines().find_map(parse_measurement).ok_or_else(|| {
        stderr
            .lines()
            .rev()
            .find(|line| line.contains("ERROR"))
            .map_or_else(
                || "the fidelity probe reported no measurement".to_owned(),
                str::to_owned,
            )
    })
}

/// Admits and encodes one real epoch of `pipeline` on the proven facts.
async fn encode(capenc_config: CapencConfig, pipeline: PipelineId) -> Result<String, String> {
    let (session, plan) = capenc::spawn(capenc_config)
        .await
        .map_err(|error| format!("capenc: {error}"))?;
    let notice = session.capture_admission().copied();
    session.shutdown().await;
    let notice = notice.ok_or("capenc READY without an admission")?;
    let served = notice.served().copied().ok_or("nothing admitted")?;
    if served.pipeline != pipeline {
        return Err(format!(
            "{} was served as {}",
            pipeline.token(),
            served.pipeline.token()
        ));
    }
    Ok(format!(
        "READY {plan:?}; transform={}",
        served.transform.token()
    ))
}

/// Qualifies Grading and HDR on this stack.
pub(super) async fn qualify(
    config: &crate::cli::Config,
    user: &QualificationUser,
    compositor_version: &str,
    output: (u32, u32, u32),
) -> FidelityOutcome {
    let route = match route_for(compositor_version) {
        Ok(route) => route,
        Err(gap) => {
            // A compositor without the capability is not a failed run.
            println!("no   fidelity_route: {gap}");
            return FidelityOutcome::gap(gap, None, Vec::new());
        }
    };
    step(
        "fidelity_route",
        true,
        &format!("{} on {compositor_version}", route.token()),
    );
    let launcher = match open_session(config, user, output, route).await {
        Ok(launcher) => launcher,
        Err(error) => {
            step("fidelity_session", false, &error);
            return FidelityOutcome::gap(FidelityGap::NoHdrRoute, Some(route), Vec::new());
        }
    };
    let outcome = qualify_in_session(config, &launcher, route).await;
    let session_id = launcher
        .environment
        .session_id()
        .unwrap_or_default()
        .to_owned();
    let runtime_dir = launcher
        .wayland
        .as_ref()
        .map(|ready| ready.runtime_dir.clone());
    let stopped = launcher.shutdown().await;
    let gone = match &runtime_dir {
        Some(dir) => super::session_gone(&session_id, dir).await,
        None => true,
    };
    step(
        "fidelity_teardown",
        stopped && gone,
        &format!("agent stopped={stopped}; session {session_id} gone={gone}"),
    );
    outcome
}

#[allow(clippy::too_many_lines)] // One linear proof.
async fn qualify_in_session(
    config: &crate::cli::Config,
    launcher: &SessionLauncher,
    route: FidelityRoute,
) -> FidelityOutcome {
    let Some(ready) = launcher.wayland.clone() else {
        step(
            "fidelity_session",
            false,
            "the launcher started no Wayland session",
        );
        return FidelityOutcome::gap(FidelityGap::NoHdrRoute, Some(route), Vec::new());
    };
    let wide = match output_client::wide_streams(
        &ready,
        launcher.identity.uid,
        launcher.agent_pid,
        ready.generation,
    )
    .await
    {
        Ok(reply) => reply.streams.first().map(|stream| stream.node_id),
        Err(error) => {
            step("fidelity_wide_stream", false, &error.to_string());
            None
        }
    };
    let Some(wide_node) = wide else {
        return FidelityOutcome::gap(FidelityGap::WideFormatsNotOffered, Some(route), Vec::new());
    };
    step(
        "fidelity_wide_stream",
        true,
        &format!(
            "RecordMonitor node {wide_node} for {}",
            ready.streams[0].connector
        ),
    );
    // GNOME Shell opens its Activities overview at login; a ramp shown under
    // it is a scaled thumbnail, not the picture (GNOME Shell 51.1 lab: all
    // 1024 codes present, max error 810). Bare Mutter has no overview.
    match super::leave_overview(&ready).await {
        Ok(escapes) => step(
            "fidelity_leave_overview",
            true,
            &format!("{escapes} Escape key presses accepted"),
        ),
        Err(error) => step("fidelity_leave_overview", false, &error),
    }
    // One ramp per composition: each offers its own preferred wide format
    // first (HDR ten-bit, Grading half float), and a proof is per format
    // and memory, so each preference is measured where it is negotiated.
    let mut ramps: Vec<RampProof> = Vec::new();
    let mut white_config = None;
    for pipeline in [PipelineId::Hdr, PipelineId::Grading] {
        let name = format!("fidelity_ramp_{}", pipeline.token());
        let probe_config = match capenc_config(
            config,
            launcher,
            &ready,
            wide_node,
            pipeline,
            vec![format!("fidelity-route={}", route.token())],
        ) {
            Ok(probe_config) => probe_config,
            Err(error) => {
                step(&name, false, &error);
                continue;
            }
        };
        match measure(&probe_config, true).await {
            Ok(measured) => {
                let ramp = measured.ramp;
                step(
                    &name,
                    ramp.proves(),
                    &format!(
                        "{} {}: {} of {} codes, max error {}, channel order {}",
                        ramp.format,
                        ramp.memory,
                        ramp.distinct_codes,
                        ramp.expected_codes,
                        ramp.max_error_codes,
                        if ramp.channels_verified {
                            "kept"
                        } else {
                            "wrong"
                        },
                    ),
                );
                if !ramps
                    .iter()
                    .any(|known| (&known.format, &known.memory) == (&ramp.format, &ramp.memory))
                {
                    ramps.push(ramp);
                }
            }
            Err(error) => step(&name, false, &error),
        }
        white_config.get_or_insert(probe_config);
    }
    let Some(white_config) = white_config.filter(|_| !ramps.is_empty()) else {
        return FidelityOutcome::gap(FidelityGap::WideImportUnproven, Some(route), ramps);
    };
    if !ramps.iter().any(RampProof::proves) {
        let gap = if ramps.iter().all(|ramp| {
            ramp.distinct_codes <= arcen_media::video::compositor_fidelity::EIGHT_BIT_CODES
        }) {
            FidelityGap::UpconvertedEightBit
        } else {
            FidelityGap::PrecisionUnproven
        };
        return FidelityOutcome::gap(gap, Some(route), ramps);
    }
    let white = match measure(&white_config, false).await {
        Ok(white) => white.white_nits,
        Err(error) => {
            step("fidelity_white", false, &error);
            return FidelityOutcome::gap(FidelityGap::ReferenceWhiteUnproven, Some(route), ramps);
        }
    };
    let white_ok = arcen_media::video::pq_white::is_graphics_white(white);
    step(
        "fidelity_white",
        white_ok,
        &format!("SDR white composed at {white:.1} nits (203 expected)"),
    );
    if !white_ok {
        return FidelityOutcome::gap(FidelityGap::ReferenceWhiteUnproven, Some(route), ramps);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let white_nits = white.round() as u16;
    let proven = ramps
        .iter()
        .filter(|ramp| ramp.proves())
        .map(|ramp| format!("{}@{}", ramp.format, ramp.memory))
        .collect::<Vec<_>>()
        .join(",");
    let args = vec![
        format!("fidelity-route={}", route.token()),
        format!("wide-proven={proven}"),
        format!("sdr-white={white_nits}"),
    ];
    let proof = FidelityProof {
        route: Some(route.token().to_owned()),
        gap: None,
        detail: None,
        ramps,
        sdr_white_nits: Some(white_nits),
    };
    let mut facts = [CapabilityFact::Missing, CapabilityFact::Missing];
    for (index, pipeline) in [PipelineId::Grading, PipelineId::Hdr]
        .into_iter()
        .enumerate()
    {
        let result =
            match capenc_config(config, launcher, &ready, wide_node, pipeline, args.clone()) {
                Ok(epoch) => encode(epoch, pipeline).await,
                Err(error) => Err(error),
            };
        let detail = match &result {
            Ok(detail) | Err(detail) => detail.clone(),
        };
        step(
            &format!("fidelity_{}", pipeline.token()),
            result.is_ok(),
            &detail,
        );
        if result.is_ok() {
            facts[index] = CapabilityFact::Available;
        }
    }
    FidelityOutcome {
        grading: facts[0],
        hdr: facts[1],
        proof,
    }
}

/// The summary line for one preset.
pub(super) fn summary(outcome: &FidelityOutcome, pipeline: PipelineId) -> String {
    let fact = match pipeline {
        PipelineId::Grading => outcome.grading,
        _ => outcome.hdr,
    };
    if fact == CapabilityFact::Available {
        let ramp = outcome.proof.ramps.first();
        format!(
            "yes ({} route; {} {} ramp {} of {} codes; SDR white {} nits)",
            outcome.proof.route.as_deref().unwrap_or("?"),
            ramp.map_or("?", |ramp| ramp.format.as_str()),
            ramp.map_or("?", |ramp| ramp.memory.as_str()),
            ramp.map_or(0, |ramp| ramp.distinct_codes),
            ramp.map_or(0, |ramp| ramp.expected_codes),
            outcome
                .proof
                .sdr_white_nits
                .map_or_else(|| "?".to_owned(), |nits| nits.to_string()),
        )
    } else {
        format!(
            "no ({})",
            outcome
                .proof
                .detail
                .as_deref()
                .or(outcome.proof.gap.as_deref())
                .unwrap_or("not proven")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_route_follows_the_compositor_release() {
        assert_eq!(
            route_for("49.4"),
            Err(FidelityGap::NoHdrScreencast(CompositorRelease::Mutter {
                major: 49,
                minor: 4
            }))
        );
        assert_eq!(route_for("40.10"), Err(FidelityGap::NoColourManagement));
        assert_eq!(route_for("50.6"), Ok(FidelityRoute::ForcedHdr));
        assert_eq!(route_for("51.1"), Ok(FidelityRoute::ForcedHdr));
    }

    #[test]
    fn a_fidelity_line_parses_into_a_ramp_proof() {
        let line = "[capenc] FIDELITY version=1 format=xbgr2101010 memory=dmabuf transfer=pq \
                    primaries=bt2020 width=1920 height=1080 ramp_expected=1024 \
                    ramp_distinct=1024 ramp_max_error=0 precision=native-wide channels=ok \
                    channel_codes=900,520,140 white_pq_code=594 \
                    white_nits=202.9 import_ms=6.62";
        let measured = parse_measurement(line).unwrap();
        assert_eq!(measured.ramp.format, "xbgr2101010");
        assert_eq!(measured.ramp.memory, "dmabuf");
        assert_eq!(measured.ramp.distinct_codes, 1024);
        assert!(measured.ramp.native_wide);
        assert!(measured.ramp.proves());
        let swapped = line.replace("channels=ok", "channels=wrong");
        assert!(!parse_measurement(&swapped).unwrap().ramp.proves());
        assert!((measured.white_nits - 202.9).abs() < 1e-9);
        assert!(parse_measurement("[capenc] READY version=2").is_none());
        let eight = line.replace("precision=native-wide", "precision=upconverted-eight-bit");
        assert!(!parse_measurement(&eight).unwrap().ramp.native_wide);
    }

    #[test]
    fn a_gap_reads_as_its_reason() {
        let outcome = FidelityOutcome::gap(
            FidelityGap::NoHdrScreencast(CompositorRelease::Mutter {
                major: 49,
                minor: 4,
            }),
            None,
            Vec::new(),
        );
        assert_eq!(
            outcome.proof.gap.as_deref(),
            Some("compositor_lacks_hdr_screencast")
        );
        let text = summary(&outcome, PipelineId::Hdr);
        assert!(
            text.starts_with("no (Mutter 49.4 has no HDR screen sharing"),
            "{text}"
        );
    }
}
