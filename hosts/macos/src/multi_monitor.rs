//! macOS host-side `multi_monitor_v1` planning and wire metadata.
//!
//! The native work here is deliberately small: enumerate already-attached
//! displays, assign one capture stream to each admitted monitor, and publish
//! the applied topology before any region frame is sent. Topology negotiation,
//! monitor-id validity, media rosters and wire validation stay in the shared
//! crates.

use arcen_protocol::messages::{
    AppliedMonitorDescriptorMsg, AppliedMonitorMediaPlanMsg, AppliedMonitorTopologyMsg,
    AuthMultiMonitorOfferMsg, AuthMultiMonitorRequestMsg, AuthRequest,
    AuthRequestMultiMonitorOfferError, ClientDisplayId, MultiMonitorCarrierMsg,
    MultiMonitorValidationError, RequestedMonitorDescriptorMsg, RotationMsg, ServerMultiMonitorMsg,
    TopologyBackendKindMsg,
};

use crate::displays::DisplaySnapshot;
use crate::{CAN_CAPTURE_MULTIPLE_DISPLAYS, MacOsMultiMonitorConfig, may_advertise_multi_monitor};

const OFFERED_CARRIERS: [MultiMonitorCarrierMsg; 1] = [MultiMonitorCarrierMsg::MuxedReliableStream];
const SUPPORTED_ROTATIONS: [RotationMsg; 1] = [RotationMsg::Degrees0];
const REGION_INPUT_ROUTING_AVAILABLE: bool = false;

/// One display admitted into a multi-monitor session.
#[derive(Debug, Clone, PartialEq)]
pub struct MacOsMonitorPlan {
    pub display: DisplaySnapshot,
    pub client_display_id: ClientDisplayId,
    pub session_monitor_id: arcen_media::SessionMonitorId,
    pub x: i32,
    pub y: i32,
    pub width_px: u32,
    pub height_px: u32,
    pub refresh_hz: u32,
    pub is_primary: bool,
}

/// The committed multi-monitor topology and media roster for one session.
#[derive(Debug, Clone, PartialEq)]
pub struct MacOsMultiMonitorPlan {
    pub generation: arcen_media::TopologyGeneration,
    pub carrier: MultiMonitorCarrierMsg,
    pub monitors: Vec<MacOsMonitorPlan>,
    pub media_roster: arcen_media::RegionMediaRoster,
    pub server_capability: ServerMultiMonitorMsg,
    pub desktop_x: i32,
    pub desktop_y: i32,
    pub desktop_width_px: u32,
    pub desktop_height_px: u32,
}

/// Why a requested topology could not be admitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MacOsMultiMonitorError {
    NotAdvertised,
    Offer(MultiMonitorValidationError),
    UnsupportedRotation {
        client_display_id: String,
        rotation: RotationMsg,
    },
    TooFewLocalDisplays {
        requested: usize,
        available: usize,
    },
    CoordinateOverflow(&'static str),
    InvalidMedia(arcen_media::MediaContractError),
    InvalidWire(MultiMonitorValidationError),
}

/// Why this host did not publish a pre-auth `multi_monitor_v1` offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultiMonitorOfferWithheld {
    OperatorDisabled,
    CaptureUnavailable,
    SingleDisplay,
    RegionInputUnavailable,
}

impl std::fmt::Display for MultiMonitorOfferWithheld {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OperatorDisabled => {
                formatter.write_str("platform.multi_monitor.advertise_enabled is false")
            }
            Self::CaptureUnavailable => {
                formatter.write_str("this build cannot capture multiple displays concurrently")
            }
            Self::SingleDisplay => {
                formatter.write_str("WindowServer reported fewer than two displays")
            }
            Self::RegionInputUnavailable => formatter.write_str(
                "region input routing is not implemented, so a Deck would reject Match My Layout",
            ),
        }
    }
}

