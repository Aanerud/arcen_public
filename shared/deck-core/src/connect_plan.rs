// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Connection planning shared by both Decks: how a connection sizes its
//! stream from the Displays setting, which monitors it advertises, and the
//! remote UI scale each one carries. The shell supplies its displays and the
//! size it can present in fullscreen; nothing here reads the platform.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "display sizes in pixels and scale percentages, the macOS Deck's arithmetic kept exactly"
)]

use arcen_protocol::messages::ClientMonitor;

use crate::settings::{DisplaysMode, RemoteUiScale};

pub use crate::display::{PINNED_MAX_REFRESH_HZ, pinned_refresh_hz};

/// How a connection sizes its stream: following the window, or pinned to
/// the primary display (Primary only, Match my layout).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamSizingPolicy {
    FollowWindow,
    PinPrimary {
        primary_size: Option<[u32; 2]>,
        local_display_count: usize,
        match_layout_fallback: bool,
    },
}

impl StreamSizingPolicy {
    /// `presentation_size` is the size the Deck can actually present in
    /// fullscreen on the primary display (see
    /// `crate::display::presentation_size_for_display` (macOS Deck)). Pinned modes use it
    /// verbatim; `None` falls back to the monitor's own reported size, which is
    /// the pre-DISPLAY-380 behaviour and what unit tests exercise.
    ///
    /// `multi_monitor_topology_attached` mirrors
    /// `options.multi_monitor_topology.is_some()` for this exact connection
    /// attempt: whether a real multi-monitor-v1 request is actually going to
    /// be sent to the host, already decided by the caller (`start_connection`'s
    /// `match_layout_preflight` plus `approved_multi_monitor_selection`) before
    /// this is called. It is what `match_layout_fallback` below is actually
    /// keyed on now that the multi-window runtime is live -- `monitors.len() >
    /// 1` alone used to be a reliable proxy for "will fall back to primary
    /// only" back when the runtime gate made that outcome unconditional, but
    /// today it would otherwise misreport a genuine, successfully negotiated
    /// multi-monitor session as a fallback.
    #[must_use]
    pub fn resolve(
        mode: DisplaysMode,
        monitors: &[ClientMonitor],
        presentation_size: Option<[u32; 2]>,
        multi_monitor_topology_attached: bool,
    ) -> Self {
        let primary_size = primary_monitor(monitors)
            .map(|monitor| presentation_size.unwrap_or([monitor.width_px, monitor.height_px]));
        match mode {
            DisplaysMode::Windowed => StreamSizingPolicy::FollowWindow,
            DisplaysMode::SinglePrimary => StreamSizingPolicy::PinPrimary {
                primary_size,
                local_display_count: monitors.len(),
                match_layout_fallback: false,
            },
            DisplaysMode::MatchLayout => StreamSizingPolicy::PinPrimary {
                primary_size,
                local_display_count: monitors.len(),
                match_layout_fallback: monitors.len() > 1 && !multi_monitor_topology_attached,
            },
        }
    }

    #[must_use]
    pub fn follows_window(self) -> bool {
        matches!(self, StreamSizingPolicy::FollowWindow)
    }

    #[must_use]
    pub fn is_pinned(self) -> bool {
        !self.follows_window()
    }

    #[must_use]
    pub fn primary_size(self) -> Option<[u32; 2]> {
        match self {
            StreamSizingPolicy::FollowWindow => None,
            StreamSizingPolicy::PinPrimary { primary_size, .. } => primary_size,
        }
    }

    #[must_use]
    pub fn match_layout_fallback(self) -> bool {
        matches!(
            self,
            StreamSizingPolicy::PinPrimary {
                match_layout_fallback: true,
                ..
            }
        )
    }

    #[must_use]
    pub fn status_note(self) -> Option<&'static str> {
        self.match_layout_fallback().then_some(
            "Multi-monitor layout matching isn't available for this connection; using the \
             primary display.",
        )
    }
}

#[must_use]
pub fn primary_monitor(monitors: &[ClientMonitor]) -> Option<&ClientMonitor> {
    monitors
        .iter()
        .find(|monitor| monitor.is_primary)
        .or_else(|| monitors.first())
}

/// The physical size to advertise so the host's synthesized EDID lands on
/// `percent` Windows display scaling.
///
/// Windows picks its recommended scaling from the display's DPI, and DPI is
/// pixels over physical size. `100%` is 96 DPI by definition, so the size that
/// produces a given percentage is `pixels * 25.4 / (96 * percent / 100)`.
#[must_use]
pub fn advertised_physical_size_mm(width_px: u32, height_px: u32, percent: u32) -> (f32, f32) {
    let dpi = 96.0 * percent.max(1) as f32 / 100.0;
    (width_px as f32 * 25.4 / dpi, height_px as f32 * 25.4 / dpi)
}

/// This *specific* monitor's own effective `HiDPI` stream-scale multiplier for
/// [`advertised_physical_size_mm`]'s "apparent size" doubling: its own real
/// backing scale (2 for a Retina panel, 1.5 for a scaled Retina mode, 1
/// otherwise) when the session streams at `HiDPI` resolution
/// (`hidpi_streaming`), or unconditionally `1` when it does not -- every
/// stream is point-sized then, regardless of any monitor's own backing
/// scale.
///
/// The *exact* advertised ratio, never an integer rounding of it: a monitor's
/// `scale` and its `width_px`/`height_px` come from one shared
/// `crate::display::metrics::DisplayMetrics` (macOS Deck) reading, so a `HiDPI` stream
/// carries exactly `scale` times the presentable points. Rounding 1.5 up to 2
/// here would advertise a physical size 33% off the size actually streamed
/// and put a scaled Retina session back at the wrong apparent size.
///
/// Always derived from *this* monitor's own `scale`/`monitor().scale`,
/// never a blanket value borrowed from the primary or any other monitor in
/// the same roster: a mixed Retina-primary/non-Retina-secondary roster must
/// scale each entry from its own backing scale, never from whichever
/// monitor happened to be used to compute a single shared multiplier for
/// everyone -- see [`apply_remote_ui_scale`] and
/// [`apply_remote_ui_scale_to_requested_topology`]'s own per-monitor
/// mixed-Retina requirement.
///
/// A non-finite or sub-1 scale (never produced by `DisplayMetrics`, which
/// refuses both) falls back to 1: an unreadable display must not silently
/// shrink the advertised panel.
#[must_use]
pub fn effective_monitor_stream_scale(monitor_scale: f32, hidpi_streaming: bool) -> f32 {
    if hidpi_streaming && monitor_scale.is_finite() {
        monitor_scale.max(1.0)
    } else {
        1.0
    }
}

/// The percentage [`advertised_physical_size_mm`] must actually target for
/// one monitor: the user's chosen percentage multiplied by that monitor's own
/// effective stream scale ([`effective_monitor_stream_scale`]).
///
/// Rounded to the whole percent the EDID's millimetre fields can express, and
/// never zero.
#[must_use]
pub fn effective_remote_ui_percent(percent: u32, monitor_scale: f32, hidpi_streaming: bool) -> u32 {
    let scale = effective_monitor_stream_scale(monitor_scale, hidpi_streaming);
    let effective = (percent as f32 * scale).round();
    effective.clamp(1.0, u32::MAX as f32) as u32
}

