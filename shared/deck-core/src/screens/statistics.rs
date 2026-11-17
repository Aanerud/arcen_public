// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Stream Statistics and the session's diagnostic overlays: the compact
//! statistics box (bottom left of the picture), the lossless-tile view and
//! the health overlay. The lines themselves are
//! `arcen_media::stats_overlay`'s; the shell supplies what it measured.

use arcen_protocol::messages::HealthStatsMsg;

/// The label Stream Statistics shows for a served pipeline token.
#[must_use]
pub fn stream_pipeline_label(token: &str) -> &str {
    match token {
        "auto" => "Auto",
        "speed" => "Speed",
        "grading" => "Grading",
        "hdr" => "HDR",
        "software" => "Software",
        "custom" => "Custom",
        other => other,
    }
}

pub use crate::session_truth::served_max_fps;

/// Why the session runs at its rate, for the statistics: "your 120 Hz
/// display", or the host limit that holds it below the display.
#[must_use]
pub fn stream_rate_note(rate: &arcen_protocol::messages::StreamRateMsg) -> String {
    let basis = arcen_media::video::FrameRateBasis::from_token(&rate.basis);
    match (basis, rate.display_refresh_hz) {
        (Some(arcen_media::video::FrameRateBasis::Display), Some(refresh)) => {
            format!("your {refresh} Hz display")
        }
        (Some(basis), Some(refresh)) if rate.fps < refresh => {
            format!("limited by {} (display {refresh} Hz)", basis.describe())
        }
        (Some(basis), _) => basis.describe().to_owned(),
        (None, Some(refresh)) => format!("display {refresh} Hz"),
        (None, None) => rate.basis.clone(),
    }
}

/// The Stream Statistics line for exact still image: why it is off, or how
/// much of the picture is verified exact (`verified`, the refinement hub's
/// status for the primary monitor).
#[must_use]
pub fn exact_status_line(
    requested: bool,
    offer: Option<&arcen_protocol::refine::LosslessRefineStatusMsg>,
    verified: impl FnOnce() -> String,
) -> String {
    if !requested {
        return "Exact: off (Deck setting)".to_owned();
    }
    match offer {
        None => "Exact: not offered by this host".to_owned(),
        Some(offer) if !offer.is_on() => format!(
            "Exact: {}",
            arcen_media::refine::off_reason_text(offer.reason.as_deref().unwrap_or("off"))
        ),
        Some(_) => verified(),
    }
}

/// The compact statistics box (P2-4), bottom left of the picture.
pub fn paint_stream_stats(ui: &egui::Ui, image_rect: egui::Rect, lines: &[String]) {
    let text = lines.join("\n");
    let font = egui::FontId::monospace(12.0);
    let galley = ui
        .painter()
        .layout_no_wrap(text, font, egui::Color32::WHITE);
    let size = galley.size() + egui::vec2(20.0, 14.0);
    let origin = egui::pos2(
        image_rect.left() + 12.0,
        image_rect.bottom() - 12.0 - size.y,
    );
    let panel = egui::Rect::from_min_size(origin, size);
    ui.painter()
        .rect_filled(panel, 8.0, egui::Color32::from_black_alpha(170));
    ui.painter()
        .galley(origin + egui::vec2(10.0, 7.0), galley, egui::Color32::WHITE);
}

/// Lossless-tile debug view: green = verified source codes, amber =
/// refined but not yet verified, untinted = live (lossy video). Block
/// coordinates are source pixels, scaled to the picture.
#[allow(
    clippy::cast_precision_loss,
    reason = "block coordinates are picture pixels, far below f32's exact range"
)]
pub fn paint_lossless_tiles(
    ui: &egui::Ui,
    image_rect: egui::Rect,
    grid: &arcen_media::refine::tile::RefineGrid,
    blocks: &[Option<bool>],
) {
    if grid.width == 0 || grid.height == 0 {
        return;
    }
    let scale_x = image_rect.width() / grid.width as f32;
    let scale_y = image_rect.height() / grid.height as f32;
    for (index, state) in blocks.iter().enumerate() {
        let Some(verified) = state else { continue };
        let Some(block) = u32::try_from(index)
            .ok()
            .and_then(|index| grid.block_rect(index))
        else {
            continue;
        };
        let min =
            image_rect.left_top() + egui::vec2(block.x as f32 * scale_x, block.y as f32 * scale_y);
        let tile = egui::Rect::from_min_size(
            min,
            egui::vec2(block.width as f32 * scale_x, block.height as f32 * scale_y),
        );
        let tint = if *verified {
            egui::Color32::from_rgba_unmultiplied(40, 200, 90, 70)
        } else {
            egui::Color32::from_rgba_unmultiplied(230, 170, 30, 70)
        };
        ui.painter().rect_filled(tile, 0.0, tint);
    }
}

