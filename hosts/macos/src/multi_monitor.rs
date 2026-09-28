//! macOS host-side `multi_monitor_v1` planning and wire metadata.
//!
//! The native work here is deliberately small: create one virtual display for
//! each Deck monitor, arrange those displays to mirror the Deck's requested
//! layout, assign one capture stream to each admitted monitor, and publish the
//! applied topology before any region frame is sent. Topology negotiation,
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
const REGION_INPUT_ROUTING_AVAILABLE: bool = true;
static NEXT_VIRTUAL_DISPLAY_SET: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(1);

/// One display admitted into a multi-monitor session.
#[derive(Debug, Clone, PartialEq)]
pub struct MacOsMonitorPlan {
    pub display: DisplaySnapshot,
    pub client_display_id: ClientDisplayId,
    pub requested_monitor: arcen_media::RequestedMonitor,
    pub session_monitor_id: arcen_media::SessionMonitorId,
    pub x: i32,
    pub y: i32,
    pub width_px: u32,
    pub height_px: u32,
    pub refresh_hz: u32,
    pub is_primary: bool,
}

/// The live virtual displays created for one multi-monitor session.
#[derive(Debug)]
pub struct MacOsVirtualDisplays {
    _child: crate::virtual_display::VirtualDisplayChild,
    len: usize,
}

impl MacOsVirtualDisplays {
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
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
    pub applied_regions: arcen_media::AppliedRegionSet,
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
    CoordinateOverflow(&'static str),
    InvalidMedia(arcen_media::MediaContractError),
    InvalidPlacement(arcen_media::TopologyPlacementError),
    InvalidRegion(arcen_media::RegionContractError),
    InvalidWire(MultiMonitorValidationError),
    VirtualDisplay {
        client_display_id: String,
        detail: String,
    },
    Arrangement(String),
    HdrNotProven {
        client_display_id: String,
        display_id: u32,
    },
}

/// Why this host did not publish a pre-auth `multi_monitor_v1` offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultiMonitorOfferWithheld {
    OperatorDisabled,
    CaptureUnavailable,
    VirtualDisplayUnavailable,
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
            Self::VirtualDisplayUnavailable => {
                formatter.write_str("this macOS did not expose virtual display creation")
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
            Self::CoordinateOverflow(context) => write!(formatter, "{context} overflowed"),
            Self::InvalidMedia(error) => write!(formatter, "invalid media plan: {error}"),
            Self::InvalidPlacement(error) => {
                write!(formatter, "invalid monitor placement: {error}")
            }
            Self::InvalidRegion(error) => {
                write!(formatter, "invalid region input topology: {error}")
            }
            Self::InvalidWire(error) => write!(formatter, "invalid applied topology: {error}"),
            Self::VirtualDisplay {
                client_display_id,
                detail,
            } => write!(
                formatter,
                "could not create an exact virtual display for {client_display_id}: {detail}"
            ),
            Self::Arrangement(detail) => {
                write!(formatter, "could not arrange virtual displays: {detail}")
            }
            Self::HdrNotProven {
                client_display_id,
                display_id,
            } => write!(
                formatter,
                "HDR requested for {client_display_id}, but virtual display {display_id} did not prove HDR headroom"
            ),
        }
    }
}

impl std::error::Error for MacOsMultiMonitorError {}

impl From<arcen_media::MediaContractError> for MacOsMultiMonitorError {
    fn from(error: arcen_media::MediaContractError) -> Self {
        Self::InvalidMedia(error)
    }
}

impl From<arcen_media::TopologyPlacementError> for MacOsMultiMonitorError {
    fn from(error: arcen_media::TopologyPlacementError) -> Self {
        Self::InvalidPlacement(error)
    }
}