/// Resolve one monitor's coherent stream-scale and synthesized EDID size.
///
/// `Automatic` means 100% for a point-sized stream. A `HiDPI` stream multiplies
/// that percentage by its exact backing ratio so both stream modes describe
/// the same apparent desktop size. The returned scale is likewise the stream's
/// actual logical-to-physical ratio, never the local panel's raw backing scale
/// when only logical points are being streamed.
#[must_use]
pub fn remote_ui_monitor_contract(
    width_px: u32,
    height_px: u32,
    monitor_scale: f32,
    hidpi_streaming: bool,
    scale: RemoteUiScale,
) -> (f32, f32, f32) {
    let stream_scale = effective_monitor_stream_scale(monitor_scale, hidpi_streaming);
    let percent = match scale {
        RemoteUiScale::Automatic => 100,
        RemoteUiScale::Percent(percent) => percent,
    };
    let effective = effective_remote_ui_percent(percent, monitor_scale, hidpi_streaming);
    let (width_mm, height_mm) = advertised_physical_size_mm(width_px, height_px, effective);
    (stream_scale, width_mm, height_mm)
}

/// Rewrite the reported physical size of each monitor so the remote desktop
/// comes up at the user's chosen UI scale.
///
/// The chosen percentage is *apparent* size, so each monitor's own effective
/// stream scale (see [`effective_monitor_stream_scale`]: 2 for a `HiDPI`
/// Retina monitor while `hidpi_streaming` is on, 1.5 for a scaled Retina
/// mode, 1 otherwise) has to multiply it — without this, "150%" on a `HiDPI`
/// stream renders every element at half the physical size it has on a
/// point-size stream, which is why a Retina session looked microscopic.
/// Computed independently per monitor (never a single shared multiplier for
/// the whole roster), so a mixed Retina-primary/non-Retina-secondary layout
/// scales each display from its own backing scale.
pub fn apply_remote_ui_scale(
    monitors: &mut [ClientMonitor],
    scale: RemoteUiScale,
    hidpi_display_mask: u8,
) {
    for (index, monitor) in monitors.iter_mut().enumerate() {
        let hidpi_streaming = index < 4 && hidpi_display_mask & (1u8 << index) != 0;
        let (stream_scale, width_mm, height_mm) = remote_ui_monitor_contract(
            monitor.width_px,
            monitor.height_px,
            monitor.scale,
            hidpi_streaming,
            scale,
        );
        monitor.scale = stream_scale;
        monitor.width_mm = width_mm;
        monitor.height_mm = height_mm;
    }
}

/// Applies Deck's default Automatic UI-scale contract to a legacy monitor
/// roster used by CLI quick-connect and smoke diagnostics.
pub fn apply_automatic_remote_ui_scale_to_monitors(
    monitors: &mut [ClientMonitor],
    hidpi_display_mask: u8,
) {
    apply_remote_ui_scale(monitors, RemoteUiScale::Automatic, hidpi_display_mask);
}

/// Rewrites the reported physical size of every monitor in an approved
/// multi-monitor-v1 requested topology to the user's chosen Remote UI Scale,
/// applying the exact same `advertised_physical_size_mm` formula and
/// per-monitor `HiDPI` multiplier ([`effective_remote_ui_percent`]) as
/// [`apply_remote_ui_scale`] uses for the legacy `options.monitors` roster
/// -- each monitor's own `monitor().scale`, never one shared multiplier
/// borrowed from the primary or any other entry in the same roster, so a
/// mixed Retina-primary/non-Retina-secondary topology scales each display
/// independently from its own backing scale.
///
/// Without this, `options.monitors` (scaled) and the multi-monitor-v1
/// sidecar (`options.multi_monitor_topology`, built straight from
/// `crate::display::topology::build_requested_topology`'s real, unscaled
/// hardware measurements) would disagree on the exact same primary
/// monitor's physical size -- a host computing DPI from the sidecar's own
/// per-monitor descriptors (`crate::transport::multi_monitor::to_wire_topology`)
/// would see the user's real panel size and ignore their chosen scale
/// entirely, while a host still falling back to the legacy roster would
/// honour it. Called once, in `connect_options_with_stream_sizing_policy`,
/// on the already-preflighted/approved topology snapshot before it is
/// threaded into `approved_multi_monitor_selection` (macOS Deck) and stored/attached as
/// the sidecar -- never re-derived later at auth time.
#[must_use]
/// # Panics
///
/// Never: rewriting a validated topology's scale and physical size keeps it
/// valid.
#[allow(
    clippy::expect_used,
    reason = "the invariant is stated in the panic message"
)]
pub fn apply_remote_ui_scale_to_requested_topology(
    topology: &arcen_media::RequestedMonitorTopology,
    scale: RemoteUiScale,
    hidpi_display_mask: u8,
) -> arcen_media::RequestedMonitorTopology {
    let monitors: Vec<arcen_media::RequestedMonitor> = topology
        .monitors()
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, mut requested)| {
            let hidpi_streaming = index < 4 && hidpi_display_mask & (1u8 << index) != 0;
            let (stream_scale, width_mm, height_mm) = remote_ui_monitor_contract(
                requested.monitor().width_px,
                requested.monitor().height_px,
                requested.monitor().scale,
                hidpi_streaming,
                scale,
            );
            requested.monitor.scale = stream_scale;
            requested.monitor.width_mm = width_mm;
            requested.monitor.height_mm = height_mm;
            requested
        })
        .collect();
    // Only `scale`/`width_mm`/`height_mm` changed above; every invariant
    // `RequestedMonitorTopology::new` re-checks (client display identity,
    // exactly one primary, logical arrangement rect, duplicate monitor id,
    // zero dimensions) depends solely on fields left untouched here, while
    // `remote_ui_monitor_contract` always returns a finite positive scale.
    arcen_media::RequestedMonitorTopology::new(monitors)
        .expect("rewriting the validated stream scale and EDID size cannot invalidate topology")
}

/// Applies Deck's default Automatic UI-scale contract to a requested topology
/// used by multi-monitor smoke diagnostics.
#[must_use]
pub fn apply_automatic_remote_ui_scale_to_topology(
    topology: &arcen_media::RequestedMonitorTopology,
    hidpi_display_mask: u8,
) -> arcen_media::RequestedMonitorTopology {
    apply_remote_ui_scale_to_requested_topology(
        topology,
        RemoteUiScale::Automatic,
        hidpi_display_mask,
    )
}

/// What a Deck states about its displays' own refresh
/// (`AuthResponse::display_refresh_v1`): each display's refresh as the
/// shell read it, before [`pinned_refresh_hz`] limits the virtual display
/// mode the Deck asks for, and the display the session window is on.
///
/// Hosts serve Auto and Speed at the refresh of the display that shows the
/// picture (each monitor its own in Match My Layout), so a 120 Hz panel gets
/// 120 fps even though its virtual display mode stays at 60 Hz. `displays`
/// must be the shell's own reading (`DeckServices::displays`, primary
/// first), not a pinned roster; a refresh of zero states nothing. `None`
/// for no displays.
#[must_use]
pub fn deck_display_refresh(
    displays: &[ClientMonitor],
    stream_display_id: Option<u32>,
) -> Option<arcen_protocol::messages::DeckDisplayRefreshMsg> {
    use arcen_protocol::messages::{DeckPanelRefreshMsg, MAX_DECK_DISPLAY_REFRESH_ENTRIES};
    let stated: Vec<_> = displays
        .iter()
        .filter(|display| display.refresh_hz > 0)
        .take(MAX_DECK_DISPLAY_REFRESH_ENTRIES)
        .map(|display| DeckPanelRefreshMsg {
            id: display.id,
            refresh_hz: display.refresh_hz,
        })
        .collect();
    (!stated.is_empty()).then(|| arcen_protocol::messages::DeckDisplayRefreshMsg {
        stream_display_id: stream_display_id
            .filter(|id| displays.iter().any(|display| display.id == *id)),
        displays: stated,
    })
}