/// The health overlay's path row.
#[must_use]
pub fn host_path_row(stats: Option<&HealthStatsMsg>) -> String {
    match stats {
        None => "Path: waiting for health_stats".to_string(),
        Some(stats) => stats.path.as_ref().map_or_else(
            || "Path: not reported by this Pier".to_string(),
            |path| format!("Path: {}", arcen_transport::path_quality_summary(path)),
        ),
    }
}

/// The health overlay's client wire and decoder rows.
#[must_use]
pub fn client_video_health_rows(wire: &str, decoder: &str) -> (String, String) {
    (
        format!("Client wire: {wire}"),
        format!("Client decoder: {decoder}"),
    )
}

/// Everything the health overlay shows, as the shell measured it.
#[cfg(feature = "runtime")]
#[derive(Debug, Clone, Copy)]
pub struct HealthOverlayFacts<'a> {
    pub deck_build: &'a arcen_protocol::messages::BuildIdentityMsg,
    pub host_build: Option<&'a arcen_protocol::messages::BuildIdentityMsg>,
    pub host_health: Option<&'a HealthStatsMsg>,
    pub wire_summary: &'a str,
    pub decoder_summary: &'a str,
    /// Measured presentation rate, and the estimate used when there is none.
    pub presented_fps: f64,
    pub presented_fps_estimated: f64,
    pub max_fps: u32,
    /// The host's stated stream rate and its reason, when it sent one.
    pub stream_rate: Option<&'a arcen_protocol::messages::StreamRateMsg>,
    /// Per-display rates of a multi-monitor session, or empty.
    pub monitor_fps_text: &'a str,
    pub received_fps: f64,
    pub wire_delay_ms: Option<i32>,
    pub clock_age_ms: Option<i32>,
    pub last_decode_ms: f64,
    pub average_decode_ms: f64,
    pub last_upload_ms: f64,
    pub average_upload_ms: f64,
    pub video_frames_seen: u64,
    pub video_frames_decoded: u64,
    pub audio_frames_seen: u64,
    pub audio_bytes_accepted: u64,
    pub inbox: &'a crate::runtime::frame_queue::IncomingMediaTelemetry,
    pub waiting_for_keyframe: bool,
    pub ingress_idr_requests: u64,
    pub malformed_media_packets: u64,
    pub last_decode_error: Option<&'a str>,
    pub input_sequence: u64,
    pub last_input_sent_type: &'a str,
    pub last_input_sent_age: Option<std::time::Duration>,
    /// The last pointer position, normalised and in stream pixels.
    pub pointer: Option<(f64, f64, i32, i32)>,
    pub audio_summary: &'a str,
}

