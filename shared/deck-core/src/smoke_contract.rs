// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! What a command-line smoke (`media-smoke`, `multi-monitor-smoke`) accepts
//! as proof of a session, so every Deck's lab tool passes and fails a run by
//! the same rules.
//!
//! - **Layout.** By default every display the Deck asked for must be served.
//!   A run that explicitly accepts the host's layout
//!   ([`LayoutAcceptance::HostLayout`]) may be served fewer, and the omitted
//!   displays are named. Either way every served monitor must be one the Deck
//!   asked for; the smoke then requires a decoded frame from each.
//! - **Pipeline.** The requested and served pipelines and the host's reason
//!   for any difference are reported ([`PipelineReport`]); a degraded
//!   pipeline fails only a run that requires the requested one.
//! - **Colour.** A served Grading or HDR pipeline must carry its contract's
//!   colour in the hello ([`verify_served_colour`]), and every decoded
//!   frame's header must agree with the hello ([`verify_frame_colour`]).

use arcen_media::colour_check::ColourAxes;
use arcen_media::video::{PipelineId, pipeline_contract};
use arcen_media::{RequestedMonitorTopology, TransferCharacteristics, VideoCodec};
use arcen_protocol::messages::{
    ServedPipelineMsg, ServedStreamPipeline, ServerHelloMsg, ServerMultiMonitorMsg, StreamPipeline,
};
use arcen_protocol::wire::VideoHeader;

/// Whether a multi-monitor smoke accepts a host that serves fewer displays
/// than the Deck asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayoutAcceptance {
    /// Every requested display must be served (the default).
    #[default]
    FullLayout,
    /// The host's own layout is accepted: some displays may be omitted, and
    /// are named.
    HostLayout,
}

/// A requested display the host did not serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OmittedDisplay {
    /// Its place in the requested layout, from 1.
    pub index: usize,
    /// The Deck's identifier for it.
    pub client_display_id: String,
    /// Its name, as the Deck's operating system gives it.
    pub name: String,
}

/// The layout a host served, as a smoke checks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedLayout {
    /// How many displays the Deck asked for.
    pub requested: usize,
    /// The session monitor ids the host serves, each of which must decode.
    pub served: Vec<u16>,
    /// The requested displays the host did not serve.
    pub omitted: Vec<OmittedDisplay>,
    /// Why the host served fewer: `host_layout_kept` for a host that keeps
    /// its own layout, `host_max_monitors=N` for one at its limit, or
    /// `unstated`. `None` when nothing is omitted.
    pub reason: Option<String>,
}

impl ServedLayout {
    /// One line per omitted display, for the smoke's output.
    #[must_use]
    pub fn omitted_lines(&self) -> Vec<String> {
        self.omitted
            .iter()
            .map(|display| {
                format!(
                    "multi-monitor omitted display={} id={} name={:?} reason={}",
                    display.index,
                    display.client_display_id,
                    display.name,
                    self.reason.as_deref().unwrap_or("unstated")
                )
            })
            .collect()
    }
}