#[must_use]
pub fn primary_only_monitors(
    monitors: &[ClientMonitor],
    pinned_size: Option<[u32; 2]>,
) -> Vec<ClientMonitor> {
    primary_monitor(monitors)
        .map(|monitor| {
            let mut monitor = monitor.clone();
            monitor.x = 0;
            monitor.y = 0;
            monitor.is_primary = true;
            // The host builds the remote desktop from this size, so it must be
            // what the Deck will present, not the raw panel size.
            if let Some([width, height]) = pinned_size {
                monitor.width_px = width;
                monitor.height_px = height;
            }
            // Never ask for a virtual display faster than the session can
            // stream; the spare pixel-clock budget is what keeps a HiDPI mode
            // inside EDID 1.4 limits.
            monitor.refresh_hz = pinned_refresh_hz(monitor.refresh_hz);
            monitor
        })
        .into_iter()
        .collect()
}

#[must_use]
pub fn monitors_for_policy(
    policy: StreamSizingPolicy,
    monitors: &[ClientMonitor],
) -> Vec<ClientMonitor> {
    match policy {
        // A windowed session asks for no faster a mode than a pinned one:
        // the panel's own refresh is stated apart (`deck_display_refresh`),
        // and a faster mode would leave a Retina resize without an EDID
        // timing.
        StreamSizingPolicy::FollowWindow => monitors
            .iter()
            .map(|monitor| ClientMonitor {
                // Zero stays "unknown", never 1 Hz.
                refresh_hz: match monitor.refresh_hz {
                    0 => 0,
                    refresh => pinned_refresh_hz(refresh),
                },
                ..monitor.clone()
            })
            .collect(),
        StreamSizingPolicy::PinPrimary { primary_size, .. } => {
            primary_only_monitors(monitors, primary_size)
        }
    }
}

/// The legacy `ConnectOptions.monitors` roster to send alongside
/// `multi_monitor_topology`.
///
/// One frozen post-fallback, post-scale snapshot: whenever multi-monitor is
/// actually going to be negotiated (`multi_monitor_topology` is
/// `Some`), the legacy roster is derived directly from that exact same
/// already-approved, already-scaled sidecar topology via
/// [`client_monitors_from_topology`] -- `monitors`/`policy` are entirely
/// ignored in that branch, never re-read or re-scaled independently. A
/// fresh, independent re-enumeration here could disagree with the
/// already-approved topology if a display was hot-plugged/unplugged in the
/// interval between `match_layout_preflight` capturing it and this call, or
/// if the two rosters were scaled independently -- either way, a host must
/// never be able to observe a `multi_monitor_topology` and a legacy
/// `monitors` roster describing two different snapshots of the local
/// hardware. A genuine post-snapshot hotplug is instead caught later, once
/// connected, by the existing committed-topology/roster-loss failure path
/// (`drive_multi_window`'s hard-failure checks), which disconnects and
/// requires a fresh reconnect rather than silently sending a mismatched
/// hello.
///
/// When no multi-monitor topology will be attached (`None` -- reached for
/// every legacy/single-display/windowed/primary-only session, and also for
/// Match My Layout with multiple displays whenever preflight itself never
/// requests one in the first place, e.g. a diagnostic-only caller that
/// never runs preflight at all), this reproduces exactly today's existing
/// behaviour: `monitors_for_policy` followed by one `apply_remote_ui_scale`
/// pass over the freshly enumerated local `monitors`. A host that lacks or
/// declines multi-monitor-v1 is *not* one of these cases: preflight has
/// already approved and attached a local topology by the time this function
/// runs, so that failure surfaces later, during auth, as a hard connection
/// failure (`crate::transport::multi_monitor::MultiMonitorAuthError::UnsupportedHost`),
/// never a silent reduction back to this legacy roster.
#[must_use]
pub fn legacy_roster_for_connect_options(
    multi_monitor_topology: Option<&crate::runtime::multi_monitor::RequestedMultiMonitorSelection>,
    policy: StreamSizingPolicy,
    monitors: &[ClientMonitor],
    remote_ui_scale: RemoteUiScale,
    hidpi_display_mask: u8,
) -> Vec<ClientMonitor> {
    if let Some(selection) = multi_monitor_topology {
        client_monitors_from_topology(&selection.topology)
    } else {
        let mut legacy_monitors = monitors_for_policy(policy, monitors);
        apply_remote_ui_scale(&mut legacy_monitors, remote_ui_scale, hidpi_display_mask);
        legacy_monitors
    }
}

#[must_use]
pub fn monitors_for_displays_mode(
    mode: DisplaysMode,
    monitors: &[ClientMonitor],
    presentation_size: Option<[u32; 2]>,
) -> Vec<ClientMonitor> {
    // The `media-smoke` CLI diagnostic never negotiates a real
    // multi-monitor-v1 topology (see `main.rs`, which always leaves
    // `ConnectOptions.multi_monitor_topology` at `None`), so this is always a
    // legacy roster derived purely from local display count/policy.
    monitors_for_policy(
        StreamSizingPolicy::resolve(mode, monitors, presentation_size, false),
        monitors,
    )
}

/// Whether the Deck may retarget the remote desktop to the viewer surface.
///
/// DISPLAY-380 fixed this deliberately: only Windowed follows the
/// app window. The pinned modes lock the stream to the target monitor and
/// suppress this tracker entirely, and hosts do not advertise
/// `supports_display_update` in a pinned mode. Asking anyway produces a host
/// rejection (`display_backend_cannot_retarget` on a host without the NVAPI
/// custom-timing path) and cannot change the stream size.
#[must_use]
pub fn display_fit_gate(
    policy: StreamSizingPolicy,
    host_supports_display_update: bool,
    in_session: bool,
    frame_fresh: bool,
) -> bool {
    policy.follows_window() && host_supports_display_update && in_session && frame_fresh
}

/// Converts an already-validated requested topology's monitors back into the
/// legacy `ClientMonitor` wire roster, in the topology's own order.
///
/// Used only to build the reconnect *baseline* identity from a frozen,
/// already-approved [`crate::runtime::multi_monitor::RequestedMultiMonitorSelection`]
/// snapshot (`connection_identity_baseline`) -- never from a fresh
/// enumeration -- so the stored baseline's legacy-roster component and its
/// validated-topology component always describe the exact same negotiated
/// facts, not two independently re-read sources that could disagree.
#[must_use]
pub fn client_monitors_from_topology(
    topology: &arcen_media::RequestedMonitorTopology,
) -> Vec<ClientMonitor> {
    topology
        .monitors()
        .iter()
        .map(|requested| {
            let monitor = requested.monitor();
            ClientMonitor {
                id: monitor.identity.id.parse().unwrap_or_default(),
                x: monitor.x,
                y: monitor.y,
                width_px: monitor.width_px,
                height_px: monitor.height_px,
                scale: monitor.scale,
                refresh_hz: monitor.refresh_hz,
                is_primary: monitor.primary,
                name: monitor.identity.name.clone(),
                width_mm: monitor.width_mm,
                height_mm: monitor.height_mm,
                vendor: monitor.identity.vendor,
                model: monitor.identity.model,
                serial: monitor.identity.serial,
                edid: String::new(),
                color: monitor.color,
                rotation_degrees: rotation_degrees(monitor.rotation),
            }
        })
        .collect()
}

