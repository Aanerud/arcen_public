//! Typed detection of the gap between a host's *applied* multi-monitor-v1
//! topology and Deck's single-viewport legacy fallback.
//!
//! # Status: the production Carrier A multi-window runtime is live
//!
//! `crate::ui::app::ArcenApp` has a full `ServerHello`-triggered per-monitor
//! window lifecycle (`ArcenApp::begin_multi_window_if_applicable`,
//! `ArcenApp::drive_multi_window`), a transactional root+secondary
//! native-window open/confirm/rollback engine
//! (`crate::ui::multi_window_runtime`), per-monitor
//! decode/frame/input routing (`crate::pipeline::monitor_router`,
//! `crate::ui::multi_window_activation::dispatch_secondary_pointer`), a
//! one-way media-worker commit (`crate::ui::media_worker::SharedMediaState::
//! committed_multi_monitor_roster`), and topology-change/hard-failure
//! teardown (`crate::ui::multi_window_activation::teardown_required`) — all
//! exercised by extensive unit/integration tests driving `ArcenApp`'s own
//! fields/methods directly, and the underlying native-window mechanism
//! itself is additionally proven against real hardware by the standalone
//! `crate::ui::multi_window_diagnostic` CLI. [`detect_multi_window_presentation_gap`]
//! below is no longer the last word on an applied multi-monitor-v1 topology:
//! `ArcenApp` now actually acts on one via
//! `ArcenApp::begin_multi_window_if_applicable`, so this function's own
//! `Err` case is only reachable defensively today (a real host applying a
//! layout Deck could not present would mean [`multi_window_runtime_available`]
//! had somehow gone back to `false`).
//!
//! [`multi_window_runtime_available`] now returns `true`: any host that
//! advertises a `multi_monitor_v1_offer` and whose negotiated topology
//! passes `crate::ui::app::match_layout_preflight` (Separate Spaces on,
//! standard fullscreen/no notch, 1..=4 local displays) is asked for a real
//! multi-monitor layout, and `ArcenApp` opens one native fullscreen window
//! per negotiated monitor to present it. An old/unsupported host (no offer,
//! or an offer the local topology cannot satisfy) never reaches that point:
//! `crate::transport::multi_monitor::attach_multi_monitor_v1` requires the
//! host's own advertised offer, so Deck's request-side gate
//! (`crate::ui::app::connect_options_with_stream_sizing_policy` and
//! `match_layout_preflight`) is unaffected by this flip in every other
//! respect than actually being reachable now. A single-viewport regression
//! -- a genuine runtime failure after this flip -- still falls back to
//! [`crate::ui::app::StreamSizingPolicy::status_note`]'s "using the primary
//! display" UI copy exactly as before.

use crate::protocol::messages::ServerHelloMsg;

/// Whether this build of Arcen Deck can actually present more than one
/// monitor at once *in a live session* (a dedicated native fullscreen window
/// per display, opened/torn down by the session controller and receiving
/// only its own monitor's frames/input).
///
/// The underlying native-window mechanism is proven
/// (`crate::ui::multi_window_runtime` + the real
/// `crate::ui::multi_window_diagnostic` CLI diagnostic genuinely
/// fullscreen-bind additional windows with no host), and `ArcenApp` has the
/// full production wiring to drive that mechanism from a real `ServerHello`
/// (see this module's own top-level doc for the exact list) -- so this now
/// returns `true`. Callers on the request side
/// (`crate::ui::app::connect_options_with_stream_sizing_policy`) build and
/// attach a real multi-monitor-v1 requested topology for more than one
/// monitor once local preflight (Separate Spaces, standard fullscreen, 1..=4
/// displays) approves one; the host must still independently advertise
/// (and accept) the offer before anything is actually negotiated on the
/// wire (`crate::transport::multi_monitor::attach_multi_monitor_v1`), so an
/// old/unsupported host is unaffected by this flip. The synthetic harness
/// (`crate::pipeline::synthetic_multi_monitor`) and the native window
/// diagnostic are both unaffected either way: they exercise the
/// router/window mechanism directly and never go through this gate.
#[must_use]
pub const fn multi_window_runtime_available() -> bool {
    true
}

/// The host applied a real multi-monitor-v1 topology with more than one
/// monitor, but this build of Arcen Deck cannot open more than one native
/// fullscreen window to present it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MultiWindowRuntimeUnavailable {
    pub applied_monitor_count: usize,
    pub topology_generation: u64,
}

impl std::fmt::Display for MultiWindowRuntimeUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "host applied a {}-display layout (topology generation {}), but this build of \
             Arcen Deck can only present a single native fullscreen window; showing the \
             primary display only",
            self.applied_monitor_count, self.topology_generation,
        )
    }
}

impl std::error::Error for MultiWindowRuntimeUnavailable {}