impl std::fmt::Display for MacOsMultiMonitorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAdvertised => {
                formatter.write_str("host did not advertise multi-monitor support")
            }
            Self::Offer(error) => {
                write!(formatter, "requested topology exceeds the offer: {error}")
            }
            Self::UnsupportedRotation {
                client_display_id,
                rotation,
            } => write!(
                formatter,
                "requested monitor {client_display_id} uses unsupported rotation {rotation}"
            ),
            Self::TooFewLocalDisplays {
                requested,
                available,
            } => write!(
                formatter,
                "requested {requested} monitors but this Mac has {available} capturable displays"
            ),
            Self::CoordinateOverflow(context) => write!(formatter, "{context} overflowed"),
            Self::InvalidMedia(error) => write!(formatter, "invalid media plan: {error}"),
            Self::InvalidWire(error) => write!(formatter, "invalid applied topology: {error}"),
        }
    }
}

impl std::error::Error for MacOsMultiMonitorError {}

impl From<arcen_media::MediaContractError> for MacOsMultiMonitorError {
    fn from(error: arcen_media::MediaContractError) -> Self {
        Self::InvalidMedia(error)
    }
}

impl From<MultiMonitorValidationError> for MacOsMultiMonitorError {
    fn from(error: MultiMonitorValidationError) -> Self {
        Self::InvalidWire(error)
    }
}

/// Builds this connection's pre-auth `multi_monitor_v1` offer.
#[must_use]
pub fn build_offer(
    config: &MacOsMultiMonitorConfig,
    displays: &[DisplaySnapshot],
) -> Option<AuthMultiMonitorOfferMsg> {
    build_offer_with_reason(config, displays).ok()
}

/// Builds this connection's offer or returns the explicit reason it was withheld.
///
/// Region input cannot be faked. Match My Layout sends absolute input against
/// the combined desktop and the host must route each event to the owning
/// region, validate that monitor id/topology generation/coordinates match the
/// admitted topology, and reject stale or out-of-bounds region events before
/// injection. The current macOS dispatcher receives only desktop coordinates,
/// so advertising multi-monitor would make a real Deck refuse the session.
pub fn build_offer_with_reason(
    config: &MacOsMultiMonitorConfig,
    displays: &[DisplaySnapshot],
) -> Result<AuthMultiMonitorOfferMsg, MultiMonitorOfferWithheld> {
    if !config.advertise_enabled {
        return Err(MultiMonitorOfferWithheld::OperatorDisabled);
    }
    if !CAN_CAPTURE_MULTIPLE_DISPLAYS {
        return Err(MultiMonitorOfferWithheld::CaptureUnavailable);
    }
    if displays.len() <= 1 {
        return Err(MultiMonitorOfferWithheld::SingleDisplay);
    }
    if !REGION_INPUT_ROUTING_AVAILABLE {
        return Err(MultiMonitorOfferWithheld::RegionInputUnavailable);
    }
    if !may_advertise_multi_monitor(
        config,
        displays.len(),
        CAN_CAPTURE_MULTIPLE_DISPLAYS,
        REGION_INPUT_ROUTING_AVAILABLE,
    ) {
        return Err(MultiMonitorOfferWithheld::CaptureUnavailable);
    }
    let protocol_ceiling = u8::try_from(arcen_media::MAX_MULTI_MONITOR_COUNT)
        .map_err(|_| MultiMonitorOfferWithheld::CaptureUnavailable)?;
    let capture_ceiling = u8::try_from(crate::multi_capture::MAX_CAPTURED_DISPLAYS)
        .map_err(|_| MultiMonitorOfferWithheld::CaptureUnavailable)?;
    let attached =
        u8::try_from(displays.len()).map_err(|_| MultiMonitorOfferWithheld::CaptureUnavailable)?;
    let max_monitors = config
        .max_monitors
        .unwrap_or(protocol_ceiling)
        .min(protocol_ceiling)
        .min(capture_ceiling)
        .min(attached);
    AuthMultiMonitorOfferMsg::new(
        max_monitors,
        SUPPORTED_ROTATIONS.to_vec(),
        OFFERED_CARRIERS.to_vec(),
    )
    .map_err(|_| MultiMonitorOfferWithheld::CaptureUnavailable)
}