/// Clockwise degrees of a display rotation, as the legacy roster carries it.
#[must_use]
pub const fn rotation_degrees(rotation: arcen_media::Rotation) -> u16 {
    match rotation {
        arcen_media::Rotation::Degrees0 => 0,
        arcen_media::Rotation::Degrees90 => 90,
        arcen_media::Rotation::Degrees180 => 180,
        arcen_media::Rotation::Degrees270 => 270,
    }
}

/// Whether an already-approved multi-monitor topology snapshot
/// (`match_layout_preflight`'s result) should actually be attached to
/// `ConnectOptions.multi_monitor_topology`, as opposed to leaving it at
/// `None` and falling back to exactly today's legacy primary-only auth.
///
/// This is evaluated over the *already-captured* snapshot rather than
/// re-deriving the decision from a fresh display enumeration, so option
/// construction cannot drift from what the preflight actually validated.
/// Building a validated topology and asking a host to apply per-display
/// encode/routing for more than one monitor is only safe once Deck's UI
/// runtime can actually present more than one monitor
/// (see the Deck's `multi_window_runtime_available`, which
/// now returns `true`): once real, this passes `approved` through
/// unchanged for Match My Layout, and still returns `None` for every other
/// mode (Primary Display Only, Windowed) regardless of what was approved.
///
/// `multi_window_runtime_available` mirrors
/// the Deck's `multi_window_runtime_available` (passed in
/// explicitly so both production's actual current behavior and a
/// hypothetical runtime-disabled behavior are directly testable without
/// touching that `const fn`) -- callers must always pass the real `const
/// fn`'s value in production.
#[must_use]
pub fn approved_multi_monitor_selection(
    displays_mode: DisplaysMode,
    approved: Option<crate::runtime::multi_monitor::RequestedMultiMonitorSelection>,
    multi_window_runtime_available: bool,
) -> Option<crate::runtime::multi_monitor::RequestedMultiMonitorSelection> {
    if displays_mode == DisplaysMode::MatchLayout && multi_window_runtime_available {
        approved
    } else {
        None
    }
}

/// The Match My Layout preflight both Decks run before credentials are shown
/// or anything is sent: whether this attempt asks the host for a real
/// multi-monitor-v1 layout, and the one validated topology snapshot it asks
/// with.
///
/// Every mode other than Match My Layout passes with `Ok(None)` (Primary
/// Display Only and Windowed never need a topology), as does Match My Layout
/// while the Deck's multi-window runtime is unavailable or with one local
/// display. With more than one display the attempt fails *loudly* rather
/// than silently streaming only the primary: `platform_gate` is the Deck's
/// own earned local prerequisite (macOS: "Displays have separate Spaces";
/// Windows: none), `full_frame_refusal` the guidance when a presentation
/// policy multi-monitor cannot reproduce on every display is in force
/// (macOS: the notch-covering fullscreen), and `build_topology` is called at
/// most once, only when a topology is actually required. The approved
/// selection always carries `StandardFullscreen`.
///
/// # Errors
///
/// The exact user-facing status text when the attempt must stop.
pub fn match_layout_preflight(
    displays_mode: DisplaysMode,
    local_display_count: usize,
    platform_gate: impl FnOnce(usize) -> Result<(), String>,
    full_frame_refusal: Option<String>,
    build_topology: impl FnOnce() -> Result<
        arcen_media::RequestedMonitorTopology,
        crate::display::topology::TopologyPreflightError,
    >,
    multi_window_runtime_available: bool,
) -> Result<Option<crate::runtime::multi_monitor::RequestedMultiMonitorSelection>, String> {
    if displays_mode != DisplaysMode::MatchLayout {
        return Ok(None);
    }
    if !multi_window_runtime_available {
        return Ok(None);
    }
    platform_gate(local_display_count)?;
    if local_display_count <= 1 {
        return Ok(None);
    }
    if let Some(refusal) = full_frame_refusal {
        return Err(refusal);
    }
    let topology =
        build_topology().map_err(|error| format!("Match My Layout cannot start: {error}"))?;
    Ok(Some(
        crate::runtime::multi_monitor::RequestedMultiMonitorSelection {
            topology,
            safe_area_policy: arcen_protocol::messages::SafeAreaPolicyMsg::StandardFullscreen,
            full_color_display_ids: Vec::new(),
        },
    ))
}

/// The opaque client display ids of an approved topology that must stream
/// full-colour 4:4:4: every display when the preset itself is 4:4:4,
/// otherwise the displays ticked "Require 4:4:4" (`full_color_display_mask`,
/// slots primary first, the first four only).
#[must_use]
pub fn full_color_display_ids(
    topology: &arcen_media::RequestedMonitorTopology,
    color_fidelity: crate::settings::ColorFidelitySettings,
    full_color_display_mask: u8,
) -> Vec<String> {
    full_color_display_ids_for_chroma(
        topology,
        crate::settings::effective_color_fidelity_variant(color_fidelity)
            .video
            .chroma,
        full_color_display_mask,
    )
}

/// [`full_color_display_ids`] for a request whose chroma is already known
/// (a command-line profile): every display when `chroma` is 4:4:4, otherwise
/// the displays in `full_color_display_mask`.
#[must_use]
pub fn full_color_display_ids_for_chroma(
    topology: &arcen_media::RequestedMonitorTopology,
    chroma: arcen_media::ChromaSubsampling,
    full_color_display_mask: u8,
) -> Vec<String> {
    let require_full_color_roster = chroma == arcen_media::ChromaSubsampling::Yuv444;
    topology
        .monitors()
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            require_full_color_roster
                || (*index < 4 && full_color_display_mask & (1u8 << index) != 0)
        })
        .map(|(_, monitor)| monitor.monitor().identity.id.clone())
        .collect()
}

/// Reduces a full validated topology down to just its primary monitor,
/// discarding every other monitor's facts.
///
/// Used to build reconnect identity's live topology snapshot when this
/// session did not negotiate real multi-monitor-v1
/// (`options.multi_monitor_topology.is_none()`: Primary Display Only or
/// Windowed mode, Match My Layout with a single local display, or local
/// preflight never having produced one): the negotiated session is
/// primary-only, so identity must depend only on the primary's own facts --
/// including rotation, which `ClientMonitor` alone cannot express -- never a
/// secondary's. Note this field's presence/absence is decided entirely
/// locally before auth; a host that later lacks/declines a genuine
/// multi-monitor-v1 request instead fails the connection outright and never
/// reaches reconnect-identity construction for that attempt.
#[must_use]
pub fn primary_only_topology(
    topology: &arcen_media::RequestedMonitorTopology,
) -> Option<arcen_media::RequestedMonitorTopology> {
    arcen_media::RequestedMonitorTopology::new(vec![topology.primary().clone()]).ok()
}