/// Checks the layout a host applied against the one this Deck asked for.
///
/// # Errors
///
/// When the host served no monitor, served one the Deck did not ask for, or
/// (unless `acceptance` is [`LayoutAcceptance::HostLayout`]) omitted any
/// requested display.
pub fn verify_applied_layout(
    requested: &RequestedMonitorTopology,
    capability: &ServerMultiMonitorMsg,
    acceptance: LayoutAcceptance,
) -> Result<ServedLayout, String> {
    let applied = capability
        .applied_topology()
        .ok_or_else(|| "host did not return an applied multi-monitor topology".to_owned())?;
    if applied.monitors().is_empty() {
        return Err("host served no monitor".to_owned());
    }
    for shown in applied.monitors() {
        if !requested
            .monitors()
            .iter()
            .any(|wanted| wanted.monitor().identity.id == shown.client_display_id.as_str())
        {
            return Err(format!(
                "host served monitor {} for display {:?}, which this Deck did not request",
                shown.session_monitor_id,
                shown.client_display_id.as_str()
            ));
        }
    }
    let omitted: Vec<OmittedDisplay> = requested
        .monitors()
        .iter()
        .enumerate()
        .filter(|(_, wanted)| {
            !applied
                .monitors()
                .iter()
                .any(|shown| shown.client_display_id.as_str() == wanted.monitor().identity.id)
        })
        .map(|(index, wanted)| OmittedDisplay {
            index: index + 1,
            client_display_id: wanted.monitor().identity.id.clone(),
            name: wanted.monitor().identity.name.clone(),
        })
        .collect();
    let served_count = applied.monitors().len();
    let reason = (!omitted.is_empty()).then(|| {
        if capability.fixed_topology() {
            "host_layout_kept".to_owned()
        } else if served_count >= usize::from(capability.max_monitors()) {
            format!("host_max_monitors={}", capability.max_monitors())
        } else {
            "unstated".to_owned()
        }
    });
    let layout = ServedLayout {
        requested: requested.monitors().len(),
        served: applied
            .monitors()
            .iter()
            .map(|monitor| monitor.session_monitor_id)
            .collect(),
        omitted,
        reason,
    };
    if acceptance == LayoutAcceptance::FullLayout && !layout.omitted.is_empty() {
        let names = layout
            .omitted
            .iter()
            .map(|display| {
                format!(
                    "display {} ({} {:?})",
                    display.index, display.client_display_id, display.name
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "host served {} of {} displays (omitted: {names}; reason: {}); \
             pass --accept-host-layout to accept the host's layout",
            served_count,
            layout.requested,
            layout.reason.as_deref().unwrap_or("unstated")
        ));
    }
    Ok(layout)
}

/// The pipeline a smoke asked for and the one the host serves, with the
/// host's reason for any difference.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PipelineReport {
    /// What this Deck asked for; `None` for custom axes.
    pub requested: Option<StreamPipeline>,
    /// What the host says it serves; `None` from a host that does not say.
    pub served: Option<ServedStreamPipeline>,
    /// The host's stable reason (`served_pipeline.reason`).
    pub reason: Option<String>,
    /// The host's sentence about it (`served_pipeline.detail`).
    pub detail: Option<String>,
}

impl PipelineReport {
    /// The report once the hello arrives.
    #[must_use]
    pub fn from_hello(requested: Option<StreamPipeline>, hello: &ServerHelloMsg) -> Self {
        Self {
            requested,
            served: hello.active_pipeline.clone(),
            reason: None,
            detail: None,
        }
    }

    /// Applies a `served_pipeline` message, which supersedes the hello.
    pub fn apply_served(&mut self, served: &ServedPipelineMsg) {
        self.served = Some(served.served.clone());
        self.reason.clone_from(&served.reason);
        self.detail.clone_from(&served.detail);
    }

    /// Whether the host serves a different pipeline from the one asked for.
    #[must_use]
    pub fn degraded(&self) -> bool {
        match (&self.requested, &self.served) {
            (Some(requested), Some(served)) => requested.token() != served.token(),
            (Some(_), None) | (None, _) => false,
        }
    }

    /// One line for the smoke's output.
    #[must_use]
    pub fn line(&self) -> String {
        format!(
            "pipeline requested={} served={} degraded={} reason={} detail={:?}",
            self.requested
                .as_ref()
                .map_or("custom", StreamPipeline::token),
            self.served
                .as_ref()
                .map_or("unreported", ServedStreamPipeline::token),
            self.degraded(),
            self.reason.as_deref().unwrap_or("-"),
            self.detail.as_deref().unwrap_or("-")
        )
    }

    /// For a run that requires the requested pipeline itself.
    ///
    /// # Errors
    ///
    /// When the host serves another pipeline or does not say which.
    pub fn require_requested(&self) -> Result<(), String> {
        let Some(requested) = &self.requested else {
            return Err("--require-served-pipeline needs --pipeline".to_owned());
        };
        match &self.served {
            None => Err(format!(
                "requested pipeline {} but the host did not say what it serves",
                requested.token()
            )),
            Some(_) if self.degraded() => Err(format!(
                "requested pipeline {} but the host serves {} (reason: {})",
                requested.token(),
                self.served
                    .as_ref()
                    .map_or("unreported", ServedStreamPipeline::token),
                self.reason.as_deref().unwrap_or("unstated")
            )),
            Some(_) => Ok(()),
        }
    }
}