/// Admits an auth-time request against the exact offer sent on this connection.
///
/// # Errors
///
/// Returns [`MacOsMultiMonitorError`] when the request was sent without an
/// offer, exceeds that offer, cannot map onto attached displays, or cannot
/// produce a valid applied topology and media roster.
pub fn admit_request(
    displays: &[DisplaySnapshot],
    offer: Option<&AuthMultiMonitorOfferMsg>,
    request: Option<&AuthMultiMonitorRequestMsg>,
    video: arcen_media::session_plan::ResolvedVideoPlan,
    codec: crate::encode::EncoderCodec,
    fps: u32,
) -> Result<Option<MacOsMultiMonitorPlan>, MacOsMultiMonitorError> {
    let Some(request) = request else {
        return Ok(None);
    };
    let Some(offer) = offer else {
        return Err(MacOsMultiMonitorError::NotAdvertised);
    };
    let wrapper = offer_wrapper(offer);
    let advertised = wrapper
        .required_multi_monitor_v1_offer()
        .map_err(|error| match error {
            AuthRequestMultiMonitorOfferError::Missing => MacOsMultiMonitorError::NotAdvertised,
            AuthRequestMultiMonitorOfferError::Invalid(error) => {
                MacOsMultiMonitorError::Offer(error)
            }
        })?;
    advertised
        .validate_request(request)
        .map_err(MacOsMultiMonitorError::Offer)?;
    let carrier = advertised
        .select_carrier(request, &OFFERED_CARRIERS)
        .map_err(MacOsMultiMonitorError::Offer)?;
    plan_request(displays, offer, request, carrier, video, codec, fps).map(Some)
}

fn offer_wrapper(offer: &AuthMultiMonitorOfferMsg) -> AuthRequest {
    AuthRequest {
        msg_type: String::new(),
        auth_methods: Vec::new(),
        challenge: String::new(),
        salt: String::new(),
        auth_mode: None,
        disclaimer: None,
        multi_monitor_v1: Some(offer.clone()),
    }
}

fn plan_request(
    displays: &[DisplaySnapshot],
    offer: &AuthMultiMonitorOfferMsg,
    request: &AuthMultiMonitorRequestMsg,
    carrier: MultiMonitorCarrierMsg,
    video: arcen_media::session_plan::ResolvedVideoPlan,
    codec: crate::encode::EncoderCodec,
    fps: u32,
) -> Result<MacOsMultiMonitorPlan, MacOsMultiMonitorError> {
    let requested = ordered_requested_monitors(request);
    if requested.len() > displays.len() {
        return Err(MacOsMultiMonitorError::TooFewLocalDisplays {
            requested: requested.len(),
            available: displays.len(),
        });
    }
    let generation = arcen_media::TopologyGeneration::FIRST;
    let monitors = plan_monitors(displays, &requested)?;
    let media_roster = media_roster(&monitors, generation, video, codec, fps)?;
    let server_capability =
        server_capability(offer, carrier, generation, &monitors, &media_roster, video)?;
    let bounds = desktop_bounds(&monitors)?;
    Ok(MacOsMultiMonitorPlan {
        generation,
        carrier,
        monitors,
        media_roster,
        server_capability,
        desktop_x: bounds.x,
        desktop_y: bounds.y,
        desktop_width_px: bounds.width,
        desktop_height_px: bounds.height,
    })
}

fn ordered_requested_monitors(
    request: &AuthMultiMonitorRequestMsg,
) -> Vec<&RequestedMonitorDescriptorMsg> {
    let topology = request.requested_topology();
    let mut ordered = Vec::with_capacity(topology.monitors().len());
    ordered.push(topology.primary());
    for monitor in topology.monitors() {
        if monitor.client_display_id != topology.primary().client_display_id {
            ordered.push(monitor);
        }
    }
    ordered
}