/// Reduces the freshly re-enumerated live display state down to exactly what
/// this session negotiated, so reconnect identity never depends on a
/// secondary display's own changes when only the primary was ever
/// negotiated.
///
/// `multi_monitor_topology_negotiated` mirrors
/// `options.multi_monitor_topology.is_some()` -- whether real
/// multi-monitor-v1 was actually approved and requested for *this* session
/// at connect time. When it was, every negotiated monitor's facts (including
/// rotation/safe-area) are identity-relevant, so the full live roster/
/// topology passes through unchanged. When it was not (an old/unsupported
/// host, Match My Layout not requested, or a single local display), the
/// negotiated session is exactly today's legacy primary-only fallback: a
/// secondary display being added, removed, resized, or rotated must never
/// force a spurious reconnect of that unrelated session, so both the roster
/// and the topology are reduced to the primary monitor alone before hashing.
#[must_use]
pub fn negotiated_live_display_state(
    multi_monitor_topology_negotiated: bool,
    full_roster: Vec<ClientMonitor>,
    full_topology: Option<arcen_media::RequestedMonitorTopology>,
) -> (
    Vec<ClientMonitor>,
    Option<arcen_media::RequestedMonitorTopology>,
) {
    if multi_monitor_topology_negotiated {
        return (full_roster, full_topology);
    }
    let primary_only_monitors = primary_monitor(&full_roster).cloned().into_iter().collect();
    let primary_only_live_topology = full_topology.as_ref().and_then(primary_only_topology);
    (primary_only_monitors, primary_only_live_topology)
}

/// Whether a session's reconnect identity covers every live display, not
/// only the primary: when it negotiated several monitors, or when it
/// matches the Deck's layout (`displays_mode` `match_layout`) with one. A
/// display added while a one-display Match My Layout session was away must
/// end its resume ([`crate::sign_in::SignInFailure::DisplaysChanged`]), so
/// the Deck signs in afresh with its whole layout instead of resuming the
/// stale one-display request. Pass the result as
/// [`identity_display_state`]'s `multi_monitor_topology_negotiated`.
#[must_use]
pub fn identity_covers_every_display(
    displays_mode: &str,
    multi_monitor_topology_negotiated: bool,
) -> bool {
    multi_monitor_topology_negotiated || displays_mode == DisplaysMode::MatchLayout.as_wire()
}

/// The live display state a reconnect identity hashes: this Deck's full
/// roster and requested topology now, put through the same Remote UI Scale
/// transform the connect options were built with (so a fixed percentage
/// alone never looks like a display change), then reduced to what this
/// session negotiated ([`negotiated_live_display_state`]). Each Deck
/// enumerates; the rule is this one.
#[must_use]
pub fn identity_display_state(
    mut full_roster: Vec<ClientMonitor>,
    full_topology: Option<arcen_media::RequestedMonitorTopology>,
    remote_ui_scale_percent: u32,
    hidpi_display_mask: u8,
    multi_monitor_topology_negotiated: bool,
) -> (
    Vec<ClientMonitor>,
    Option<arcen_media::RequestedMonitorTopology>,
) {
    let scale = RemoteUiScale::from_percent(remote_ui_scale_percent);
    apply_remote_ui_scale(&mut full_roster, scale, hidpi_display_mask);
    let full_topology = full_topology.map(|topology| {
        apply_remote_ui_scale_to_requested_topology(&topology, scale, hidpi_display_mask)
    });
    let (roster, topology) = negotiated_live_display_state(
        multi_monitor_topology_negotiated,
        full_roster,
        full_topology,
    );
    // A negotiated layout's roster is the layout's own (as the baseline's,
    // `client_monitors_from_topology`): pinned refresh and the stream size
    // each display asks for, never the enumeration's raw refresh and
    // logical size, which would differ with nothing changed.
    match (multi_monitor_topology_negotiated, topology) {
        (true, Some(topology)) => (client_monitors_from_topology(&topology), Some(topology)),
        (_, topology) => (roster, topology),
    }
}

/// The Displays mode the attempt about to start uses.
///
/// A pending one-shot (a recovery's "Primary Display Only" or "Reconnect to
/// your session") wins and is consumed, but only for the endpoint it was
/// made for; a stale one for another host is dropped. Otherwise the
/// connection's own saved choice (`None`: follow the app default). This is
/// what stops a choice made for one host from following the user to the
/// next one.
pub fn resolve_attempt_displays_mode(
    pending_override: &mut Option<(String, DisplaysMode)>,
    endpoint: &str,
    saved: Option<DisplaysMode>,
) -> Option<DisplaysMode> {
    if let Some((owner, mode)) = pending_override.take()
        && owner == endpoint
    {
        return Some(mode);
    }
    saved
}

/// Where an attempt's Displays mode came from, for the log (a field report
/// of "Primary display only changes nothing" was a saved connection keeping
/// its own choice): a recovery's one-shot for this endpoint, the saved
/// connection's own choice, or Settings › Displays.
#[must_use]
pub fn displays_mode_source(
    pending_override: Option<&(String, DisplaysMode)>,
    endpoint: &str,
    saved: Option<DisplaysMode>,
) -> &'static str {
    if pending_override.is_some_and(|(owner, _)| owner == endpoint) {
        "recovery"
    } else if saved.is_some() {
        "connection"
    } else {
        "settings"
    }
}

/// The note Settings › Displays shows while the selected saved connection
/// keeps its own Displays choice, which wins over the setting there.
#[must_use]
pub fn connection_displays_note(connection: &str, mode: DisplaysMode) -> String {
    format!(
        "\u{201c}{connection}\u{201d} has its own Displays choice, {}, and connects with \
         it whatever is chosen here.",
        mode.label()
    )
}

/// The button that hands a saved connection back to Settings › Displays.
#[must_use]
pub fn connection_displays_follow_label(connection: &str) -> String {
    format!("Use this setting for \u{201c}{connection}\u{201d}")
}

/// What a saved connection's panel says it connects with, next to Connect,
/// in the style every inherited value uses: "App default (Match my
/// layout)" while it follows Settings › Displays, "Windowed (this
/// connection)" with a [`CONNECTION_DISPLAYS_USE_SETTINGS`] link while it
/// sets its own (`true`). A field report of "Primary display only still
/// gives two displays" was a saved connection keeping Match my layout,
/// visible only in its editor.
#[must_use]
pub fn connection_displays_line(
    saved: crate::overrides::Override<DisplaysMode>,
    settings: DisplaysMode,
) -> (String, bool) {
    let effective = saved.resolve(settings);
    match effective.source {
        crate::overrides::SettingSource::AppDefault => {
            (format!("App default ({})", effective.value.label()), false)
        }
        crate::overrides::SettingSource::ThisConnection => (
            format!("{} (this connection)", effective.value.label()),
            true,
        ),
    }
}

/// The link at the end of a saved connection's own Displays line that
/// hands it back to Settings › Displays.
pub const CONNECTION_DISPLAYS_USE_SETTINGS: &str = "Use app default";

/// A display recovery the user has to choose, recognised from how a
/// connection ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplaysRecovery {
    /// "This desktop was started without your layout": reconnect to it on
    /// the layout it has, or start a fresh one with yours.
    TopologyConflict,
    /// "This host cannot match all active displays": Primary Display Only.
    Unsupported,
}

/// Which display recovery, if any, a session end offers. The host's reason
/// can arrive wrapped by the transport, so it is matched anywhere in the
/// message.
#[must_use]
pub fn displays_recovery_for(message: &str) -> Option<DisplaysRecovery> {
    if message.contains(arcen_protocol::messages::MULTI_MONITOR_TOPOLOGY_CONFLICT_REASON) {
        Some(DisplaysRecovery::TopologyConflict)
    } else if message.contains(crate::runtime::session::MULTI_MONITOR_UNSUPPORTED_PREFIX) {
        Some(DisplaysRecovery::Unsupported)
    } else {
        None
    }
}