/// Checks that the colour the hello states is the served pipeline's: Grading
/// and HDR carry their contract's depth, chroma, range, matrix, primaries and
/// transfer (Grading's BT.709 transfer is also served as sRGB); Auto and
/// Speed never carry an HDR transfer. Software, custom and unknown pipelines
/// promise no colour.
///
/// # Errors
///
/// Names each axis that differs, or that the hello does not state for
/// Grading or HDR.
pub fn verify_served_colour(
    served: Option<&ServedStreamPipeline>,
    active: &ColourAxes,
) -> Result<(), String> {
    let Some(pipeline) = served.and_then(PipelineId::from_served_wire) else {
        return Ok(());
    };
    let hdr_transfer = |transfer: TransferCharacteristics| {
        matches!(
            transfer,
            TransferCharacteristics::Pq | TransferCharacteristics::Hlg
        )
    };
    let mut wrong = Vec::new();
    match pipeline {
        PipelineId::Grading | PipelineId::Hdr => {
            let contract = pipeline_contract(pipeline).colour;
            let mut axis = |name: &'static str, matches: Option<bool>| match matches {
                Some(true) => {}
                Some(false) => wrong.push(name.to_owned()),
                None => wrong.push(format!("{name} (not stated)")),
            };
            axis(
                "chroma",
                active.chroma.map(|chroma| chroma == contract.chroma),
            );
            axis(
                "bit depth",
                active.bit_depth.map(|depth| depth == contract.bit_depth),
            );
            axis("range", active.range.map(|range| range == contract.range));
            axis(
                "matrix",
                active.matrix.map(|matrix| matrix == contract.matrix),
            );
            axis(
                "primaries",
                active
                    .primaries
                    .map(|primaries| primaries == contract.primaries),
            );
            axis(
                "transfer",
                active
                    .transfer
                    .map(|transfer| transfer.serves_request(contract.transfer)),
            );
        }
        PipelineId::Auto | PipelineId::Speed => {
            if active.transfer.is_some_and(hdr_transfer) {
                wrong.push("transfer".to_owned());
            }
        }
        PipelineId::Software => {}
    }
    if wrong.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "served pipeline {} does not match the hello's colour ({}): {}",
            pipeline.served_wire().token(),
            describe(*active),
            wrong.join(", ")
        ))
    }
}

/// Checks one decoded frame's header against the colour the hello states:
/// bit depth, range and matrix. (Codec and chroma are checked against each
/// monitor's own media plan; primaries and transfer ride in no header.)
///
/// # Errors
///
/// When the header holds a reserved colour value or disagrees with the
/// hello.
pub fn verify_frame_colour(
    monitor_id: u16,
    active: &ColourAxes,
    header: &VideoHeader,
) -> Result<(), String> {
    // The codec is not compared here; any value labels the configuration.
    let frame = crate::decode::header_video_configuration(
        header,
        VideoCodec::H264,
        crate::decode::SessionColor::default(),
    )
    .map_err(|error| format!("monitor {monitor_id}: frame header colour: {error:?}"))?;
    let mut wrong = Vec::new();
    if active
        .bit_depth
        .is_some_and(|depth| depth != frame.bit_depth)
    {
        wrong.push(format!("bit depth {}", frame.bit_depth.token()));
    }
    if active.range.is_some_and(|range| range != frame.range) {
        wrong.push(format!("range {}", frame.range.token()));
    }
    if active.matrix.is_some_and(|matrix| matrix != frame.matrix) {
        wrong.push(format!("matrix {}", frame.matrix.token()));
    }
    if wrong.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "monitor {monitor_id}: decoded frame header disagrees with the hello ({}): {}",
            describe(*active),
            wrong.join(", ")
        ))
    }
}