fn plan_monitors(
    displays: &[DisplaySnapshot],
    requested: &[&RequestedMonitorDescriptorMsg],
) -> Result<Vec<MacOsMonitorPlan>, MacOsMultiMonitorError> {
    let mut monitors = Vec::with_capacity(requested.len());
    for (index, requested_monitor) in requested.iter().enumerate() {
        if !SUPPORTED_ROTATIONS.contains(&requested_monitor.rotation) {
            return Err(MacOsMultiMonitorError::UnsupportedRotation {
                client_display_id: requested_monitor.client_display_id.as_str().to_owned(),
                rotation: requested_monitor.rotation,
            });
        }
        let display = displays[index];
        let session_monitor_id = arcen_media::SessionMonitorId::new(
            u16::try_from(index + 1)
                .map_err(|_| MacOsMultiMonitorError::CoordinateOverflow("monitor id"))?,
        )?;
        monitors.push(MacOsMonitorPlan {
            display,
            client_display_id: requested_monitor.client_display_id.clone(),
            session_monitor_id,
            x: display_axis_to_i32(display.origin_x, "display x")?,
            y: display_axis_to_i32(display.origin_y, "display y")?,
            width_px: u32::try_from(display.pixel_width)
                .map_err(|_| MacOsMultiMonitorError::CoordinateOverflow("display width"))?,
            height_px: u32::try_from(display.pixel_height)
                .map_err(|_| MacOsMultiMonitorError::CoordinateOverflow("display height"))?,
            refresh_hz: requested_monitor.refresh_hz.max(1),
            is_primary: index == 0,
        });
    }
    Ok(monitors)
}

fn display_axis_to_i32(value: f64, context: &'static str) -> Result<i32, MacOsMultiMonitorError> {
    if !value.is_finite() || value < f64::from(i32::MIN) || value > f64::from(i32::MAX) {
        return Err(MacOsMultiMonitorError::CoordinateOverflow(context));
    }
    #[allow(clippy::cast_possible_truncation)]
    Ok(value.round() as i32)
}