/// What a display recovery choice sets up for the retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplaysRecoveryRetry {
    /// Ask the host to replace the incompatible desktop.
    pub replace_desktop: bool,
    /// A one-shot Displays mode for this endpoint's next attempt.
    /// Nothing is saved on the connection: a host's failure decides one
    /// retry, never the connection's later sessions (a host that recovers
    /// gets the whole layout again).
    pub displays_override: Option<(String, DisplaysMode)>,
    pub status: &'static str,
}

/// The topology conflict's choice. Replacing starts a fresh desktop with
/// this layout; keeping it retries with the layout the existing desktop can
/// serve, once and never persisted (keeping the desktop this once must not
/// downgrade the connection).
#[must_use]
pub fn topology_conflict_retry(endpoint: &str, replace: bool) -> DisplaysRecoveryRetry {
    if replace {
        DisplaysRecoveryRetry {
            replace_desktop: true,
            displays_override: None,
            status: "Starting a fresh session with your layout…",
        }
    } else {
        DisplaysRecoveryRetry {
            replace_desktop: false,
            displays_override: Some((endpoint.to_owned(), DisplaysMode::SinglePrimary)),
            status: "Reconnecting to your existing session…",
        }
    }
}

/// "Use Primary Display Only" for a host that cannot serve the whole
/// layout: pinned for this retry only and never saved on the connection.
/// The host's failure may pass (more outputs connected, a restarted
/// display manager), and a saved choice kept the connection on one
/// display after it had.
#[must_use]
pub fn displays_unsupported_retry(endpoint: &str) -> DisplaysRecoveryRetry {
    DisplaysRecoveryRetry {
        replace_desktop: false,
        displays_override: Some((endpoint.to_owned(), DisplaysMode::SinglePrimary)),
        status: "Connecting with one display for this session\u{2026}",
    }
}

/// What the Connecting status adds while a display recovery's one-shot
/// ([`displays_mode_source`] "recovery") holds the attempt to one display.
pub const RECOVERY_ONE_DISPLAY_NOTE: &str = "One display, for this session only.";

/// What the running session says, with the session's other notes (Stream
/// Statistics and the note shown when the picture arrives), while a display
/// recovery holds it to one display.
pub const RECOVERY_ONE_DISPLAY_SESSION_NOTE: &str =
    "One display for this session only. The next connection asks for all your displays.";

/// The Connecting screen's status for `draft`, with the attempt's note
/// (a display recovery's [`SessionDisplaysOrigin::connecting_note`], or the
/// stream sizing's own) after it.
#[must_use]
pub fn connecting_status(
    draft: &crate::connection_draft::ConnectionDraft,
    note: Option<&str>,
) -> String {
    match note {
        Some(note) => format!("Connecting to {}... {note}", draft.display_name()),
        None => format!("Connecting to {}...", draft.display_name()),
    }
}

/// Why a session shows the displays it does, as far as the Deck has to say
/// so. A Deck resolves it with each attempt's Displays mode, keeps it for
/// the session that attempt starts (resumes included), and replaces it at
/// the next attempt, so an ordinary connection after a recovery says
/// nothing. Never saved.
///
/// Three cases look alike on screen and only one is announced here:
/// - the user's own Primary display only (the connection's choice or
///   Settings › Displays) is [`Self::Chosen`] and needs no note;
/// - a host that shows the asked layout only in part says so itself, in
///   [`crate::multi_window::layout_note`], whatever the origin;
/// - a display recovery's retry to one display is
///   [`Self::RecoveryOneDisplay`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SessionDisplaysOrigin {
    /// The connection's own Displays choice or Settings › Displays.
    #[default]
    Chosen,
    /// A display recovery's one-shot held the attempt to one display, for
    /// this session only.
    RecoveryOneDisplay,
}

impl SessionDisplaysOrigin {
    /// The origin of an attempt whose Displays mode came from `source`
    /// ([`displays_mode_source`]) and resolved to `mode`
    /// ([`resolve_attempt_displays_mode`]).
    #[must_use]
    pub fn for_attempt(source: &str, mode: Option<DisplaysMode>) -> Self {
        if source == "recovery" && mode == Some(DisplaysMode::SinglePrimary) {
            Self::RecoveryOneDisplay
        } else {
            Self::Chosen
        }
    }

    /// What the Connecting status adds: [`RECOVERY_ONE_DISPLAY_NOTE`].
    #[must_use]
    pub const fn connecting_note(self) -> Option<&'static str> {
        match self {
            Self::Chosen => None,
            Self::RecoveryOneDisplay => Some(RECOVERY_ONE_DISPLAY_NOTE),
        }
    }

    /// What the running session says: [`RECOVERY_ONE_DISPLAY_SESSION_NOTE`].
    #[must_use]
    pub const fn session_note(self) -> Option<&'static str> {
        match self {
            Self::Chosen => None,
            Self::RecoveryOneDisplay => Some(RECOVERY_ONE_DISPLAY_SESSION_NOTE),
        }
    }
}

/// Writes the streaming preset's wire profile (codec, chroma, depth, range,
/// matrix, transfer, primaries, selection, pipeline, frame-rate ceiling,
/// encode intent and motion priority) into `profile`, from the Streaming
/// settings. An incoherent Advanced combination falls back to the preset's
/// own contract, with a warning.
pub fn apply_streaming_settings(
    profile: &mut crate::runtime::session::StreamProfile,
    performance: crate::settings::PerformanceMode,
    color_fidelity: crate::settings::ColorFidelitySettings,
) {
    use crate::settings::{PerformanceMode, StreamingPreset, effective_color_fidelity_variant};
    let resolved_color_fidelity = color_fidelity.resolved_variant();
    if !resolved_color_fidelity.is_coherent() {
        // Only the explicit `variant_override` seam is guaranteed
        // coherent; individual Advanced axis overrides are not
        // cross-validated against each other (see
        // `ColorFidelitySettings::resolved_variant`'s doc comment), so
        // this is reachable from ordinary Advanced settings use, not
        // just a programming error. Falling back to the preset keeps an
        // incoherent combination from ever reaching the host.
        tracing::warn!(
            target: "arcen::display",
            requested = %resolved_color_fidelity.id(),
            preset = color_fidelity.preset.key(),
            "colour fidelity resolved to a combination Arcen does not offer; falling back \
             to the preset's own contract for this connection",
        );
    }
    let effective_color_fidelity = effective_color_fidelity_variant(color_fidelity);
    profile.codec = effective_color_fidelity.video.codec.token().to_string();
    profile.chroma = effective_color_fidelity.video.chroma.token().to_string();
    profile.bit_depth = effective_color_fidelity.video.bit_depth.token().to_string();
    profile.color_range = effective_color_fidelity.video.range.token().to_string();
    profile.color_matrix = effective_color_fidelity.video.matrix.token().to_string();
    // The two axes that carry an HDR ask. They come from the same resolved
    // variant as the others, so a preset cannot advertise HDR primaries
    // while streaming an SDR transfer.
    profile.transfer = effective_color_fidelity.video.transfer.token().to_string();
    profile.color_primaries = effective_color_fidelity.video.primaries.token().to_string();
    profile.video_selection = color_fidelity.video_selection();
    let preset_contract = StreamingPreset::from_settings(performance, color_fidelity).contract();
    profile.pipeline = StreamingPreset::from_settings(performance, color_fidelity).wire_pipeline();
    profile.max_fps =
        preset_contract.map_or_else(|| performance.max_fps(), |contract| contract.max_fps);
    profile.encode_intent = preset_contract
        .map_or_else(
            || color_fidelity.preset.encode_intent(),
            |contract| contract.intent,
        )
        .token()
        .to_string();
    // A custom combination still says what it favours: the high frame-rate
    // mode is Speed's, so it keeps motion; anything else keeps detail.
    profile.motion_priority = preset_contract
        .map_or_else(
            || match performance {
                PerformanceMode::High | PerformanceMode::HighLegacy => {
                    arcen_media::video::MotionPriority::Motion
                }
                PerformanceMode::Standard => arcen_media::video::MotionPriority::Detail,
            },
            |contract| contract.priority,
        )
        .token()
        .to_string();
}