/// The health overlay's text.
#[cfg(feature = "runtime")]
#[must_use]
pub fn health_overlay_text(facts: &HealthOverlayFacts<'_>) -> String {
    let (host_delivery, host_video) = facts.host_health.map_or_else(
        || {
            (
                "Host delivery: waiting for health_stats".to_string(),
                "Host video contract: waiting for negotiation".to_string(),
            )
        },
        |stats| {
            (
                format!(
                    "Host delivery: {:.1}/{:.0} fps · {:.2} Mbps · stage {:.1} ms · encode {:.1} ms · drops {}",
                    stats.fps_actual,
                    stats.fps_target,
                    stats.bandwidth_mbps,
                    stats.capture_time_ms,
                    stats.encode_time_ms,
                    stats.frames_dropped,
                ),
                format!(
                    "Host video contract: codec={} · chroma={} · frame={}",
                    stats.codec, stats.chroma, stats.resolution,
                ),
            )
        },
    );
    let host_path = host_path_row(facts.host_health);
    let host_input = facts.host_health.map_or_else(
        || "Host input: waiting".to_string(),
        |stats| {
            format!(
                "Host input: events {} · last {} #{} · proc {:.2} ms",
                stats.input_events,
                if stats.last_input_type.is_empty() {
                    "-"
                } else {
                    stats.last_input_type.as_str()
                },
                stats.last_input_sequence,
                stats.input_latency_ms,
            )
        },
    );
    let pointer = facts.pointer.map_or_else(
        || "pointer outside viewer".to_string(),
        |(x, y, sx, sy)| format!("pointer {x:.3},{y:.3} => {sx},{sy}"),
    );
    let sent_age = facts.last_input_sent_age.map_or_else(
        || "never".to_string(),
        |age| format!("{:.0} ms ago", age.as_secs_f64() * 1000.0),
    );
    let local_build = facts.deck_build;
    let host_build = facts.host_build.map_or_else(
        || "Pier: build unavailable".to_string(),
        |identity| {
            format!(
                "Pier: {} · build {} · rev {} · {}",
                identity.version,
                identity.build_id,
                identity.source_revision,
                identity.feature_profile
            )
        },
    );
    let deck_build = format!(
        "Deck: {} · build {} · rev {}",
        local_build.version, local_build.build_id, local_build.source_revision,
    );
    let deck_runtime = format!(
        "Deck runtime: {} · signing {}",
        local_build.feature_profile,
        local_build.signing_state.as_deref().unwrap_or("unknown")
    );
    let (client_wire, client_decoder) =
        client_video_health_rows(facts.wire_summary, facts.decoder_summary);
    let displayed_presented_fps = if facts.presented_fps > 0.0 {
        facts.presented_fps
    } else {
        facts.presented_fps_estimated
    };
    let presented_label = if facts.presented_fps > 0.0 {
        "presented"
    } else {
        "presented estimated"
    };
    let presented_fps_text = match (facts.stream_rate, facts.max_fps) {
        (Some(rate), _) => format!(
            "{displayed_presented_fps:.1} fps (up to {} · {})",
            rate.fps,
            stream_rate_note(rate)
        ),
        (None, max_fps) if max_fps > 0 => {
            format!("{displayed_presented_fps:.1} fps (up to {max_fps})")
        }
        (None, _) => format!("{displayed_presented_fps:.1} fps"),
    };
    let wire_delay = facts
        .wire_delay_ms
        .map_or_else(|| "-".to_string(), |delay| delay.to_string());
    let clock_age = facts
        .clock_age_ms
        .map_or_else(|| "-".to_string(), |age| age.to_string());
    let client_delivery = if facts.monitor_fps_text.is_empty() {
        format!(
            "Client delivery: rx {:.1} fps · {presented_label} {presented_fps_text} · wire delay {wire_delay} ms · clock age {clock_age} ms",
            facts.received_fps,
        )
    } else {
        format!(
            "Client delivery: rx {:.1} fps · presented {presented_fps_text} · wire delay {wire_delay} ms · clock age {clock_age} ms\nDisplay fps: {}",
            facts.received_fps, facts.monitor_fps_text,
        )
    };
    let inbox = facts.inbox;
    format!(
        "{deck_build}\n{deck_runtime}\n{host_build}\n{host_delivery}\n{host_video}\n{host_path}\n{client_delivery}\n{client_wire}\n{client_decoder} · decode {:.1} ms avg {:.1} · upload {:.1} ms avg {:.1}\nVideo packets {} · presented {} · audio packets {} / {} KB\nInbox video: {}/{} B (high {}/{}) · dropped {} / {} B · superseded {} · loss epochs {}\nInbox audio: {}/{} B (high {}/{}) · drop-tail {} / {} B\nRecovery: waiting={} · ingress IDRs {} · malformed {} (video {} / audio {}) · decoder error={}\nInput sent: {} · last {} #{} ({}) · {pointer}\n{host_input}\n{}",
        facts.last_decode_ms,
        facts.average_decode_ms,
        facts.last_upload_ms,
        facts.average_upload_ms,
        facts.video_frames_seen,
        facts.video_frames_decoded,
        facts.audio_frames_seen,
        facts.audio_bytes_accepted / 1024,
        inbox.video_depth,
        inbox.video_bytes,
        inbox.video_high_water_depth,
        inbox.video_high_water_bytes,
        inbox.video_dropped_packets,
        inbox.video_dropped_bytes,
        inbox.video_superseded_packets,
        inbox.video_loss_epochs,
        inbox.audio_depth,
        inbox.audio_bytes,
        inbox.audio_high_water_depth,
        inbox.audio_high_water_bytes,
        inbox.audio_dropped_packets,
        inbox.audio_dropped_bytes,
        facts.waiting_for_keyframe,
        facts.ingress_idr_requests,
        facts.malformed_media_packets,
        inbox.malformed_video_packets,
        inbox.malformed_audio_packets,
        facts.last_decode_error.unwrap_or("-"),
        facts.input_sequence,
        if facts.last_input_sent_type.is_empty() {
            "-"
        } else {
            facts.last_input_sent_type
        },
        facts.input_sequence,
        sent_age,
        facts.audio_summary,
    )
}