fn media_roster(
    monitors: &[MacOsMonitorPlan],
    generation: arcen_media::TopologyGeneration,
    video: arcen_media::session_plan::ResolvedVideoPlan,
    codec: crate::encode::EncoderCodec,
    fps: u32,
) -> Result<arcen_media::RegionMediaRoster, MacOsMultiMonitorError> {
    let epoch = arcen_media::MediaStreamEpoch::new(generation.get())?;
    let plans = monitors
        .iter()
        .map(|monitor| {
            let bitrate_budget =
                encoder_bitrate_budget(monitor.width_px, monitor.height_px, codec, fps)?;
            arcen_media::RegionMediaPlan::new(
                monitor.session_monitor_id,
                epoch,
                arcen_media::video::EncoderBackend::VideoToolbox,
                video_configuration(video, codec),
                monitor.width_px,
                monitor.height_px,
                fps,
                bitrate_budget,
            )
            .map_err(MacOsMultiMonitorError::InvalidMedia)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(arcen_media::RegionMediaRoster::new(plans)?)
}

fn video_configuration(
    plan: arcen_media::session_plan::ResolvedVideoPlan,
    codec: crate::encode::EncoderCodec,
) -> arcen_media::VideoConfiguration {
    arcen_media::VideoConfiguration {
        codec: match codec {
            crate::encode::EncoderCodec::H264 => arcen_media::VideoCodec::H264,
            crate::encode::EncoderCodec::Hevc => arcen_media::VideoCodec::H265,
        },
        chroma: match plan.chroma {
            arcen_media::session_plan::PlanChroma::Yuv420 => arcen_media::ChromaSubsampling::Yuv420,
            arcen_media::session_plan::PlanChroma::Yuv444 => arcen_media::ChromaSubsampling::Yuv444,
        },
        bit_depth: match plan.bit_depth {
            arcen_media::session_plan::PlanBitDepth::Eight => arcen_media::BitDepth::Eight,
            arcen_media::session_plan::PlanBitDepth::Ten => arcen_media::BitDepth::Ten,
        },
        range: arcen_media::ColorRange::Limited,
        matrix: arcen_media::ColorMatrix::Bt709,
        primaries: arcen_media::ColorPrimaries::Bt709,
        transfer: arcen_media::TransferCharacteristics::Bt709,
    }
}

fn encoder_bitrate_budget(
    width: u32,
    height: u32,
    codec: crate::encode::EncoderCodec,
    fps: u32,
) -> Result<arcen_media::BitrateBudgetKbps, MacOsMultiMonitorError> {
    let width = i32::try_from(width)
        .map_err(|_| MacOsMultiMonitorError::CoordinateOverflow("media width"))?;
    let height = i32::try_from(height)
        .map_err(|_| MacOsMultiMonitorError::CoordinateOverflow("media height"))?;
    let kbps = u32::try_from(
        crate::encode::EncoderConfig::realtime(width, height, codec, fps).bitrate_bps / 1_000,
    )
    .map_err(|_| MacOsMultiMonitorError::CoordinateOverflow("media bitrate"))?;
    arcen_media::BitrateBudgetKbps::new(kbps).map_err(MacOsMultiMonitorError::InvalidMedia)
}

fn desktop_bounds(
    monitors: &[MacOsMonitorPlan],
) -> Result<arcen_media::LayoutBounds, MacOsMultiMonitorError> {
    let rects = monitors
        .iter()
        .map(|monitor| {
            arcen_media::LayoutRect::new(monitor.x, monitor.y, monitor.width_px, monitor.height_px)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(arcen_media::LayoutBounds::from_rects(&rects)?)
}

fn server_capability(
    offer: &AuthMultiMonitorOfferMsg,
    carrier: MultiMonitorCarrierMsg,
    generation: arcen_media::TopologyGeneration,
    monitors: &[MacOsMonitorPlan],
    media_roster: &arcen_media::RegionMediaRoster,
    video: arcen_media::session_plan::ResolvedVideoPlan,
) -> Result<ServerMultiMonitorMsg, MacOsMultiMonitorError> {
    let bounds = desktop_bounds(monitors)?;
    let translation = bounds.translation_to_origin();
    let translated = bounds.translated(translation)?;
    let descriptors = applied_descriptors(monitors, media_roster, translation, video)?;
    let applied = AppliedMonitorTopologyMsg::new(
        generation.get(),
        translated.x,
        translated.y,
        translated.width,
        translated.height,
        translation.dx,
        translation.dy,
        carrier,
        descriptors,
    )?;
    Ok(ServerMultiMonitorMsg::new(
        offer.max_monitors(),
        offer.supported_rotations().to_vec(),
        true,
        TopologyBackendKindMsg::PhysicalOutputs,
        OFFERED_CARRIERS.to_vec(),
        Some(applied),
    )?)
}

fn applied_descriptors(
    monitors: &[MacOsMonitorPlan],
    media_roster: &arcen_media::RegionMediaRoster,
    translation: arcen_media::LayoutTranslation,
    video: arcen_media::session_plan::ResolvedVideoPlan,
) -> Result<Vec<AppliedMonitorDescriptorMsg>, MacOsMultiMonitorError> {
    monitors
        .iter()
        .map(|monitor| applied_descriptor(monitor, media_roster, translation, video))
        .collect()
}

fn applied_descriptor(
    monitor: &MacOsMonitorPlan,
    media_roster: &arcen_media::RegionMediaRoster,
    translation: arcen_media::LayoutTranslation,
    video: arcen_media::session_plan::ResolvedVideoPlan,
) -> Result<AppliedMonitorDescriptorMsg, MacOsMultiMonitorError> {
    let media = media_roster.plan(monitor.session_monitor_id).ok_or(
        MacOsMultiMonitorError::InvalidMedia(
            arcen_media::MediaContractError::DuplicateSessionMonitorId(
                monitor.session_monitor_id.get(),
            ),
        ),
    )?;
    let x = checked_add_axis(monitor.x, translation.dx, "monitor x")?;
    let y = checked_add_axis(monitor.y, translation.dy, "monitor y")?;
    Ok(AppliedMonitorDescriptorMsg {
        client_display_id: monitor.client_display_id.clone(),
        session_monitor_id: monitor.session_monitor_id.get(),
        x,
        y,
        width_px: monitor.width_px,
        height_px: monitor.height_px,
        refresh_hz: monitor.refresh_hz,
        rotation: RotationMsg::Degrees0,
        is_primary: monitor.is_primary,
        media_plan: AppliedMonitorMediaPlanMsg {
            stream_epoch: media.stream_epoch.get(),
            encoder_backend: "videotoolbox".to_owned(),
            encoder_class: String::new(),
            codec: media.video.codec.token().to_owned(),
            chroma: match video.chroma {
                arcen_media::session_plan::PlanChroma::Yuv420 => "yuv420",
                arcen_media::session_plan::PlanChroma::Yuv444 => "yuv444",
            }
            .to_owned(),
            width_px: media.width,
            height_px: media.height,
            fps: media.fps,
            bitrate_kbps: media.applied_bitrate_kbps(),
            cursor_mode: arcen_protocol::messages::CursorMode::Local,
            degraded: !video.is_exact(),
        },
    })
}

fn checked_add_axis(
    axis: i32,
    shift: i64,
    context: &'static str,
) -> Result<i32, MacOsMultiMonitorError> {
    let value = i64::from(axis)
        .checked_add(shift)
        .ok_or(MacOsMultiMonitorError::CoordinateOverflow(context))?;
    i32::try_from(value).map_err(|_| MacOsMultiMonitorError::CoordinateOverflow(context))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn display(id: u32, x: f64, y: f64) -> DisplaySnapshot {
        DisplaySnapshot {
            display_id: id,
            pixel_width: 1920,
            pixel_height: 1080,
            origin_x: x,
            origin_y: y,
        }
    }

    fn requested(id: &str, legacy: u32, primary: bool) -> RequestedMonitorDescriptorMsg {
        RequestedMonitorDescriptorMsg {
            client_display_id: ClientDisplayId::new(id).expect("display id"),
            client_monitor_id: legacy,
            x: if primary { 0 } else { 1920 },
            y: 0,
            width_px: 1920,
            height_px: 1080,
            logical_width: 1920,
            logical_height: 1080,
            scale: 1.0,
            refresh_hz: 60,
            rotation: RotationMsg::Degrees0,
            is_primary: primary,
            name: id.to_owned(),
            width_mm: 0.0,
            height_mm: 0.0,
            vendor: 0,
            model: 0,
            serial: 0,
            edid: String::new(),
            color: None,
            safe_area_policy: arcen_protocol::messages::SafeAreaPolicyMsg::StandardFullscreen,
            quality_intent: arcen_protocol::messages::MonitorQualityIntentMsg::HostDefault,
        }
    }

    fn request() -> AuthMultiMonitorRequestMsg {
        let topology = arcen_protocol::messages::RequestedMonitorTopologyMsg::new(vec![
            requested("secondary", 2, false),
            requested("primary", 1, true),
        ])
        .expect("topology");
        AuthMultiMonitorRequestMsg::new(topology, OFFERED_CARRIERS.to_vec()).expect("request")
    }

    fn enabled_config() -> MacOsMultiMonitorConfig {
        MacOsMultiMonitorConfig {
            advertise_enabled: true,
            max_monitors: None,
        }
    }

    #[test]
    fn offer_is_withheld_until_region_input_routing_exists() {
        assert!(
            build_offer(
                &MacOsMultiMonitorConfig::default(),
                &[display(1, 0.0, 0.0), display(2, 1920.0, 0.0)]
            )
            .is_none()
        );
        assert!(build_offer(&enabled_config(), &[display(1, 0.0, 0.0)]).is_none());
        let error = build_offer_with_reason(
            &enabled_config(),
            &[display(1, 0.0, 0.0), display(2, 1920.0, 0.0)],
        )
        .expect_err("region input gate");
        assert_eq!(error, MultiMonitorOfferWithheld::RegionInputUnavailable);
    }

    #[test]
    fn admitted_plan_uses_nonzero_session_monitor_ids() {
        let displays = [display(10, 0.0, 0.0), display(11, 1920.0, 0.0)];
        let offer = AuthMultiMonitorOfferMsg::new(
            2,
            SUPPORTED_ROTATIONS.to_vec(),
            OFFERED_CARRIERS.to_vec(),
        )
        .expect("offer");
        let plan = admit_request(
            &displays,
            Some(&offer),
            Some(&request()),
            arcen_media::session_plan::ResolvedVideoPlan::standard(None),
            crate::encode::EncoderCodec::Hevc,
            60,
        )
        .expect("admission")
        .expect("planned");

        assert_eq!(plan.monitors[0].client_display_id.as_str(), "primary");
        assert_eq!(plan.monitors[0].session_monitor_id.get(), 1);
        assert_eq!(plan.monitors[1].session_monitor_id.get(), 2);
        let applied = plan.server_capability.applied_topology().expect("applied");
        assert_eq!(applied.primary().session_monitor_id, 1);
        for monitor in applied.monitors() {
            assert_ne!(monitor.session_monitor_id, 0);
            assert_eq!(monitor.media_plan.stream_epoch, plan.generation.get());
        }
    }

    #[test]
    fn a_missing_offer_refuses_the_sidecar_request() {
        let error = admit_request(
            &[display(10, 0.0, 0.0), display(11, 1920.0, 0.0)],
            None,
            Some(&request()),
            arcen_media::session_plan::ResolvedVideoPlan::standard(None),
            crate::encode::EncoderCodec::Hevc,
            60,
        )
        .expect_err("no offer");
        assert_eq!(error, MacOsMultiMonitorError::NotAdvertised);
    }

    #[test]
    fn applied_topology_normalises_negative_origins() {
        let displays = [display(10, -1920.0, 0.0), display(11, 0.0, 0.0)];
        let offer = AuthMultiMonitorOfferMsg::new(
            2,
            SUPPORTED_ROTATIONS.to_vec(),
            OFFERED_CARRIERS.to_vec(),
        )
        .expect("offer");
        let plan = admit_request(
            &displays,
            Some(&offer),
            Some(&request()),
            arcen_media::session_plan::ResolvedVideoPlan::standard(None),
            crate::encode::EncoderCodec::Hevc,
            60,
        )
        .expect("admission")
        .expect("planned");
        let applied = plan.server_capability.applied_topology().expect("applied");
        assert_eq!(applied.desktop_x(), 0);
        assert_eq!(applied.translation_x(), 1920);
        assert_eq!(applied.primary().x, 0);
    }

    #[test]
    fn region_media_matches_the_resolved_session_plan() {
        let displays = [display(10, 0.0, 0.0), display(11, 1920.0, 0.0)];
        let offer = AuthMultiMonitorOfferMsg::new(
            2,
            SUPPORTED_ROTATIONS.to_vec(),
            OFFERED_CARRIERS.to_vec(),
        )
        .expect("offer");
        let plan = admit_request(
            &displays,
            Some(&offer),
            Some(&request()),
            arcen_media::session_plan::ResolvedVideoPlan::standard(None),
            crate::encode::EncoderCodec::H264,
            30,
        )
        .expect("admission")
        .expect("planned");

        let media = plan.media_roster.plans()[0];
        assert_eq!(media.video.codec, arcen_media::VideoCodec::H264);
        assert_eq!(media.fps, 30);
        // Derived from the picture rather than fixed, so this states the
        // relationship instead of pinning a number that was only ever a guess.
        let expected = arcen_media::video::average_bitrate_bps(
            media.width,
            media.height,
            media.fps,
            arcen_media::ChromaSubsampling::Yuv420,
            arcen_media::BitDepth::Eight,
        ) / 1_000;
        assert_eq!(media.applied_bitrate_kbps(), expected);
        assert!(
            media.applied_bitrate_kbps() < 20_000,
            "a single monitor should not still be billed the old flat budget"
        );
        let applied = plan.server_capability.applied_topology().expect("applied");
        let primary = applied.primary();
        assert_eq!(primary.media_plan.codec, "h264");
        assert!(primary.media_plan.encoder_class.is_empty());
        assert_eq!(primary.media_plan.fps, 30);
        // What the Deck is told must be what the encoder was sized for; two
        // numbers here is how a client plans around a budget the host is not
        // actually encoding to.
        assert_eq!(primary.media_plan.bitrate_kbps, expected);
    }
}