/// The request `profile` makes, as a video configuration; `None` when one
/// of its tokens is not one Arcen knows.
#[must_use]
pub fn profile_video(
    profile: &crate::runtime::session::StreamProfile,
) -> Option<arcen_media::VideoConfiguration> {
    Some(arcen_media::VideoConfiguration {
        codec: arcen_media::VideoCodec::from_token(&profile.codec)?,
        chroma: arcen_media::ChromaSubsampling::from_token(&profile.chroma)?,
        bit_depth: arcen_media::BitDepth::from_token(&profile.bit_depth)?,
        range: arcen_media::ColorRange::from_token(&profile.color_range)?,
        matrix: arcen_media::ColorMatrix::from_token(&profile.color_matrix)?,
        primaries: arcen_media::ColorPrimaries::from_token(&profile.color_primaries)?,
        transfer: arcen_media::TransferCharacteristics::from_token(&profile.transfer)?,
    })
}

/// Never asks for a preset this Deck cannot decode: a Grading or HDR
/// `profile` whose request fails the Pier's own decode check for
/// `capabilities` (no HEVC Main 4:4:4 10-bit, for example) becomes Auto,
/// the preset Settings shows in its place
/// ([`crate::decode_report::decodable_preset`]). Returns the pipeline it
/// replaced. Auto, Speed (adaptive: the Pier picks their codec from the
/// hello's flags) and Custom (a developer's exact axes) are left alone.
pub fn keep_profile_decodable(
    profile: &mut crate::runtime::session::StreamProfile,
    capabilities: &crate::decode::DecodeCapabilities,
) -> Option<arcen_protocol::messages::StreamPipeline> {
    use arcen_protocol::messages::StreamPipeline;
    let requested = profile.pipeline.clone()?;
    if !matches!(requested, StreamPipeline::Grading | StreamPipeline::Hdr) {
        return None;
    }
    if profile_video(profile).is_some_and(|video| {
        crate::decode_report::request_decodes(capabilities, profile.video_selection, video)
    }) {
        return None;
    }
    apply_streaming_settings(
        profile,
        crate::settings::PerformanceMode::Standard,
        crate::settings::ColorFidelitySettings::default(),
    );
    tracing::warn!(
        target: crate::log_target::SESSION,
        event = "deck_preset_not_decodable",
        requested = requested.token(),
        asked = profile.pipeline.as_ref().map(StreamPipeline::token),
        h265_decode = capabilities.h265,
        yuv444_decode = capabilities.yuv444,
        main10_decode = capabilities.main10,
        decoder_backend = capabilities.decoder_backend_label(),
        "this decoder cannot decode the chosen preset; the Deck asks for Auto instead",
    );
    Some(requested)
}