/// Paints the health overlay's panel and text at the top left of `rect`.
pub fn paint_health_overlay(ui: &egui::Ui, rect: egui::Rect, text: String) {
    let overlay_width = (rect.width() - 36.0).clamp(320.0, 1040.0);
    let overlay = egui::Rect::from_min_size(
        rect.left_top() + egui::vec2(18.0, 18.0),
        egui::vec2(overlay_width, 380.0),
    );
    ui.painter().rect_filled(
        overlay,
        10.0,
        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 175),
    );
    ui.painter().text(
        overlay.left_top() + egui::vec2(14.0, 14.0),
        egui::Align2::LEFT_TOP,
        text,
        egui::FontId::monospace(13.0),
        egui::Color32::WHITE,
    );
}

/// The on-video badge that says a session is degraded (w5-negotiated-truth):
/// never behind the detail panel, but not shown for an exact session.
pub fn paint_truth_badge(
    ui: &egui::Ui,
    rect: egui::Rect,
    truth: &crate::session_truth::NegotiatedTruth,
    decoder_hardware: Option<bool>,
) {
    if !crate::session_truth::should_show_truth_badge(truth) {
        return;
    }
    let color = if truth.degradation.colour_degraded() {
        super::theme::DANGER
    } else {
        super::theme::WARNING
    };
    let text = crate::session_truth::truth_badge_text(truth, decoder_hardware);
    let badge = egui::Rect::from_min_size(
        egui::pos2(rect.center().x - 280.0, rect.top() + 8.0),
        egui::vec2(560.0, 24.0),
    );
    ui.painter()
        .rect_filled(badge, 6.0, egui::Color32::from_black_alpha(190));
    ui.painter().text(
        badge.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(13.0),
        color,
    );
}

/// A persistent presentation warning (a fidelity layer fell back), centred
/// `top` points below the top of the picture, `width` wide.
pub fn paint_presentation_warning(
    ui: &egui::Ui,
    rect: egui::Rect,
    top: f32,
    width: f32,
    text: String,
) {
    let warning = egui::Rect::from_min_size(
        egui::pos2(rect.center().x - width / 2.0, rect.top() + top),
        egui::vec2(width, 26.0),
    );
    ui.painter().rect_filled(
        warning,
        6.0,
        egui::Color32::from_rgba_unmultiplied(90, 20, 0, 220),
    );
    ui.painter().text(
        warning.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(13.0),
        super::theme::DANGER,
    );
}

/// What the Negotiated Truth panel says about decoding on this Deck.
#[derive(Debug, Clone, Copy)]
pub struct DecodeTruth<'a> {
    pub hardware: Option<bool>,
    /// The decoder that produced the last frame, or empty before one.
    pub backend: &'a str,
    /// This machine, as the platform names it.
    pub platform: &'a str,
    /// The decode path's assessment token.
    pub path: &'a str,
}