fn describe(active: ColourAxes) -> String {
    fn token<T>(value: Option<T>, name: fn(T) -> &'static str) -> &'static str {
        value.map_or("?", name)
    }
    format!(
        "{} {} {} {} {} {}",
        active.chroma.map_or("?", |chroma| match chroma {
            arcen_media::ChromaSubsampling::Yuv420 => "yuv420",
            arcen_media::ChromaSubsampling::Yuv422 => "yuv422",
            arcen_media::ChromaSubsampling::Yuv444 => "yuv444",
        }),
        token(active.bit_depth, arcen_media::BitDepth::token),
        token(active.range, arcen_media::ColorRange::token),
        token(active.matrix, arcen_media::ColorMatrix::token),
        token(active.primaries, arcen_media::ColorPrimaries::token),
        token(active.transfer, TransferCharacteristics::token),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_media::{
        BitDepth, ChromaSubsampling, ColorMatrix, ColorPrimaries, ColorRange, Monitor,
        MonitorIdentity, RequestedMonitor, Rotation,
    };
    use arcen_protocol::messages::{
        AppliedMonitorDescriptorMsg, AppliedMonitorMediaPlanMsg, AppliedMonitorTopologyMsg,
        ClientDisplayId, CursorMode, MultiMonitorCarrierMsg, RotationMsg, TopologyBackendKindMsg,
    };

    fn wanted(id: &str, x: i32, primary: bool) -> RequestedMonitor {
        RequestedMonitor::new(
            Monitor {
                identity: MonitorIdentity {
                    id: id.to_owned(),
                    name: format!("Desk {id}"),
                    ..MonitorIdentity::default()
                },
                x,
                y: 0,
                width_px: 1920,
                height_px: 1080,
                scale: 1.0,
                refresh_hz: 60,
                rotation: Rotation::Degrees0,
                primary,
                width_mm: 0.0,
                height_mm: 0.0,
                color: None,
            },
            1920,
            1080,
        )
        .expect("requested monitor")
    }

    fn requested(count: usize) -> RequestedMonitorTopology {
        RequestedMonitorTopology::new(
            (0..count)
                .map(|index| {
                    let x = i32::try_from(index).expect("index") * 1920;
                    wanted(&format!("d{}", index + 1), x, index == 0)
                })
                .collect(),
        )
        .expect("requested")
    }

    fn shown(id: &str, monitor: u16, x: i32) -> AppliedMonitorDescriptorMsg {
        AppliedMonitorDescriptorMsg {
            client_display_id: ClientDisplayId::new(id.to_owned()).expect("id"),
            session_monitor_id: monitor,
            x,
            y: 0,
            width_px: 1920,
            height_px: 1080,
            refresh_hz: 60,
            rotation: RotationMsg::Degrees0,
            is_primary: monitor == 1,
            media_plan: AppliedMonitorMediaPlanMsg {
                stream_epoch: 1,
                encoder_backend: "native-nvenc".to_owned(),
                encoder_class: "hardware".to_owned(),
                codec: "h265".to_owned(),
                chroma: "yuv444".to_owned(),
                width_px: 1920,
                height_px: 1080,
                fps: 60,
                bitrate_kbps: 20_000,
                cursor_mode: CursorMode::Local,
                degraded: false,
                degradation_reason: String::new(),
            },
        }
    }

    fn capability(
        shown: Vec<AppliedMonitorDescriptorMsg>,
        max_monitors: u8,
        fixed_topology: bool,
    ) -> ServerMultiMonitorMsg {
        let width = u32::try_from(shown.len()).expect("count") * 1920;
        let applied = AppliedMonitorTopologyMsg::new(
            1,
            0,
            0,
            width,
            1080,
            0,
            0,
            MultiMonitorCarrierMsg::MuxedReliableStream,
            shown,
        )
        .expect("applied");
        ServerMultiMonitorMsg::new(
            max_monitors,
            vec![RotationMsg::Degrees0],
            fixed_topology,
            TopologyBackendKindMsg::VirtualOutputs,
            vec![MultiMonitorCarrierMsg::MuxedReliableStream],
            Some(applied),
        )
        .expect("capability")
    }

    #[test]
    fn a_full_layout_passes_by_default() {
        let layout = verify_applied_layout(
            &requested(2),
            &capability(vec![shown("d1", 1, 0), shown("d2", 2, 1920)], 4, false),
            LayoutAcceptance::FullLayout,
        )
        .expect("full layout");
        assert_eq!(layout.served, vec![1, 2]);
        assert!(layout.omitted.is_empty());
        assert_eq!(layout.reason, None);
    }

    #[test]
    fn an_omitted_display_fails_by_default_and_is_named() {
        let error = verify_applied_layout(
            &requested(3),
            &capability(vec![shown("d1", 1, 0), shown("d2", 2, 1920)], 2, false),
            LayoutAcceptance::FullLayout,
        )
        .expect_err("a display is missing");
        assert!(error.contains("host served 2 of 3 displays"), "{error}");
        assert!(error.contains("display 3 (d3 \"Desk d3\")"), "{error}");
        assert!(error.contains("host_max_monitors=2"), "{error}");
        assert!(error.contains("--accept-host-layout"), "{error}");
    }

    #[test]
    fn an_accepted_host_layout_names_the_omitted_displays() {
        let layout = verify_applied_layout(
            &requested(2),
            &capability(vec![shown("d1", 1, 0)], 4, true),
            LayoutAcceptance::HostLayout,
        )
        .expect("host layout accepted");
        assert_eq!(layout.served, vec![1]);
        assert_eq!(
            layout.omitted,
            vec![OmittedDisplay {
                index: 2,
                client_display_id: "d2".to_owned(),
                name: "Desk d2".to_owned(),
            }]
        );
        assert_eq!(
            layout.omitted_lines(),
            vec![
                "multi-monitor omitted display=2 id=d2 name=\"Desk d2\" reason=host_layout_kept"
                    .to_owned()
            ]
        );
    }

    #[test]
    fn a_monitor_the_deck_did_not_request_fails_even_with_the_host_layout() {
        let error = verify_applied_layout(
            &requested(2),
            &capability(vec![shown("d1", 1, 0), shown("other", 2, 1920)], 4, true),
            LayoutAcceptance::HostLayout,
        )
        .expect_err("an unrequested monitor");
        assert!(error.contains("did not request"), "{error}");
    }

    fn served_msg(served: ServedStreamPipeline, reason: Option<&str>) -> ServedPipelineMsg {
        ServedPipelineMsg::new(served, "hardware", reason.map(str::to_owned))
            .with_detail("The displays offer no HDR.")
    }

    #[test]
    fn a_degraded_pipeline_is_reported_with_the_hosts_reason() {
        let mut report = PipelineReport {
            requested: Some(StreamPipeline::Hdr),
            served: Some(ServedStreamPipeline::Grading),
            ..PipelineReport::default()
        };
        report.apply_served(&served_msg(
            ServedStreamPipeline::Grading,
            Some("hdr_display_unsupported"),
        ));
        assert!(report.degraded());
        assert_eq!(
            report.line(),
            "pipeline requested=hdr served=grading degraded=true \
             reason=hdr_display_unsupported detail=\"The displays offer no HDR.\""
        );
        let error = report.require_requested().expect_err("degraded");
        assert!(error.contains("hdr_display_unsupported"), "{error}");
    }

    #[test]
    fn the_requested_pipeline_served_passes_a_strict_run() {
        let report = PipelineReport {
            requested: Some(StreamPipeline::Grading),
            served: Some(ServedStreamPipeline::Grading),
            ..PipelineReport::default()
        };
        assert!(!report.degraded());
        assert_eq!(report.require_requested(), Ok(()));
        let unreported = PipelineReport {
            served: None,
            ..report
        };
        assert!(unreported.require_requested().is_err());
    }

    fn axes(video: arcen_media::VideoConfiguration) -> ColourAxes {
        ColourAxes::from_configuration(video)
    }

    fn hdr10() -> arcen_media::VideoConfiguration {
        pipeline_contract(PipelineId::Hdr).colour
    }

    fn grading() -> arcen_media::VideoConfiguration {
        pipeline_contract(PipelineId::Grading).colour
    }

    #[test]
    fn served_hdr_needs_the_hdr_contracts_colour() {
        assert_eq!(
            verify_served_colour(Some(&ServedStreamPipeline::Hdr), &axes(hdr10())),
            Ok(())
        );
        let error = verify_served_colour(Some(&ServedStreamPipeline::Hdr), &axes(grading()))
            .expect_err("Grading colour is not HDR");
        assert!(error.contains("matrix"), "{error}");
        assert!(error.contains("primaries"), "{error}");
        assert!(error.contains("transfer"), "{error}");
    }

    #[test]
    fn served_grading_accepts_srgb_and_refuses_an_unstated_transfer() {
        let mut srgb = grading();
        srgb.transfer = TransferCharacteristics::Srgb;
        assert_eq!(
            verify_served_colour(Some(&ServedStreamPipeline::Grading), &axes(srgb)),
            Ok(())
        );
        let mut unstated = axes(grading());
        unstated.transfer = None;
        let error = verify_served_colour(Some(&ServedStreamPipeline::Grading), &unstated)
            .expect_err("transfer unknown");
        assert!(error.contains("transfer (not stated)"), "{error}");
    }

    #[test]
    fn auto_never_carries_an_hdr_transfer_and_custom_promises_nothing() {
        let mut sdr = axes(grading());
        sdr.bit_depth = Some(BitDepth::Eight);
        assert_eq!(
            verify_served_colour(Some(&ServedStreamPipeline::Auto), &sdr),
            Ok(())
        );
        assert!(verify_served_colour(Some(&ServedStreamPipeline::Speed), &axes(hdr10())).is_err());
        assert_eq!(
            verify_served_colour(Some(&ServedStreamPipeline::Custom), &axes(hdr10())),
            Ok(())
        );
        assert_eq!(verify_served_colour(None, &axes(hdr10())), Ok(()));
    }

    fn header(
        depth: arcen_protocol::wire::BitDepth,
        range: arcen_protocol::wire::ColorRange,
        matrix: arcen_protocol::wire::ColorMatrix,
    ) -> VideoHeader {
        VideoHeader {
            frame_type: arcen_protocol::wire::FrameType::VideoH265,
            codec: arcen_protocol::wire::VideoCodec::H265,
            chroma: arcen_protocol::ChromaSubsampling::Yuv444,
            flags: VideoHeader::encode_flags(true, depth, range, matrix),
            timestamp_ms: 0,
            monitor_id: 1,
            topology_generation: 0,
            stream_epoch: 0,
        }
    }

    #[test]
    fn a_frame_header_must_agree_with_the_hello() {
        use arcen_protocol::wire;
        let hello = axes(hdr10());
        assert_eq!(
            verify_frame_colour(
                1,
                &hello,
                &header(
                    wire::BitDepth::Ten,
                    wire::ColorRange::Full,
                    wire::ColorMatrix::Bt2020Ncl
                )
            ),
            Ok(())
        );
        let error = verify_frame_colour(
            2,
            &hello,
            &header(
                wire::BitDepth::Ten,
                wire::ColorRange::Full,
                wire::ColorMatrix::Bt709,
            ),
        )
        .expect_err("BT.709 frames under a BT.2020 hello");
        assert!(error.starts_with("monitor 2:"), "{error}");
        assert!(error.contains("matrix bt709"), "{error}");
        let eight = verify_frame_colour(
            1,
            &axes(grading()),
            &header(
                wire::BitDepth::Eight,
                wire::ColorRange::Limited,
                wire::ColorMatrix::Bt709,
            ),
        )
        .expect_err("eight-bit limited frames under a Grading hello");
        assert!(eight.contains("bit depth 8"), "{eight}");
        assert!(eight.contains("range limited"), "{eight}");
    }

    #[test]
    fn an_unstated_hello_axis_is_not_compared() {
        use arcen_protocol::wire;
        let hello = ColourAxes {
            chroma: Some(ChromaSubsampling::Yuv420),
            range: Some(ColorRange::Limited),
            matrix: Some(ColorMatrix::Bt709),
            primaries: Some(ColorPrimaries::Bt709),
            ..ColourAxes::default()
        };
        assert_eq!(
            verify_frame_colour(
                1,
                &hello,
                &header(
                    wire::BitDepth::Ten,
                    wire::ColorRange::Limited,
                    wire::ColorMatrix::Bt709
                )
            ),
            Ok(())
        );
    }
}