impl From<arcen_media::RegionContractError> for MacOsMultiMonitorError {
    fn from(error: arcen_media::RegionContractError) -> Self {
        Self::InvalidRegion(error)
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
    if !REGION_INPUT_ROUTING_AVAILABLE {
        return Err(MultiMonitorOfferWithheld::RegionInputUnavailable);
    }
    if !crate::virtual_display::VirtualDisplay::is_supported() {
        return Err(MultiMonitorOfferWithheld::VirtualDisplayUnavailable);
    }
    if !may_advertise_multi_monitor(
        config,
        displays.len(),
        CAN_CAPTURE_MULTIPLE_DISPLAYS,
        REGION_INPUT_ROUTING_AVAILABLE,
        true,
    ) {
        return Err(MultiMonitorOfferWithheld::CaptureUnavailable);
    }
    let protocol_ceiling = u8::try_from(arcen_media::MAX_MULTI_MONITOR_COUNT)
        .map_err(|_| MultiMonitorOfferWithheld::CaptureUnavailable)?;
    let capture_ceiling = u8::try_from(crate::multi_capture::MAX_CAPTURED_DISPLAYS)
        .map_err(|_| MultiMonitorOfferWithheld::CaptureUnavailable)?;
    let max_monitors = config
        .max_monitors
        .unwrap_or(protocol_ceiling)
        .min(protocol_ceiling)
        .min(capture_ceiling);
    AuthMultiMonitorOfferMsg::new(
        max_monitors,
        SUPPORTED_ROTATIONS.to_vec(),
        OFFERED_CARRIERS.to_vec(),
    )
    .map_err(|_| MultiMonitorOfferWithheld::CaptureUnavailable)
}

/// Admits an auth-time request by creating one exact virtual display per Deck monitor.
///
/// # Errors
///
/// Returns [`MacOsMultiMonitorError`] when the request was sent without an
/// offer, exceeds that offer, cannot create every requested virtual display
/// exactly, cannot mirror the requested arrangement, or cannot produce valid
/// applied topology / media / region-input metadata. Any partial display set is
/// dropped before the error returns.
#[allow(clippy::too_many_arguments)]
pub fn admit_virtual_request(
    offer: Option<&AuthMultiMonitorOfferMsg>,
    request: Option<&AuthMultiMonitorRequestMsg>,
    video: arcen_media::session_plan::ResolvedVideoPlan,
    codec: crate::encode::EncoderCodec,
    fps: u32,
    pq_requested: bool,
    force_sdr_panel: bool,
) -> Result<Option<(MacOsMultiMonitorPlan, MacOsVirtualDisplays)>, MacOsMultiMonitorError> {
    let Some(request) = request else {
        return Ok(None);
    };
    let Some(offer) = offer else {
        return Err(MacOsMultiMonitorError::NotAdvertised);
    };
    let carrier = validate_request(offer, request)?;
    let specs = planned_virtual_specs(request, fps, pq_requested, force_sdr_panel)?;
    let (lease, displays) = create_virtual_displays(&specs)?;
    let plan = plan_from_virtual_displays(&specs, &displays, offer, carrier, video, codec, fps)?;
    Ok(Some((plan, lease)))
}

fn validate_request(
    offer: &AuthMultiMonitorOfferMsg,
    request: &AuthMultiMonitorRequestMsg,
) -> Result<MultiMonitorCarrierMsg, MacOsMultiMonitorError> {
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
    advertised
        .select_carrier(request, &OFFERED_CARRIERS)
        .map_err(MacOsMultiMonitorError::Offer)
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

#[derive(Debug, Clone, PartialEq)]
struct PlannedVirtualMonitor {
    requested_wire: RequestedMonitorDescriptorMsg,
    requested_monitor: arcen_media::RequestedMonitor,
    session_monitor_id: arcen_media::SessionMonitorId,
    layout_rect: arcen_media::LayoutRect,
    raw_layout_rect: arcen_media::LayoutRect,
    panel: crate::virtual_display::VirtualPanel,
    identity: crate::virtual_display::PanelIdentity,
    refresh_hz: u32,
}

impl PlannedVirtualMonitor {
    const fn raw_x(&self) -> i32 {
        self.raw_layout_rect.x
    }

    const fn raw_y(&self) -> i32 {
        self.raw_layout_rect.y
    }
}

fn planned_virtual_specs(
    request: &AuthMultiMonitorRequestMsg,
    fps: u32,
    pq_requested: bool,
    force_sdr_panel: bool,
) -> Result<Vec<PlannedVirtualMonitor>, MacOsMultiMonitorError> {
    let display_set = NEXT_VIRTUAL_DISPLAY_SET.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let requested = ordered_requested_monitors(request);
    let requested_monitors = requested
        .iter()
        .map(|monitor| {
            arcen_media::RequestedMonitor::try_from(*monitor)
                .map_err(MacOsMultiMonitorError::InvalidMedia)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let placed = arcen_media::place_monitors(
        &requested_monitors,
        0,
        arcen_media::TransformConvention::AlreadyCompositorOriented,
        arcen_media::OriginPolicy::PreserveSigned,
    )?;
    let raw_placed = requested_monitors
        .iter()
        .map(|monitor| monitor.logical_arrangement_rect())
        .collect::<Result<Vec<_>, _>>()?;
    requested
        .iter()
        .zip(requested_monitors)
        .zip(placed.rects())
        .zip(&raw_placed)
        .enumerate()
        .map(
            |(index, (((requested_wire, requested_monitor), rect), raw_rect))| {
                if !SUPPORTED_ROTATIONS.contains(&requested_wire.rotation) {
                    return Err(MacOsMultiMonitorError::UnsupportedRotation {
                        client_display_id: requested_wire.client_display_id.as_str().to_owned(),
                        rotation: requested_wire.rotation,
                    });
                }
                let session_monitor_id = arcen_media::SessionMonitorId::new(
                    u16::try_from(index + 1)
                        .map_err(|_| MacOsMultiMonitorError::CoordinateOverflow("monitor id"))?,
                )?;
                let panel = virtual_panel_for(pq_requested, force_sdr_panel, requested_wire);
                Ok(PlannedVirtualMonitor {
                    requested_wire: (*requested_wire).clone(),
                    requested_monitor,
                    session_monitor_id,
                    layout_rect: *rect,
                    raw_layout_rect: *raw_rect,
                    panel,
                    identity: panel_identity(requested_wire, display_set),
                    refresh_hz: arranged_refresh_hz(fps, requested_wire.refresh_hz),
                })
            },
        )
        .collect()
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

fn virtual_panel_for(
    pq_requested: bool,
    force_sdr_panel: bool,
    monitor: &RequestedMonitorDescriptorMsg,
) -> crate::virtual_display::VirtualPanel {
    let client_hdr = monitor
        .color
        .as_ref()
        .map(arcen_media::display_color::DisplayColor::from_msg)
        .is_some_and(arcen_media::display_color::DisplayColor::is_hdr);
    if pq_requested && !force_sdr_panel && client_hdr {
        crate::virtual_display::VirtualPanel::Hdr
    } else {
        crate::virtual_display::VirtualPanel::Sdr
    }
}

fn virtual_serial(display_id: &ClientDisplayId, display_set: u32) -> u32 {
    let mut hash = 0x811C_9DC5_u32;
    for byte in display_id.as_str().as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    0x4100_0000 | ((display_set & 0x0000_0FFF) << 16) | (hash & 0x0000_FFFF)
}

fn panel_identity(
    monitor: &RequestedMonitorDescriptorMsg,
    display_set: u32,
) -> crate::virtual_display::PanelIdentity {
    let size_mm = arcen_outputs::edid::physical_size_mm(arcen_outputs::edid::EdidRequest {
        width: monitor.width_px,
        height: monitor.height_px,
        refresh_hz: monitor.refresh_hz,
        width_mm: monitor.width_mm,
        height_mm: monitor.height_mm,
        scale: monitor.scale,
        product_id: 0,
        serial: 0,
        color: monitor
            .color
            .as_ref()
            .map(arcen_media::display_color::DisplayColor::from_msg),
    });
    crate::virtual_display::PanelIdentity {
        size_mm,
        name: if monitor.name.trim().is_empty() {
            "Deck display".to_owned()
        } else {
            monitor.name.trim().to_owned()
        },
        color: monitor
            .color
            .as_ref()
            .map(arcen_media::display_color::DisplayColor::from_msg),
        serial: virtual_serial(&monitor.client_display_id, display_set),
    }
}

fn arranged_refresh_hz(fps: u32, requested: u32) -> u32 {
    const MIN_HZ: u32 = 60;
    const MAX_HZ: u32 = 120;
    requested
        .max(fps.max(1).saturating_mul(2))
        .clamp(MIN_HZ, MAX_HZ)
}

fn create_virtual_displays(
    specs: &[PlannedVirtualMonitor],
) -> Result<(MacOsVirtualDisplays, Vec<DisplaySnapshot>), MacOsMultiMonitorError> {
    let requests = specs
        .iter()
        .map(|spec| crate::virtual_display::VirtualDisplayChildRequest {
            width: spec.requested_wire.width_px,
            height: spec.requested_wire.height_px,
            refresh_hz: f64::from(spec.refresh_hz),
            hdr: spec.panel == crate::virtual_display::VirtualPanel::Hdr,
            size_mm: spec.identity.size_mm,
            name: spec.identity.name.clone(),
            serial: spec.identity.serial,
            x: spec.raw_x(),
            y: spec.raw_y(),
        })
        .collect::<Vec<_>>();
    let (child, child_displays) = crate::virtual_display::VirtualDisplayChild::start(&requests)
        .map_err(|error| {
            tracing::warn!(
                target: arcen_telemetry::names::target::MEDIA,
                %error,
                "virtual display child failed"
            );
            MacOsMultiMonitorError::Arrangement(error)
        })?;
    let mut displays = specs
        .iter()
        .zip(&child_displays)
        .map(|(spec, display)| DisplaySnapshot {
            display_id: display.display_id,
            pixel_width: usize::try_from(spec.requested_wire.width_px).unwrap_or(0),
            pixel_height: usize::try_from(spec.requested_wire.height_px).unwrap_or(0),
            origin_x: f64::from(spec.raw_x()),
            origin_y: f64::from(spec.raw_y()),
        })
        .collect::<Vec<_>>();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    for (spec, display) in specs.iter().zip(&mut displays) {
        if let Some((x, y, _width, _height)) = crate::displays::point_bounds(display.display_id) {
            display.origin_x = x;
            display.origin_y = y;
        }
        if spec.panel == crate::virtual_display::VirtualPanel::Hdr
            && !crate::displays::potential_headroom(display.display_id)
                .is_some_and(|headroom| headroom > 1.0)
        {
            return Err(MacOsMultiMonitorError::HdrNotProven {
                client_display_id: spec.requested_wire.client_display_id.as_str().to_owned(),
                display_id: display.display_id,
            });
        }
    }
    Ok((
        MacOsVirtualDisplays {
            _child: child,
            len: displays.len(),
        },
        displays,
    ))
}

#[derive(Debug)]
struct GenericVirtualDisplays<T> {
    displays: Vec<T>,
}

fn create_virtual_displays_with<T>(
    specs: &[PlannedVirtualMonitor],
    mut create: impl FnMut(&PlannedVirtualMonitor) -> Result<(T, u32), String>,
) -> Result<(GenericVirtualDisplays<T>, Vec<DisplaySnapshot>), MacOsMultiMonitorError> {
    let mut displays = Vec::with_capacity(specs.len());
    let mut snapshots = Vec::with_capacity(specs.len());
    for spec in specs {
        let (display, display_id) =
            create(spec).map_err(|detail| MacOsMultiMonitorError::VirtualDisplay {
                client_display_id: spec.requested_wire.client_display_id.as_str().to_owned(),
                detail,
            })?;
        snapshots.push(DisplaySnapshot {
            display_id,
            pixel_width: usize::try_from(spec.requested_wire.width_px)
                .map_err(|_| MacOsMultiMonitorError::CoordinateOverflow("display width"))?,
            pixel_height: usize::try_from(spec.requested_wire.height_px)
                .map_err(|_| MacOsMultiMonitorError::CoordinateOverflow("display height"))?,
            origin_x: f64::from(spec.layout_rect.x),
            origin_y: f64::from(spec.layout_rect.y),
        });
        displays.push(display);
    }
    Ok((GenericVirtualDisplays { displays }, snapshots))
}

fn plan_from_virtual_displays(
    specs: &[PlannedVirtualMonitor],
    displays: &[DisplaySnapshot],
    offer: &AuthMultiMonitorOfferMsg,
    carrier: MultiMonitorCarrierMsg,
    video: arcen_media::session_plan::ResolvedVideoPlan,
    codec: crate::encode::EncoderCodec,
    fps: u32,
) -> Result<MacOsMultiMonitorPlan, MacOsMultiMonitorError> {
    let raw_rects = specs
        .iter()
        .map(|spec| spec.layout_rect)
        .collect::<Vec<_>>();
    let raw_bounds = arcen_media::LayoutBounds::from_rects(&raw_rects)?;
    let translation = raw_bounds.translation_to_origin();
    let monitors = specs
        .iter()
        .zip(displays)
        .map(|(spec, display)| {
            let x = checked_add_axis(spec.layout_rect.x, translation.dx, "monitor x")?;
            let y = checked_add_axis(spec.layout_rect.y, translation.dy, "monitor y")?;
            Ok(MacOsMonitorPlan {
                display: *display,
                client_display_id: spec.requested_wire.client_display_id.clone(),
                requested_monitor: spec.requested_monitor.clone(),
                session_monitor_id: spec.session_monitor_id,
                x,
                y,
                width_px: spec.requested_wire.width_px,
                height_px: spec.requested_wire.height_px,
                refresh_hz: spec.refresh_hz,
                is_primary: spec.requested_wire.is_primary,
            })
        })
        .collect::<Result<Vec<_>, MacOsMultiMonitorError>>()?;
    let media_roster = media_roster(
        &monitors,
        arcen_media::TopologyGeneration::FIRST,
        video,
        codec,
        fps,
    )?;
    let applied_regions = region_sets(&monitors, arcen_media::TopologyGeneration::FIRST)?.1;
    let server_capability = server_capability(
        offer,
        carrier,
        arcen_media::TopologyGeneration::FIRST,
        &monitors,
        &media_roster,
        video,
    )?;
    let bounds = desktop_bounds(&monitors)?;
    Ok(MacOsMultiMonitorPlan {
        generation: arcen_media::TopologyGeneration::FIRST,
        carrier,
        monitors,
        media_roster,
        server_capability,
        desktop_x: bounds.x,
        desktop_y: bounds.y,
        desktop_width_px: bounds.width,
        desktop_height_px: bounds.height,
        applied_regions,
    })
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
        range: match plan.range {
            "full" => arcen_media::ColorRange::Full,
            _ => arcen_media::ColorRange::Limited,
        },
        matrix: match plan.matrix {
            "bt2020-ncl" | "bt2020" => arcen_media::ColorMatrix::Bt2020Ncl,
            _ => arcen_media::ColorMatrix::Bt709,
        },
        primaries: match plan.primaries {
            "bt2020" => arcen_media::ColorPrimaries::Bt2020,
            "display-p3" => arcen_media::ColorPrimaries::DisplayP3,
            _ => arcen_media::ColorPrimaries::Bt709,
        },
        transfer: match plan.transfer {
            "pq" => arcen_media::TransferCharacteristics::Pq,
            "hlg" => arcen_media::TransferCharacteristics::Hlg,
            "srgb" => arcen_media::TransferCharacteristics::Srgb,
            _ => arcen_media::TransferCharacteristics::Bt709,
        },
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
    let descriptors = applied_descriptors(monitors, media_roster, video)?;
    let applied = AppliedMonitorTopologyMsg::new(
        generation.get(),
        bounds.x,
        bounds.y,
        bounds.width,
        bounds.height,
        0,
        0,
        carrier,
        descriptors,
    )?;
    Ok(ServerMultiMonitorMsg::new(
        offer.max_monitors(),
        offer.supported_rotations().to_vec(),
        true,
        TopologyBackendKindMsg::VirtualOutputs,
        OFFERED_CARRIERS.to_vec(),
        Some(applied),
    )?)
}

fn applied_descriptors(
    monitors: &[MacOsMonitorPlan],
    media_roster: &arcen_media::RegionMediaRoster,
    video: arcen_media::session_plan::ResolvedVideoPlan,
) -> Result<Vec<AppliedMonitorDescriptorMsg>, MacOsMultiMonitorError> {
    monitors
        .iter()
        .map(|monitor| applied_descriptor(monitor, media_roster, video))
        .collect()
}

fn applied_descriptor(
    monitor: &MacOsMonitorPlan,
    media_roster: &arcen_media::RegionMediaRoster,
    video: arcen_media::session_plan::ResolvedVideoPlan,
) -> Result<AppliedMonitorDescriptorMsg, MacOsMultiMonitorError> {
    let media = media_roster.plan(monitor.session_monitor_id).ok_or(
        MacOsMultiMonitorError::InvalidMedia(
            arcen_media::MediaContractError::DuplicateSessionMonitorId(
                monitor.session_monitor_id.get(),
            ),
        ),
    )?;
    Ok(AppliedMonitorDescriptorMsg {
        client_display_id: monitor.client_display_id.clone(),
        session_monitor_id: monitor.session_monitor_id.get(),
        x: monitor.x,
        y: monitor.y,
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
            degradation_reason: video
                .degraded
                .map(|reason| format!("{reason:?}").to_ascii_lowercase())
                .unwrap_or_default(),
        },
    })
}

fn region_sets(
    monitors: &[MacOsMonitorPlan],
    generation: arcen_media::TopologyGeneration,
) -> Result<(arcen_media::RegionSet, arcen_media::AppliedRegionSet), MacOsMultiMonitorError> {
    let generation = arcen_media::RegionGeneration::new(generation.get())?;
    let placements = monitors
        .iter()
        .map(|monitor| {
            Ok(arcen_media::RegionPlacement {
                region_id: arcen_media::RegionId::new(u32::from(monitor.session_monitor_id.get()))?,
                output: arcen_media::OutputIdentity::new(format!(
                    "macos-virtual-display:{}",
                    monitor.display.display_id
                ))?,
                logical_rect: arcen_media::logical_rect_from_layout(
                    monitor.requested_monitor.logical_arrangement_rect()?,
                )?,
                stream_size: arcen_media::PhysicalSize::new(monitor.width_px, monitor.height_px)?,
                scale: arcen_media::scale120_from_scale(monitor.requested_monitor.monitor().scale)?,
                rotation: monitor.requested_monitor.monitor().rotation,
                primary: monitor.is_primary,
                applied_rect: arcen_media::AppliedRect::new(
                    arcen_media::AppliedPoint::new(i64::from(monitor.x), i64::from(monitor.y)),
                    arcen_media::AppliedSize::new(monitor.width_px, monitor.height_px)?,
                )?,
            })
        })
        .collect::<Result<Vec<_>, MacOsMultiMonitorError>>()?;
    Ok(arcen_media::build_region_sets(
        generation,
        arcen_media::TransformConvention::AlreadyCompositorOriented,
        &placements,
    )?)
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

    fn requested_at(
        id: &str,
        legacy: u32,
        primary: bool,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> RequestedMonitorDescriptorMsg {
        RequestedMonitorDescriptorMsg {
            client_display_id: ClientDisplayId::new(id).expect("display id"),
            client_monitor_id: legacy,
            x,
            y,
            width_px: width,
            height_px: height,
            logical_width: width,
            logical_height: height,
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

    fn request(monitors: Vec<RequestedMonitorDescriptorMsg>) -> AuthMultiMonitorRequestMsg {
        let topology =
            arcen_protocol::messages::RequestedMonitorTopologyMsg::new(monitors).expect("topology");
        AuthMultiMonitorRequestMsg::new(topology, OFFERED_CARRIERS.to_vec()).expect("request")
    }

    fn offer(max: u8) -> AuthMultiMonitorOfferMsg {
        AuthMultiMonitorOfferMsg::new(max, SUPPORTED_ROTATIONS.to_vec(), OFFERED_CARRIERS.to_vec())
            .expect("offer")
    }

    fn two_request() -> AuthMultiMonitorRequestMsg {
        request(vec![
            requested_at("secondary", 2, false, 1920, 0, 1920, 1080),
            requested_at("primary", 1, true, 0, 0, 1920, 1080),
        ])
    }

    #[test]
    fn planned_two_display_layout_is_primary_first_and_mirrors_arrangement() {
        let specs = planned_virtual_specs(&two_request(), 60, false, false).expect("specs");
        assert_eq!(specs.len(), 2);
        assert_eq!(
            specs[0].requested_wire.client_display_id.as_str(),
            "primary"
        );
        assert_eq!(specs[0].layout_rect.x, 0);
        assert_eq!(specs[1].layout_rect.x, 1920);
    }

    #[test]
    fn planned_four_display_layout_keeps_the_deck_shape() {
        let specs = planned_virtual_specs(
            &request(vec![
                requested_at("left", 2, false, -1280, 0, 1280, 1024),
                requested_at("primary", 1, true, 0, 0, 1920, 1080),
                requested_at("right", 3, false, 1920, 0, 1280, 1024),
                requested_at("above", 4, false, 0, -900, 1600, 900),
            ]),
            30,
            false,
            false,
        )
        .expect("specs");
        assert_eq!(specs.len(), 4);
        assert_eq!(
            specs[0].requested_wire.client_display_id.as_str(),
            "primary"
        );
        assert_eq!((specs[1].layout_rect.x, specs[1].layout_rect.y), (-1280, 0));
        assert_eq!((specs[2].layout_rect.x, specs[2].layout_rect.y), (1920, 0));
        assert_eq!((specs[3].layout_rect.x, specs[3].layout_rect.y), (0, -900));
    }

    #[test]
    fn planned_offset_layout_preserves_raw_cg_origins_and_applied_bounds() {
        let specs = planned_virtual_specs(
            &request(vec![
                requested_at("primary", 2, true, 0, 0, 2560, 1440),
                requested_at("built-in", 1, false, -1800, 832, 1800, 1130),
            ]),
            60,
            false,
            false,
        )
        .expect("specs");
        assert_eq!(specs[0].raw_x(), 0);
        assert_eq!(specs[0].raw_y(), 0);
        assert_eq!(specs[1].raw_x(), -1800);
        assert_eq!(specs[1].raw_y(), 832);

        let displays = specs
            .iter()
            .map(|spec| DisplaySnapshot {
                display_id: u32::from(spec.session_monitor_id.get()),
                pixel_width: spec.requested_wire.width_px as usize,
                pixel_height: spec.requested_wire.height_px as usize,
                origin_x: f64::from(spec.raw_x()),
                origin_y: f64::from(spec.raw_y()),
            })
            .collect::<Vec<_>>();
        let plan = plan_from_virtual_displays(
            &specs,
            &displays,
            &offer(2),
            MultiMonitorCarrierMsg::MuxedReliableStream,
            arcen_media::session_plan::ResolvedVideoPlan::standard(None),
            crate::encode::EncoderCodec::Hevc,
            60,
        )
        .expect("plan");
        assert_eq!(plan.desktop_width_px, 4360);
        assert_eq!(plan.desktop_height_px, 1962);
        assert_eq!((plan.monitors[0].x, plan.monitors[0].y), (1800, 0));
        assert_eq!((plan.monitors[1].x, plan.monitors[1].y), (0, 832));
    }

    #[test]
    fn virtual_display_serials_are_unique_per_deck_display_and_display_set() {
        let first = ClientDisplayId::new("1").expect("id");
        let second = ClientDisplayId::new("2").expect("id");
        assert_eq!(virtual_serial(&first, 7), virtual_serial(&first, 7));
        assert_ne!(virtual_serial(&first, 7), virtual_serial(&second, 7));
        assert_ne!(virtual_serial(&first, 7), virtual_serial(&first, 8));
    }

    #[test]
    fn partial_virtual_display_failure_drops_started_displays() {
        #[derive(Debug)]
        struct Token(std::rc::Rc<std::cell::Cell<u32>>);
        impl Drop for Token {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let specs = planned_virtual_specs(&two_request(), 60, false, false).expect("specs");
        let drops = std::rc::Rc::new(std::cell::Cell::new(0));
        let mut calls = 0;
        let error = create_virtual_displays_with(&specs, |spec| {
            calls += 1;
            if calls == 2 {
                Err("refused".to_owned())
            } else {
                Ok((
                    Token(std::rc::Rc::clone(&drops)),
                    spec.session_monitor_id.get().into(),
                ))
            }
        })
        .expect_err("partial failure");
        assert!(
            error
                .to_string()
                .contains("could not create an exact virtual display")
        );
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn admitted_plan_uses_nonzero_session_monitor_ids_and_regions() {
        let specs = planned_virtual_specs(&two_request(), 60, false, false).expect("specs");
        let displays = specs
            .iter()
            .map(|spec| DisplaySnapshot {
                display_id: u32::from(spec.session_monitor_id.get()),
                pixel_width: spec.requested_wire.width_px as usize,
                pixel_height: spec.requested_wire.height_px as usize,
                origin_x: f64::from(spec.layout_rect.x),
                origin_y: f64::from(spec.layout_rect.y),
            })
            .collect::<Vec<_>>();
        let plan = plan_from_virtual_displays(
            &specs,
            &displays,
            &offer(2),
            MultiMonitorCarrierMsg::MuxedReliableStream,
            arcen_media::session_plan::ResolvedVideoPlan::standard(None),
            crate::encode::EncoderCodec::Hevc,
            60,
        )
        .expect("plan");
        assert_eq!(plan.monitors[0].session_monitor_id.get(), 1);
        assert_eq!(plan.monitors[1].session_monitor_id.get(), 2);
        let applied = plan.server_capability.applied_topology().expect("applied");
        assert_eq!(applied.primary().session_monitor_id, 1);
        assert_eq!(plan.applied_regions.regions().len(), 2);
        for monitor in applied.monitors() {
            assert_ne!(monitor.session_monitor_id, 0);
            assert_eq!(monitor.media_plan.stream_epoch, plan.generation.get());
        }
    }

    #[test]
    fn a_missing_offer_refuses_the_sidecar_request() {
        let error = admit_virtual_request(
            None,
            Some(&two_request()),
            arcen_media::session_plan::ResolvedVideoPlan::standard(None),
            crate::encode::EncoderCodec::Hevc,
            60,
            false,
            false,
        )
        .expect_err("no offer");
        assert_eq!(error, MacOsMultiMonitorError::NotAdvertised);
    }

    #[test]
    fn refusal_message_names_exact_virtual_display_creation() {
        let specs = planned_virtual_specs(&two_request(), 60, false, false).expect("specs");
        let error =
            create_virtual_displays_with::<()>(&specs, |_| Err("unsupported mode".to_owned()))
                .expect_err("refused");
        assert!(
            error
                .to_string()
                .contains("could not create an exact virtual display")
        );
        assert!(error.to_string().contains("unsupported mode"));
    }
}