/// Full session-info detail (w5-negotiated-truth): every colour axis the
/// Deck can compare, requested versus actual, plus hardware/software for
/// encode and decode shown independently. Encode intent is on its own line:
/// no host reports it back, so there is no "actual" to compare against.
pub fn paint_negotiated_truth_panel(
    ui: &egui::Ui,
    rect: egui::Rect,
    truth: Option<&crate::session_truth::NegotiatedTruth>,
    intent: &str,
    decode: &DecodeTruth<'_>,
) {
    use crate::session_truth::{hardware_label, selection_summary};
    let overlay = egui::Rect::from_min_size(
        rect.left_bottom() + egui::vec2(18.0, -238.0),
        egui::vec2(560.0, 220.0),
    );
    ui.painter().rect_filled(
        overlay,
        10.0,
        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 190),
    );
    let Some(truth) = truth else {
        ui.painter().text(
            overlay.left_top() + egui::vec2(14.0, 14.0),
            egui::Align2::LEFT_TOP,
            "Negotiated Truth\nwaiting for server_hello",
            egui::FontId::monospace(13.0),
            egui::Color32::WHITE,
        );
        return;
    };
    let axis = |label: &str, requested: &str, actual: Option<&str>| {
        format!(
            "{label:<10} requested {requested:<9} actual {}",
            actual.unwrap_or("unknown")
        )
    };
    let text = format!(
        "Negotiated Truth (server_hello's ServerColorCaps active_* fields)\n\
         {}\n{}\n{}\n{}\n{}\n\
         Intent: {} (requested; no host reports it back)\n\
         Encode: {} ({})\n\
         Decode: {} ({}) on {}, {}\n\
         {}",
        axis(
            "Codec",
            if truth.selection
                == arcen_protocol::messages::VideoSelectionIntent::AdaptivePerformance
            {
                "adaptive"
            } else {
                truth.requested.codec.token()
            },
            truth.active.codec.map(arcen_media::VideoCodec::token),
        ),
        axis(
            "Chroma",
            truth.requested.chroma.token(),
            truth
                .active
                .chroma
                .map(arcen_media::ChromaSubsampling::token),
        ),
        axis(
            "Bit depth",
            truth.requested.bit_depth.token(),
            truth.active.bit_depth.map(arcen_media::BitDepth::token),
        ),
        axis(
            "Range",
            truth.requested.range.token(),
            truth.active.range.map(arcen_media::ColorRange::token),
        ),
        axis(
            "Matrix",
            truth.requested.matrix.token(),
            truth.active.matrix.map(arcen_media::ColorMatrix::token),
        ),
        intent,
        hardware_label(truth.encoder_hardware),
        truth.encoder_backend,
        hardware_label(decode.hardware),
        if decode.backend.is_empty() {
            "no frame decoded yet"
        } else {
            decode.backend
        },
        decode.platform,
        decode.path,
        selection_summary(truth.selection, truth.degradation),
    );
    ui.painter().text(
        overlay.left_top() + egui::vec2(14.0, 14.0),
        egui::Align2::LEFT_TOP,
        text,
        egui::FontId::monospace(13.0),
        egui::Color32::WHITE,
    );
}

/// What the Tablet Monitor panel shows (View ▸ Tablet Monitor): only
/// in-memory state of the current session, never logged or persisted.
#[derive(Debug, Clone, Copy)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one line of the panel per flag"
)]
pub struct TabletMonitorFacts<'a> {
    /// A pen tablet on this machine.
    pub tablet_presence: arcen_input::CapabilityAvailability,
    /// The typed pen this Deck offered at connect.
    pub local_pen_offered: arcen_protocol::messages::InputCapabilityAvailability,
    /// The host's pen capability from its hello.
    pub host_pen: arcen_protocol::messages::InputCapabilityAvailability,
    /// The host confirmed the typed pen.
    pub pen_negotiated: bool,
    /// The active tablet mode lets a typed pen through.
    pub typed_pen_allowed: bool,
    /// The platform's tablet capture is installed.
    pub runtime_installed: bool,
    pub mode_requested: arcen_protocol::messages::TabletModeMsg,
    pub mode_active: arcen_protocol::messages::TabletModeMsg,
    pub mode_reason: &'a str,
    pub last_event: Option<&'a arcen_input::PenEvent>,
    pub events_sent: u64,
    pub overflow_resets: u64,
    /// Samples the capture dropped because its queue was full.
    pub producer_dropped: u64,
    pub dispatcher_edges: u64,
    pub motion_forwarded: u64,
    pub dispatcher_overflow: u64,
    /// Which pen axes this machine's tablet has been seen to deliver.
    pub probe_pressure: arcen_input::CapabilityAvailability,
    pub probe_tilt: arcen_input::CapabilityAvailability,
    pub probe_rotation: arcen_input::CapabilityAvailability,
    pub probe_eraser: arcen_input::CapabilityAvailability,
    pub probe_barrel_buttons: arcen_input::CapabilityAvailability,
    pub last_error: Option<&'a str>,
}