/// The TLS trust a connection starts with when its draft carries none, by
/// Security mode: High keeps the system's roots (`None`), Medium the pin
/// remembered or accepted for `endpoint` or else trust on first use, Low the
/// insecure escape hatch (which the TLS layer still double-gates behind
/// `ARCEN_ACCEPT_INSECURE=1`).
#[must_use]
pub fn tls_trust_for(
    security: crate::settings::SecurityMode,
    endpoint: String,
    pin: Option<arcen_transport::tls::TlsPin>,
) -> Option<crate::runtime::tls::TlsTrustConfig> {
    use crate::runtime::tls::TlsTrustConfig;
    use crate::settings::SecurityMode;
    // Low without ARCEN_ACCEPT_INSECURE=1 checks as Medium, as Settings
    // shows it, instead of failing every connection at the double gate.
    match security.in_force() {
        SecurityMode::High => None,
        SecurityMode::Medium => Some(match pin {
            Some(pin) => match pin.kind {
                arcen_transport::tls::PinKind::CertificateSha256 => {
                    TlsTrustConfig::pinned_certificate(pin.digest)
                }
                arcen_transport::tls::PinKind::SubjectPublicKeyInfoSha256 => {
                    TlsTrustConfig::pinned_spki(pin.digest)
                }
            },
            None => TlsTrustConfig::tofu_probe(endpoint),
        }),
        SecurityMode::Low => Some(TlsTrustConfig::insecure_dev_escape_hatch(true)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::metrics::{DisplayMetrics, LogicalRect, SafeAreaInsets};
    use crate::display::topology::build_requested_topology_from;

    #[test]
    fn a_saved_connections_panel_says_which_displays_choice_it_connects_with() {
        use crate::overrides::Override;
        assert_eq!(
            connection_displays_line(
                Override::Own(DisplaysMode::Windowed),
                DisplaysMode::MatchLayout
            ),
            ("Windowed (this connection)".to_owned(), true)
        );
        // Its own value, even when Settings has the same one: the link hands
        // it back.
        assert_eq!(
            connection_displays_line(
                Override::Own(DisplaysMode::SinglePrimary),
                DisplaysMode::SinglePrimary
            ),
            ("Primary display only (this connection)".to_owned(), true)
        );
        assert_eq!(
            connection_displays_line(Override::Inherit, DisplaysMode::MatchLayout),
            ("App default (Match my layout)".to_owned(), false)
        );
    }

    #[test]
    fn a_display_recovery_holds_one_retry_and_is_never_saved() {
        let endpoint = "pier.example:18444";
        let unsupported = displays_unsupported_retry(endpoint);
        assert_eq!(
            unsupported.displays_override,
            Some((endpoint.to_owned(), DisplaysMode::SinglePrimary))
        );
        // The retry consumes it; the next attempt reads the connection's
        // own choice (or Settings) again.
        let mut pending = unsupported.displays_override;
        let source = displays_mode_source(pending.as_ref(), endpoint, None);
        let mode = resolve_attempt_displays_mode(&mut pending, endpoint, None);
        assert_eq!(
            SessionDisplaysOrigin::for_attempt(source, mode).connecting_note(),
            Some(RECOVERY_ONE_DISPLAY_NOTE)
        );
        let source = displays_mode_source(pending.as_ref(), endpoint, None);
        let mode = resolve_attempt_displays_mode(&mut pending, endpoint, None);
        assert_eq!((source, mode), ("settings", None));
        assert_eq!(
            SessionDisplaysOrigin::for_attempt(source, mode).connecting_note(),
            None
        );
    }

    /// Each way into a session, and what the running session says about
    /// its displays.
    #[test]
    fn only_a_recovery_retry_says_its_one_display_is_for_this_session() {
        let endpoint = "pier.example:18444";
        let attempt = |pending: &mut Option<(String, DisplaysMode)>,
                       endpoint: &str,
                       saved: Option<DisplaysMode>| {
            let source = displays_mode_source(pending.as_ref(), endpoint, saved);
            let mode = resolve_attempt_displays_mode(pending, endpoint, saved);
            SessionDisplaysOrigin::for_attempt(source, mode)
        };

        // "Use Primary Display Only" and "Reconnect to your session" both
        // hold the retry to one display, and the running session says so.
        for retry in [
            displays_unsupported_retry(endpoint),
            topology_conflict_retry(endpoint, false),
        ] {
            let mut pending = retry.displays_override;
            let origin = attempt(&mut pending, endpoint, None);
            assert_eq!(origin, SessionDisplaysOrigin::RecoveryOneDisplay);
            assert_eq!(
                origin.session_note(),
                Some(RECOVERY_ONE_DISPLAY_SESSION_NOTE)
            );
            // The next ordinary connection resets it.
            let next = attempt(&mut pending, endpoint, None);
            assert_eq!(next, SessionDisplaysOrigin::Chosen);
            assert_eq!(next.session_note(), None);
        }

        // Starting fresh with the whole layout pins nothing.
        let mut pending = topology_conflict_retry(endpoint, true).displays_override;
        assert_eq!(
            attempt(&mut pending, endpoint, None),
            SessionDisplaysOrigin::Chosen
        );

        // The user's own Primary display only needs no note, from the
        // connection or from Settings › Displays.
        let mut pending = None;
        let own = attempt(&mut pending, endpoint, Some(DisplaysMode::SinglePrimary));
        assert_eq!(own, SessionDisplaysOrigin::Chosen);
        assert_eq!((own.connecting_note(), own.session_note()), (None, None));
        assert_eq!(
            SessionDisplaysOrigin::for_attempt("settings", Some(DisplaysMode::SinglePrimary)),
            SessionDisplaysOrigin::Chosen
        );

        // A one-shot made for another host is dropped, and says nothing.
        let mut pending = displays_unsupported_retry("other.example:18444").displays_override;
        assert_eq!(
            attempt(&mut pending, endpoint, None),
            SessionDisplaysOrigin::Chosen
        );
        assert_eq!(pending, None);
        assert_eq!(
            SessionDisplaysOrigin::default(),
            SessionDisplaysOrigin::Chosen
        );
    }

    #[test]
    fn a_saved_connections_own_displays_choice_wins_and_says_so() {
        // The field case: Settings › Displays is Primary display only, the
        // saved connection keeps Match my layout, so the attempt asks for
        // both monitors. The log and Settings now name why.
        let saved = Some(DisplaysMode::MatchLayout);
        let mut pending = None;
        assert_eq!(
            displays_mode_source(pending.as_ref(), "pier.example:18444", saved),
            "connection"
        );
        assert_eq!(
            resolve_attempt_displays_mode(&mut pending, "pier.example:18444", saved)
                .unwrap_or(DisplaysMode::SinglePrimary),
            DisplaysMode::MatchLayout
        );
        assert_eq!(
            displays_mode_source(None, "pier.example:18444", None),
            "settings"
        );
        let recovery = Some(("pier.example:18444".to_owned(), DisplaysMode::SinglePrimary));
        assert_eq!(
            displays_mode_source(recovery.as_ref(), "pier.example:18444", saved),
            "recovery"
        );
        assert_eq!(
            displays_mode_source(recovery.as_ref(), "other.example:18444", saved),
            "connection"
        );
        assert_eq!(
            connection_displays_note("Studio", DisplaysMode::MatchLayout),
            "\u{201c}Studio\u{201d} has its own Displays choice, Match my layout, and connects \
             with it whatever is chosen here."
        );
    }

    /// A 4K panel at 150 % running at 144 Hz, and a 1080p panel beside it.
    fn displays() -> Vec<(ClientMonitor, DisplayMetrics)> {
        let pair = |id: u32, x: i32, logical: (u32, u32), backing: (u32, u32), hz: u32| {
            let metrics = DisplayMetrics::new(
                id,
                LogicalRect::new(x, 0, logical.0, logical.1).expect("arrangement"),
                backing.0,
                backing.1,
                arcen_media::Rotation::Degrees0,
                SafeAreaInsets::ZERO,
            )
            .expect("metrics");
            let monitor = ClientMonitor {
                id,
                x,
                width_px: logical.0,
                height_px: logical.1,
                scale: metrics.scale().get(),
                refresh_hz: hz,
                is_primary: id == 1,
                name: format!("Display {id}"),
                ..ClientMonitor::default()
            };
            (monitor, metrics)
        };
        vec![
            pair(1, 0, (2560, 1440), (3840, 2160), 144),
            pair(2, 2560, (1920, 1080), (1920, 1080), 60),
        ]
    }

    #[test]
    fn the_displays_own_refresh_is_stated_apart_from_the_pinned_mode() {
        let monitors: Vec<ClientMonitor> =
            displays().into_iter().map(|(monitor, _)| monitor).collect();
        // The mode the host is asked to create stays pinned …
        let pinned = primary_only_monitors(&monitors, None);
        assert_eq!(pinned[0].refresh_hz, PINNED_MAX_REFRESH_HZ);
        // … while the panel's own 144 Hz is stated beside it.
        let stated = deck_display_refresh(&monitors, Some(2)).expect("stated");
        assert_eq!(stated.stream_display_id, Some(2));
        assert_eq!(stated.refresh_of(1), Some(144));
        assert_eq!(stated.refresh_of(2), Some(60));
        // A window on a display the Deck does not list names none.
        assert_eq!(
            deck_display_refresh(&monitors, Some(9))
                .expect("stated")
                .stream_display_id,
            None
        );
        assert_eq!(deck_display_refresh(&[], Some(1)), None);
        // A windowed session's roster is pinned the same way.
        let windowed = monitors_for_policy(StreamSizingPolicy::FollowWindow, &monitors);
        assert_eq!(windowed[0].refresh_hz, PINNED_MAX_REFRESH_HZ);
        assert_eq!(windowed[1].refresh_hz, 60);
    }

    /// The reconnect baseline of a negotiated layout and a later live
    /// reading of the same displays hash the same state: refresh above the
    /// pinned 60 Hz and Full resolution must not look like a display change.
    #[test]
    fn a_negotiated_layouts_identity_is_stable_with_high_refresh_and_full_resolution() {
        let displays = displays();
        for (mask, percent) in [(0b01, 0), (0b11, 150), (0, 0)] {
            let approved = build_requested_topology_from(&displays, false, mask).expect("topology");
            let baseline_topology = apply_remote_ui_scale_to_requested_topology(
                &approved,
                RemoteUiScale::from_percent(percent),
                mask,
            );
            let baseline_roster = client_monitors_from_topology(&baseline_topology);
            let roster: Vec<ClientMonitor> = displays
                .iter()
                .map(|(monitor, _)| monitor.clone())
                .collect();
            let (live_roster, live_topology) =
                identity_display_state(roster, Some(approved), percent, mask, true);
            assert_eq!(live_roster, baseline_roster, "mask {mask:#b} {percent} %");
            assert_eq!(live_topology, Some(baseline_topology));
        }
    }

    #[test]
    fn a_console_that_cannot_take_the_layout_offers_primary_display_only() {
        let reason = arcen_protocol::messages::CONSOLE_LAYOUT_UNSUPPORTED_REASON;
        assert!(reason.len() <= 120, "the control-frame budget");
        assert_eq!(
            displays_recovery_for(&format!("session closed: {reason}")),
            Some(DisplaysRecovery::Unsupported)
        );
    }
}