/// Inspects `hello`'s multi-monitor-v1 capability (if present and
/// well-formed) and reports [`MultiWindowRuntimeUnavailable`] when the host
/// actually applied more than one monitor server-side.
///
/// Returns `Ok(())` for every legacy/primary-only/malformed case: a host that
/// never advertised multi-monitor-v1, applied only the primary, or sent an
/// unparsable capability object behaves exactly as today (single-display
/// presentation), preserving current single-monitor behavior unconditionally.
pub fn detect_multi_window_presentation_gap(
    hello: &ServerHelloMsg,
) -> Result<(), MultiWindowRuntimeUnavailable> {
    let Ok(Some(capability)) = hello.multi_monitor_v1() else {
        return Ok(());
    };
    let Some(applied) = capability.applied_topology() else {
        return Ok(());
    };
    let monitor_count = applied.monitors().len();
    if monitor_count > 1 {
        return Err(MultiWindowRuntimeUnavailable {
            applied_monitor_count: monitor_count,
            topology_generation: applied.topology_generation(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::messages::{
        AppliedMonitorDescriptorMsg, AppliedMonitorMediaPlanMsg, AppliedMonitorTopologyMsg,
        ClientDisplayId, CursorMode, MultiMonitorCarrierMsg, RotationMsg, ServerMultiMonitorMsg,
        TopologyBackendKindMsg,
    };

    fn base_hello() -> ServerHelloMsg {
        serde_json::from_value(serde_json::json!({
            "type": "server_hello",
            "screen_width": 1920,
            "screen_height": 1080,
        }))
        .expect("minimal server_hello parses")
    }

    fn media_plan() -> AppliedMonitorMediaPlanMsg {
        AppliedMonitorMediaPlanMsg {
            stream_epoch: 1,
            encoder_backend: "openh264-sw-h264".to_string(),
            encoder_class: "software".to_string(),
            codec: "h264".to_string(),
            chroma: "yuv420".to_string(),
            width_px: 1920,
            height_px: 1080,
            fps: 60,
            bitrate_kbps: 8_000,
            cursor_mode: CursorMode::Local,
            degraded: false,
        }
    }

    fn applied_monitor(tag: &str, x: i32, is_primary: bool) -> AppliedMonitorDescriptorMsg {
        AppliedMonitorDescriptorMsg {
            client_display_id: ClientDisplayId::new(tag).expect("valid id"),
            session_monitor_id: u16::from(is_primary) + 1,
            x,
            y: 0,
            width_px: 1920,
            height_px: 1080,
            refresh_hz: 60,
            rotation: RotationMsg::Degrees0,
            is_primary,
            media_plan: media_plan(),
        }
    }

    fn server_multi_monitor(monitors: Vec<AppliedMonitorDescriptorMsg>) -> ServerMultiMonitorMsg {
        let applied = if monitors.is_empty() {
            None
        } else {
            Some(
                AppliedMonitorTopologyMsg::new(
                    3,
                    0,
                    0,
                    1920 * u32::try_from(monitors.len()).unwrap(),
                    1080,
                    0,
                    0,
                    MultiMonitorCarrierMsg::PerMonitorReliableStream,
                    monitors,
                )
                .expect("applied topology"),
            )
        };
        ServerMultiMonitorMsg::new(
            4,
            vec![RotationMsg::Degrees0],
            true,
            TopologyBackendKindMsg::PhysicalOutputs,
            vec![MultiMonitorCarrierMsg::PerMonitorReliableStream],
            applied,
        )
        .expect("server capability")
    }

    #[test]
    fn multi_window_runtime_is_available() {
        // The production Carrier A multi-window wiring is live: this is the
        // load-bearing invariant `crate::ui::app::connect_options_with_stream_sizing_policy`
        // and `match_layout_preflight` both key off to actually request a
        // real multi-monitor-v1 topology once local preflight approves one.
        assert!(multi_window_runtime_available());
    }

    #[test]
    fn legacy_hello_without_the_capability_is_unaffected() {
        assert_eq!(detect_multi_window_presentation_gap(&base_hello()), Ok(()));
    }

    #[test]
    fn applied_primary_only_topology_is_unaffected() {
        let hello = base_hello()
            .with_multi_monitor_v1(&server_multi_monitor(vec![applied_monitor(
                "display-primary",
                0,
                true,
            )]))
            .expect("attach capability");
        assert_eq!(detect_multi_window_presentation_gap(&hello), Ok(()));
    }

    #[test]
    fn applied_multi_monitor_topology_reports_runtime_unavailable() {
        let hello = base_hello()
            .with_multi_monitor_v1(&server_multi_monitor(vec![
                applied_monitor("display-primary", 0, true),
                applied_monitor("display-left", 1920, false),
            ]))
            .expect("attach capability");

        let error = detect_multi_window_presentation_gap(&hello)
            .expect_err("more than one applied monitor must be reported");
        assert_eq!(error.applied_monitor_count, 2);
        assert_eq!(error.topology_generation, 3);
        assert!(error.to_string().contains("2-display"));
    }
}