/// The Tablet Monitor panel, at the top right of `rect`: device presence,
/// negotiated fields, tool/proximity, event counters and the live pressure,
/// tilt and rotation, for checking the tablet path by hand.
pub fn paint_tablet_monitor_panel(ui: &egui::Ui, rect: egui::Rect, facts: &TabletMonitorFacts<'_>) {
    use arcen_input::CapabilityAvailability as Local;
    use arcen_protocol::messages::InputCapabilityAvailability as Wire;
    let overlay = egui::Rect::from_min_size(
        rect.right_top() + egui::vec2(-420.0, 18.0),
        egui::vec2(400.0, 370.0),
    );
    ui.painter().rect_filled(
        overlay,
        10.0,
        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 175),
    );
    let presence = match facts.tablet_presence {
        Local::Available => "present",
        Local::Unavailable => "not detected",
        Local::Unknown => "unknown",
    };
    let presence_at_connect = match facts.local_pen_offered {
        Wire::Available => "yes",
        Wire::Unavailable => "no (setting off)",
        Wire::Unknown => "unknown (no active session)",
    };
    let host_pen_cap = match facts.host_pen {
        Wire::Available => "available",
        Wire::Unavailable => "unavailable (uinput may have failed)",
        Wire::Unknown => "unknown (no active session)",
    };
    let negotiated = if facts.pen_negotiated {
        "yes (typed pen)"
    } else if facts.typed_pen_allowed {
        "no — host did not confirm pen capability\n\
         (check pier log: uinput tablet creation may have failed;\n\
          try: modprobe uinput and verify /dev/uinput permissions)"
    } else {
        "no (setting disabled)"
    };
    let (tool, in_proximity, touching, pressure, tilt_x, tilt_y, rotation) = facts
        .last_event
        .map_or(("-", false, false, 0.0, 0.0, 0.0, 0.0), |event| {
            (
                match event.tool {
                    arcen_input::PenTool::Tip => "tip",
                    arcen_input::PenTool::Eraser => "eraser",
                },
                event.in_proximity,
                event.touching,
                event.pressure,
                event.tilt_x_degrees,
                event.tilt_y_degrees,
                event.rotation_degrees,
            )
        });
    let text = format!(
        "Tablet Monitor (local-only; never logged/persisted)\n\
         Pen tablet: {presence}\n\
         Typed pen offered: {presence_at_connect}  ·  Host pen cap: {host_pen_cap}\n\
         Runtime installed: {}\n\
         Mode requested: {:?}\n\
         Mode active: {:?}\n\
         Mode reason: {}\n\
         Negotiated: {negotiated}\n\
         Tool: {tool}  ·  in proximity: {in_proximity}  ·  touching: {touching}\n\
         Pressure: {pressure:.3}  ·  tilt {tilt_x:.1}°/{tilt_y:.1}°  ·  rotation {rotation:.1}°\n\
         Events sent: {}  ·  overflow resets: {}\n\
         Producer dropped (queue full): {}\n\
         Dispatcher edges: {}  ·  motion forwarded: {}  ·  overflow: {}\n\
         Probe — pressure: {:?}  tilt: {:?}  rotation: {:?}  eraser: {:?}  barrel: {:?}\n\
         Last error: {}",
        facts.runtime_installed,
        facts.mode_requested,
        facts.mode_active,
        if facts.mode_reason.is_empty() {
            "-"
        } else {
            facts.mode_reason
        },
        facts.events_sent,
        facts.overflow_resets,
        facts.producer_dropped,
        facts.dispatcher_edges,
        facts.motion_forwarded,
        facts.dispatcher_overflow,
        facts.probe_pressure,
        facts.probe_tilt,
        facts.probe_rotation,
        facts.probe_eraser,
        facts.probe_barrel_buttons,
        facts.last_error.unwrap_or("-"),
    );
    ui.painter().text(
        overlay.left_top() + egui::vec2(14.0, 14.0),
        egui::Align2::LEFT_TOP,
        text,
        egui::FontId::monospace(12.0),
        egui::Color32::WHITE,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_protocol::messages::{ServerHelloMsg, StreamRateMsg};

    fn hello(rate: Option<StreamRateMsg>) -> ServerHelloMsg {
        let mut hello: ServerHelloMsg =
            serde_json::from_str(r#"{"type":"server_hello","codec":"h265"}"#).expect("hello");
        hello.stream_rate = rate;
        hello
    }

    #[test]
    fn the_served_rate_and_its_reason_come_from_the_host() {
        let older = hello(None);
        assert_eq!(served_max_fps(Some(&older), 30), 30);
        assert_eq!(served_max_fps(None, 30), 30);
        let display = StreamRateMsg {
            fps: 120,
            display_refresh_hz: Some(120),
            basis: "display".to_owned(),
        };
        assert_eq!(served_max_fps(Some(&hello(Some(display.clone()))), 30), 120);
        assert_eq!(stream_rate_note(&display), "your 120 Hz display");
        let encoder = StreamRateMsg {
            fps: 60,
            display_refresh_hz: Some(144),
            basis: "encoder".to_owned(),
        };
        assert_eq!(
            stream_rate_note(&encoder),
            "limited by the host's encoder (display 144 Hz)"
        );
    }
}
