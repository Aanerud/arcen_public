// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Deck's settings model: what `settings.json` holds and the
//! choices behind it (streaming presets, colour fidelity, displays, logs,
//! security). Moved from the macOS Deck (ADR 0024 extraction step 2); the
//! wording is the Deck's and is shared by every Deck.
//!
//! On-disk compatibility is pinned by the golden fixtures in
//! `shared/deck-core/tests/fixtures/deck-config/`.

#![allow(
    clippy::missing_errors_doc,
    reason = "every fallible function here fails with ClientConfigError, which documents each case"
)]

use std::time::{Duration, Instant};

use arcen_protocol::messages::{CursorMode, TabletModeMsg};

use crate::config::{SavedConnectionSummary, load_saved_connections, write_saved_connections};

/// Log verbosity, matching the reference client's Level 0..3 scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Critical,
    Error,
    Info,
    Debug,
}

impl LogLevel {
    pub const ALL: [LogLevel; 4] = [
        LogLevel::Critical,
        LogLevel::Error,
        LogLevel::Info,
        LogLevel::Debug,
    ];

    #[must_use]
    pub fn index(self) -> u8 {
        match self {
            LogLevel::Critical => 0,
            LogLevel::Error => 1,
            LogLevel::Info => 2,
            LogLevel::Debug => 3,
        }
    }

    #[must_use]
    pub fn from_index(value: u8) -> Self {
        match value {
            0 => LogLevel::Critical,
            1 => LogLevel::Error,
            3 => LogLevel::Debug,
            _ => LogLevel::Info,
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            LogLevel::Critical => "Level 0 Critical",
            LogLevel::Error => "Level 1 Error",
            LogLevel::Info => "Level 2 Info",
            LogLevel::Debug => "Level 3 Debug",
        }
    }

    #[must_use]
    pub fn profile(self) -> arcen_telemetry::OperationalProfile {
        match self {
            LogLevel::Critical => arcen_telemetry::OperationalProfile::Critical,
            LogLevel::Error => arcen_telemetry::OperationalProfile::Error,
            LogLevel::Info => arcen_telemetry::OperationalProfile::Info,
            LogLevel::Debug => arcen_telemetry::OperationalProfile::Debug,
        }
    }
}

/// Certificate verification strictness for outgoing connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityMode {
    High,
    Medium,
    Low,
}

impl SecurityMode {
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            SecurityMode::High => "high",
            SecurityMode::Medium => "medium",
            SecurityMode::Low => "low",
        }
    }

    #[must_use]
    pub fn from_key(value: &str) -> Self {
        match value {
            "high" => SecurityMode::High,
            "low" => SecurityMode::Low,
            _ => SecurityMode::Medium,
        }
    }
}

/// Preferred frame-rate/latency profile for the video stream.
///
/// This used to also pick the codec and chroma subsampling (High = HEVC
/// 4:4:4, the other two modes = H.264 4:2:0), back when there was only one
/// axis of "how much colour fidelity". Now that [`ColorFidelity`] owns the
/// full codec/chroma/bit-depth/range/matrix contract as its own independent
/// setting, `PerformanceMode` keeps exactly the one thing that is genuinely
/// about performance rather than colour: the frame-rate ceiling. See
/// `connect_options_with_stream_sizing_policy`, which is the one place these
/// two settings still meet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerformanceMode {
    Standard,
    High,
    HighLegacy,
}

impl PerformanceMode {
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            PerformanceMode::Standard => "standard",
            PerformanceMode::High => "high",
            PerformanceMode::HighLegacy => "high_legacy",
        }
    }

    #[must_use]
    pub fn from_key(value: &str) -> Self {
        match value {
            "high" => PerformanceMode::High,
            "high_legacy" => PerformanceMode::HighLegacy,
            _ => PerformanceMode::Standard,
        }
    }

    /// Frame-rate ceiling for GUI-initiated sessions. Standard trades peak
    /// smoothness for steadier cadence; High/Legacy run the full 60 fps path.
    ///
    /// This is the only thing `PerformanceMode` still controls: `High` and
    /// `HighLegacy` are now behaviourally identical (both 60 fps, with colour
    /// fully decoupled into `ColorFidelity`). `HighLegacy` is kept only so a
    /// `settings.json` persisted before this change keeps loading to the same
    /// frame-rate ceiling it always had; there is no reason for a new choice
    /// to prefer it over `High`.
    #[must_use]
    pub fn max_fps(self) -> u32 {
        match self {
            PerformanceMode::Standard => 30,
            PerformanceMode::High | PerformanceMode::HighLegacy => 60,
        }
    }
}

#[must_use]
pub const fn streaming_contract(
    preset: arcen_media::video::StreamingPreset,
) -> arcen_media::video::PresetContract {
    arcen_media::video::contract(preset)
}

/// Colour-fidelity preset for the video stream. Standard fixes the ordinary
/// 4:2:0/8-bit colour axes while authorizing host-ranked AV1/HEVC/H.264;
/// higher presets request concrete HEVC 4:4:4 contracts.
///
/// `Standard` reproduces Arcen's historical contract exactly (H.264 4:2:0
/// 8-bit limited range), so it is both the default for a new install and
/// what every settings.json written before this setting existed resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorFidelity {
    /// Adaptive hardware codec, 4:2:0 8-bit limited range. The H.264 value in
    /// `preset_configuration` is the legacy compatibility preference; the
    /// auth-time selection intent authorizes the Pier to choose AV1/HEVC/H.264.
    Standard,
    /// HEVC 4:4:4 8-bit full range -- no chroma subsampling and the full
    /// code range, at the original 8-bit depth.
    FullColour,
    /// HEVC 4:4:4 10-bit full range BT.709 -- the grader/VFX-grade target
    /// contract (`arcen_media::VideoConfiguration::grading_reference`).
    GradingReference,
    /// HEVC 4:4:4 10-bit full range, **PQ over BT.2020** -- the HDR10 ask.
    ///
    /// The only preset whose transfer is not an SDR curve, and therefore the
    /// only one that asks a host to compose, capture and encode the desktop
    /// wide. `GradingReference` is already 10-bit; depth is not what makes
    /// this HDR, the transfer is.
    Hdr10,
}

impl ColorFidelity {
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            ColorFidelity::Standard => "standard",
            ColorFidelity::FullColour => "full_colour",
            ColorFidelity::GradingReference => "grading_reference",
            ColorFidelity::Hdr10 => "hdr10",
        }
    }

    #[must_use]
    pub fn from_key(value: &str) -> Self {
        match value {
            "full_colour" => ColorFidelity::FullColour,
            "grading_reference" => ColorFidelity::GradingReference,
            "hdr10" => ColorFidelity::Hdr10,
            _ => ColorFidelity::Standard,
        }
    }

    /// One-line description of this preset's contract, suitable for the
    /// Advanced settings UI (see `settings_radio`).
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            ColorFidelity::Standard => "Automatic performance codec, 4:2:0 8-bit limited range",
            ColorFidelity::FullColour => "4:4:4 8-bit full range -- no chroma subsampling",
            ColorFidelity::GradingReference => {
                "4:4:4 10-bit full range, BT.709 -- highest-fidelity SDR. The extra depth \
                 absorbs rounding, not brightness; added latency"
            }
            ColorFidelity::Hdr10 => {
                "4:4:4 10-bit, PQ over BT.2020 -- asks the host to compose, capture and \
                 stream in HDR. Hosts without a proven HDR desktop provider visibly degrade \
                 to Grading Reference SDR"
            }
        }
    }

    /// Short display name for this preset, used as the settings-radio title
    /// in the Colour fidelity section (`describe` is that radio's longer
    /// body text).
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            ColorFidelity::Standard => "Standard (Adaptive)",
            ColorFidelity::FullColour => "Full Colour (SDR)",
            ColorFidelity::GradingReference => "Grading Reference (SDR)",
            ColorFidelity::Hdr10 => "HDR10 (wide)",
        }
    }

    /// Encoder effort follows the user-facing fidelity choice. Grading is the
    /// only preset that intentionally trades interaction latency for image
    /// quality; ordinary and full-colour work remain latency-first.
    #[must_use]
    pub const fn encode_intent(self) -> arcen_media::EncodeIntent {
        match self {
            Self::GradingReference | Self::Hdr10 => arcen_media::EncodeIntent::Quality,
            Self::Standard | Self::FullColour => arcen_media::EncodeIntent::Interactive,
        }
    }

    /// The compatibility codec preference plus complete colour axes this
    /// preset asks for, before any Advanced axis override in
    /// [`ColorFidelityOverrides`] is applied.
    #[must_use]
    pub fn preset_configuration(self) -> arcen_media::VideoConfiguration {
        match self {
            ColorFidelity::Standard => arcen_media::VideoConfiguration::legacy_h264(),
            ColorFidelity::FullColour => arcen_media::VideoConfiguration {
                codec: arcen_media::VideoCodec::H265,
                chroma: arcen_media::ChromaSubsampling::Yuv444,
                bit_depth: arcen_media::BitDepth::Eight,
                range: arcen_media::ColorRange::Full,
                matrix: arcen_media::ColorMatrix::Bt709,
                primaries: arcen_media::ColorPrimaries::Bt709,
                transfer: arcen_media::TransferCharacteristics::Bt709,
            },
            ColorFidelity::GradingReference => arcen_media::VideoConfiguration::grading_reference(),
            ColorFidelity::Hdr10 => arcen_media::VideoConfiguration {
                codec: arcen_media::VideoCodec::H265,
                chroma: arcen_media::ChromaSubsampling::Yuv444,
                bit_depth: arcen_media::BitDepth::Ten,
                range: arcen_media::ColorRange::Full,
                matrix: arcen_media::ColorMatrix::Bt2020Ncl,
                primaries: arcen_media::ColorPrimaries::Bt2020,
                transfer: arcen_media::TransferCharacteristics::Pq,
            },
        }
    }
}

/// Advanced per-axis overrides on top of a [`ColorFidelity`] preset.
///
/// Every axis defaults to `None`, meaning "follow the preset". Setting an
/// axis to `Some` marks it as the user's explicit choice, distinct from
/// whatever the preset itself asks for on that axis, so a later preset
/// change -- which only ever replaces an axis this struct leaves `None` --
/// can never silently discard it. See
/// [`ColorFidelitySettings::resolved_variant`] for the full precedence rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ColorFidelityOverrides {
    pub codec: Option<arcen_media::VideoCodec>,
    pub chroma: Option<arcen_media::ChromaSubsampling>,
    pub bit_depth: Option<arcen_media::BitDepth>,
    pub range: Option<arcen_media::ColorRange>,
    pub matrix: Option<arcen_media::ColorMatrix>,
}

impl ColorFidelityOverrides {
    /// Applies whichever axes this struct sets on top of `base`, leaving
    /// `base`'s own value for every axis left `None`. Colour primaries and
    /// transfer characteristics are not an overridable axis here and always
    /// come from `base`.
    #[must_use]
    pub fn apply_to(
        self,
        base: arcen_media::VideoConfiguration,
    ) -> arcen_media::VideoConfiguration {
        arcen_media::VideoConfiguration {
            codec: self.codec.unwrap_or(base.codec),
            chroma: self.chroma.unwrap_or(base.chroma),
            bit_depth: self.bit_depth.unwrap_or(base.bit_depth),
            range: self.range.unwrap_or(base.range),
            matrix: self.matrix.unwrap_or(base.matrix),
            ..base
        }
    }
}

/// A client's full colour-fidelity choice: a [`ColorFidelity`] preset, any
/// Advanced per-axis [`ColorFidelityOverrides`] on top of it, and an optional
/// explicit probe-matrix variant that bypasses both.
///
/// Persisted as part of [`ClientSettings`]; see `load_client_settings` and
/// `save_client_settings`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorFidelitySettings {
    pub preset: ColorFidelity,
    pub overrides: ColorFidelityOverrides,
    /// Explicit variant to stream, identified by its stable
    /// `arcen_media::video::VideoVariant` id (e.g. `hevc-444-10-full-bt709`).
    /// Typically one of the 11 `arcen_media::video::PROBE_MATRIX` rows, so
    /// the w0 testing machine can drive any of them directly from
    /// settings.json, bypassing `overrides` and `preset` entirely -- though
    /// any coherent id is accepted, not only those 11. `None` for ordinary
    /// use.
    pub variant_override: Option<arcen_media::video::VideoVariant>,
}

impl Default for ColorFidelitySettings {
    fn default() -> Self {
        Self {
            preset: ColorFidelity::Standard,
            overrides: ColorFidelityOverrides::default(),
            variant_override: None,
        }
    }
}

impl ColorFidelitySettings {
    #[must_use]
    pub fn video_selection(self) -> arcen_protocol::messages::VideoSelectionIntent {
        if cfg!(feature = "dev-tools")
            && (self.variant_override.is_some()
                || self.overrides.codec.is_some()
                || self
                    .overrides
                    .chroma
                    .is_some_and(|chroma| chroma != arcen_media::ChromaSubsampling::Yuv420))
        {
            return arcen_protocol::messages::VideoSelectionIntent::Exact;
        }
        match self.preset {
            ColorFidelity::Standard => {
                arcen_protocol::messages::VideoSelectionIntent::AdaptivePerformance
            }
            ColorFidelity::FullColour | ColorFidelity::GradingReference | ColorFidelity::Hdr10 => {
                arcen_protocol::messages::VideoSelectionIntent::ColorFidelity
            }
        }
    }

    /// Resolves the effective video contract, applying precedence:
    /// `variant_override` (an explicit probe-matrix row) beats any
    /// individual `overrides` axis, which in turn beats `preset`. An axis
    /// `overrides` leaves `None` falls through to the preset's own value for
    /// that axis.
    ///
    /// `variant_override` is always coherent -- it can only ever have been
    /// populated by a successful `VideoVariant::from_id`, which checks this
    /// itself. Individual `overrides` axes are *not* cross-checked against
    /// each other here, though: each axis is applied independently, so a
    /// combination the caller picks one axis at a time (e.g. the Standard
    /// preset's 4:2:0 chroma plus an identity-matrix override, with no
    /// accompanying chroma override) can still resolve to a
    /// [`arcen_media::video::VideoVariant`] that is not offered. Call
    /// `.is_coherent()` on the result before relying on it for anything that
    /// must actually stream.
    #[must_use]
    pub fn resolved_variant(self) -> arcen_media::video::VideoVariant {
        if let Some(variant) = self.variant_override {
            return variant;
        }
        let configuration = self.overrides.apply_to(self.preset.preset_configuration());
        arcen_media::video::VideoVariant::new(configuration)
    }
}

/// The four ordinary streaming choices shown by the Deck.
///
/// The persisted performance and colour settings remain separate for
/// backwards compatibility and for developer-only axis overrides. This type
/// is the simple product surface that maps each user choice to one complete,
/// coherent pair of settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamingPreset {
    Auto,
    Speed,
    Grading,
    Hdr,
    Custom,
}

impl StreamingPreset {
    pub const PRIMARY: [Self; 4] = [Self::Auto, Self::Speed, Self::Grading, Self::Hdr];

    #[must_use]
    pub fn from_settings(
        performance: PerformanceMode,
        color: ColorFidelitySettings,
    ) -> StreamingPreset {
        if color.overrides != ColorFidelityOverrides::default() || color.variant_override.is_some()
        {
            return Self::Custom;
        }
        match (performance, color.preset) {
            (PerformanceMode::Standard, ColorFidelity::Standard) => Self::Auto,
            (PerformanceMode::High | PerformanceMode::HighLegacy, ColorFidelity::Standard) => {
                Self::Speed
            }
            (PerformanceMode::Standard, ColorFidelity::GradingReference) => Self::Grading,
            (PerformanceMode::Standard, ColorFidelity::Hdr10) => Self::Hdr,
            _ => Self::Custom,
        }
    }

    pub fn apply_to(self, performance: &mut PerformanceMode, color: &mut ColorFidelitySettings) {
        let (next_performance, next_color) = match self {
            Self::Auto => (PerformanceMode::Standard, ColorFidelity::Standard),
            Self::Speed => (PerformanceMode::High, ColorFidelity::Standard),
            Self::Grading => (PerformanceMode::Standard, ColorFidelity::GradingReference),
            Self::Hdr => (PerformanceMode::Standard, ColorFidelity::Hdr10),
            Self::Custom => return,
        };
        *performance = next_performance;
        *color = ColorFidelitySettings {
            preset: next_color,
            overrides: ColorFidelityOverrides::default(),
            variant_override: None,
        };
    }

    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Speed => "Speed",
            Self::Grading => "Grading",
            Self::Hdr => "HDR",
            Self::Custom => "Custom",
        }
    }

    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Auto => streaming_contract(arcen_media::video::StreamingPreset::Auto).summary,
            Self::Speed => streaming_contract(arcen_media::video::StreamingPreset::Speed).summary,
            Self::Grading => {
                streaming_contract(arcen_media::video::StreamingPreset::Grading).summary
            }
            Self::Hdr => streaming_contract(arcen_media::video::StreamingPreset::Hdr).summary,
            Self::Custom => "Developer-defined streaming configuration.",
        }
    }

    #[must_use]
    pub const fn contract(self) -> Option<arcen_media::video::PresetContract> {
        match self {
            Self::Auto => Some(streaming_contract(
                arcen_media::video::StreamingPreset::Auto,
            )),
            Self::Speed => Some(streaming_contract(
                arcen_media::video::StreamingPreset::Speed,
            )),
            Self::Grading => Some(streaming_contract(
                arcen_media::video::StreamingPreset::Grading,
            )),
            Self::Hdr => Some(streaming_contract(arcen_media::video::StreamingPreset::Hdr)),
            Self::Custom => None,
        }
    }

    #[must_use]
    pub const fn wire_pipeline(self) -> Option<arcen_protocol::messages::StreamPipeline> {
        match self {
            Self::Auto => Some(arcen_protocol::messages::StreamPipeline::Auto),
            Self::Speed => Some(arcen_protocol::messages::StreamPipeline::Speed),
            Self::Grading => Some(arcen_protocol::messages::StreamPipeline::Grading),
            Self::Hdr => Some(arcen_protocol::messages::StreamPipeline::Hdr),
            Self::Custom => None,
        }
    }
}

/// The exact contract a connection will request on the wire: `settings`'s own
/// `resolved_variant()` when that is a combination Arcen actually offers, or
/// -- when it is not, per `resolved_variant`'s documented gap where
/// individual `overrides` axes are not cross-validated against each other --
/// `settings.preset`'s own contract instead, which
/// `color_fidelity_preset_maps_to_documented_contract` pins as always
/// coherent standing alone.
///
/// Shared by the Advanced settings screen's live "Resolving to" read-out
/// (`settings_screen`) and `connect_options_with_stream_sizing_policy`, so
/// neither can ever disagree with the other about what a connection is about
/// to request, and so an incoherent Advanced axis combination can never
/// reach the host.
#[must_use]
pub fn effective_color_fidelity_variant(
    settings: ColorFidelitySettings,
) -> arcen_media::video::VideoVariant {
    if !cfg!(feature = "dev-tools") {
        return arcen_media::video::VideoVariant::new(settings.preset.preset_configuration());
    }
    let resolved = settings.resolved_variant();
    if resolved.is_coherent() {
        resolved
    } else {
        arcen_media::video::VideoVariant::new(settings.preset.preset_configuration())
    }
}

#[must_use]
pub fn color_fidelity_codec_label(codec: arcen_media::VideoCodec) -> &'static str {
    match codec {
        arcen_media::VideoCodec::Jpeg => "JPEG",
        arcen_media::VideoCodec::H264 => "H.264",
        arcen_media::VideoCodec::H265 => "HEVC (H.265)",
        arcen_media::VideoCodec::Vp9 => "VP9",
        arcen_media::VideoCodec::Av1 => "AV1",
    }
}

/// Human-readable label for a chroma axis choice.
#[must_use]
pub fn color_fidelity_chroma_label(chroma: arcen_media::ChromaSubsampling) -> &'static str {
    match chroma {
        arcen_media::ChromaSubsampling::Yuv420 => "4:2:0",
        arcen_media::ChromaSubsampling::Yuv422 => "4:2:2",
        arcen_media::ChromaSubsampling::Yuv444 => "4:4:4",
    }
}

/// Human-readable label for a bit-depth axis choice.
#[must_use]
pub fn color_fidelity_bit_depth_label(bit_depth: arcen_media::BitDepth) -> String {
    format!("{}-bit", bit_depth.bits())
}

/// Human-readable label for a colour-range axis choice.
#[must_use]
pub fn color_fidelity_range_label(range: arcen_media::ColorRange) -> &'static str {
    match range {
        arcen_media::ColorRange::Limited => "Limited range",
        arcen_media::ColorRange::Full => "Full range",
    }
}

/// Human-readable label for a matrix axis choice.
#[must_use]
pub fn color_fidelity_matrix_label(matrix: arcen_media::ColorMatrix) -> &'static str {
    match matrix {
        arcen_media::ColorMatrix::Identity => "Identity (GBR)",
        arcen_media::ColorMatrix::Bt709 => "BT.709",
        arcen_media::ColorMatrix::Bt601 => "BT.601",
        arcen_media::ColorMatrix::Bt2020Ncl => "BT.2020 NCL",
    }
}

/// How the client wants the host to size the streamed desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplaysMode {
    /// Match every active local display to one host output, up to four.
    MatchLayout,
    /// Stream only the client's primary display, at its native resolution.
    SinglePrimary,
    /// Single stream shown in a resizable window (fit-to-window on the client).
    Windowed,
}

impl DisplaysMode {
    pub const WIRE_VALUES: [&'static str; 3] = ["match_layout", "single_primary", "windowed"];

    pub const ALL: [DisplaysMode; 3] = [
        DisplaysMode::MatchLayout,
        DisplaysMode::SinglePrimary,
        DisplaysMode::Windowed,
    ];

    /// Wire token sent to the host in the auth response.
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            DisplaysMode::MatchLayout => "match_layout",
            DisplaysMode::SinglePrimary => "single_primary",
            DisplaysMode::Windowed => "windowed",
        }
    }

    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "match_layout" => Some(DisplaysMode::MatchLayout),
            "single_primary" => Some(DisplaysMode::SinglePrimary),
            "windowed" => Some(DisplaysMode::Windowed),
            _ => None,
        }
    }

    #[must_use]
    pub fn key(self) -> &'static str {
        self.as_wire()
    }

    #[must_use]
    pub fn from_key(value: &str) -> Self {
        match value {
            "single_primary" => DisplaysMode::SinglePrimary,
            "windowed" => DisplaysMode::Windowed,
            _ => DisplaysMode::MatchLayout,
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            DisplaysMode::MatchLayout => "Match my layout",
            DisplaysMode::SinglePrimary => "Primary display only",
            DisplaysMode::Windowed => "Windowed",
        }
    }

    /// One-line explanation shown under the selector in the Displays panel.
    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            DisplaysMode::MatchLayout => {
                "Match up to 4 of your displays with real native fullscreen windows on a \
                 host that supports it, each pixel-exact. Requires Separate Spaces on and \
                 standard fullscreen (Notch area off) locally, and requires the host to \
                 support and accept it. If the host doesn't support multi-monitor, or the \
                 local layout can't be applied, the connection fails, with an error that \
                 clearly explains why, instead of silently falling back to the primary \
                 display — switch to Primary Display Only and reconnect to use this host."
            }
            DisplaysMode::SinglePrimary => {
                "Stream only your primary display, sized to what the Deck can show in \
                 fullscreen so the image is pixel-exact. The stream does not resize \
                 with the app window."
            }
            DisplaysMode::Windowed => {
                "Stream your primary display into a resizable window. The image \
                 scales to fit the window instead of taking over a whole monitor."
            }
        }
    }
}

/// Client preferences. Microphone consent is intentionally runtime-only; all
/// other fields are persisted to `<config>/settings.json`.
///
/// USB policy is deliberate: HID devices (keyboard, mouse, pen tablet stylus)
/// always ride the low-latency input channel natively — the lesson learned
/// from Sunshine. Only NON-HID devices (tablet control surfaces, USB
/// flash drives) use bridged auto-forwarding, which adds WAN latency, so it
/// stays OFF unless the user explicitly opts in.
#[derive(Debug, Clone, Copy)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent persisted switches"
)]
pub struct ClientSettings {
    pub log_level: LogLevel,
    pub security_mode: SecurityMode,
    pub performance_mode: PerformanceMode,
    pub displays_mode: DisplaysMode,
    pub cursor_preference: CursorMode,
    pub clipboard_enabled: bool,
    pub microphone_enabled: bool,
    pub hidpi_streaming: bool,
    /// Whether a pinned fullscreen session may cover the notch region on a
    /// notched Mac.
    ///
    /// Off (default): the stream is sized to the safe area, so it fills the
    /// whole presentable area 1:1 with no bars and no downscale; the notch
    /// strip stays black because it is a hole in the panel. On: the stream is
    /// sized to the full panel, leaving no black at all, but the notch
    /// physically covers a slice of the remote desktop's top edge.
    pub fullscreen_uses_notch_area: bool,
    /// Windows display-scaling percentage to aim the remote desktop at, or 0
    /// for the panel's real measurements.
    pub remote_ui_scale_percent: u32,
    /// Primary-first/spatial display slots that require full-color 4:4:4.
    /// Zero preserves the ordinary host-default connection profile.
    pub full_color_display_mask: u8,
    /// Primary-first/spatial display slots that stream at their own backing
    /// (`HiDPI`) resolution instead of point size.
    ///
    /// A point-sized stream sends the display's *logical* size, so a Retina
    /// panel showing 1800 points across a large physical screen produces a
    /// 1800-pixel remote desktop that Windows then renders at 100% -- which
    /// is why the Philips looked oversized on 2026-08-11. Streaming the
    /// backing pixels instead lets the remote OS apply its own scaling and
    /// land at the right apparent size.
    ///
    /// Per display rather than one session-wide switch (`hidpi_streaming`,
    /// which this supersedes for multi-monitor sessions): the cost is
    /// roughly four times the pixels for a 2x panel, so it must be
    /// affordable per monitor rather than all-or-nothing. Zero -- the
    /// default -- keeps every monitor point-sized, exactly as before.
    pub hidpi_display_mask: u8,
    /// Local opt-in for typed `AppKit` tablet/pen input (`tablet` module local
    /// termination). Defaults to `true`: users with no pen tablet are
    /// unaffected (capability probing still reports `Unavailable`), while
    /// pen tablet owners get typed pen support without an extra opt-in step. A
    /// user who wants to force the tablet driver's mouse-emulation fallback can
    /// still turn this off explicitly in Advanced settings.
    pub tablet_input_enabled: bool,
    pub tablet_mode_requested: TabletModeMsg,
    /// What the pen's barrel buttons click on the host (Tablet support).
    pub pen_buttons: arcen_input::pen_buttons::PenButtonSettings,
    /// Exact still image (lossless refinement, ADR 0016): ask the host to
    /// send still pictures' exact pixels. On by default; the host decides
    /// whether it offers it.
    pub exact_still_image: bool,
    /// Canvas screen updates (ADR 0028): with a host that encodes in
    /// software, keep the picture on this Deck and receive only what
    /// changed. On by default; asked for only where this Deck's presenter
    /// has a canvas executor, and the host decides whether it serves it.
    pub canvas: bool,
    /// The canvas cache budget in MiB (default
    /// [`crate::canvas::DEFAULT_CACHE_MIB`]); never advertised below
    /// [`crate::canvas::MIN_CACHE_MIB`].
    pub canvas_cache_mib: u32,
    /// Colour-fidelity preset plus any Advanced overrides and explicit
    /// probe-matrix variant. See `ColorFidelitySettings::resolved_variant`
    /// for how these combine into the effective video contract.
    pub color_fidelity: ColorFidelitySettings,
    /// What the encoder should optimise for. Deliberately kept out of
    /// [`ColorFidelitySettings`]: colour is a contract the host negotiates
    /// and reports back, while intent is a one-way knob that never changes
    /// which pixels are asked for (see [`arcen_media::EncodeIntent`]).
    pub encode_intent: arcen_media::EncodeIntent,
    /// How typed characters reach a host that takes committed text
    /// (`arcen_input::text::TypingMode`; Auto by default).
    pub typing_mode: arcen_input::text::TypingMode,
    /// What each Option key does: Automatic by default (a Mac host keeps
    /// both as Option; other hosts get left Alt, right characters).
    pub option_key_mode: arcen_input::text::OptionKeyPreference,
    /// Paste as Keystrokes: confirmation threshold and typing rate.
    pub paste: arcen_input::text::PasteSettings,
    /// When Alt+Tab, the Windows key and Print Screen go to the host
    /// (`arcen_input::keyboard::KeyboardCapturePolicy`; the host whenever a
    /// session window has focus by default).
    pub keyboard_capture: arcen_input::keyboard::KeyboardCapturePolicy,
}

pub const SAVED_CONNECTIONS_VERSION: u32 = 2;
pub const SAVED_CONNECTION_SETTINGS_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct SavedConnectionsStore {
    pub version: u32,
    pub connections: Vec<SavedConnectionSummary>,
    pub unparsed_connections: Vec<serde_json::Value>,
    pub last_selected_connection_id: Option<String>,
    pub legacy_username_fallback: Option<String>,
    pub unknown: serde_json::Map<String, serde_json::Value>,
    pub fingerprint: Option<[u8; 32]>,
}

impl SavedConnectionsStore {
    #[must_use]
    pub fn with_connections(connections: Vec<SavedConnectionSummary>) -> Self {
        Self {
            version: SAVED_CONNECTIONS_VERSION,
            connections,
            unparsed_connections: Vec::new(),
            last_selected_connection_id: None,
            legacy_username_fallback: None,
            unknown: serde_json::Map::new(),
            fingerprint: None,
        }
    }
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            log_level: if cfg!(debug_assertions) {
                LogLevel::Info
            } else {
                LogLevel::Critical
            },
            security_mode: SecurityMode::Medium,
            performance_mode: PerformanceMode::Standard,
            displays_mode: DisplaysMode::MatchLayout,
            cursor_preference: CursorMode::Local,
            clipboard_enabled: true,
            microphone_enabled: false,
            hidpi_streaming: false,
            fullscreen_uses_notch_area: false,
            remote_ui_scale_percent: 0,
            full_color_display_mask: 0,
            hidpi_display_mask: 0,
            tablet_input_enabled: true,
            tablet_mode_requested: TabletModeMsg::LocalTermination,
            pen_buttons: arcen_input::pen_buttons::PenButtonSettings::default(),
            exact_still_image: true,
            canvas: true,
            canvas_cache_mib: crate::canvas::DEFAULT_CACHE_MIB,
            color_fidelity: ColorFidelitySettings::default(),
            encode_intent: arcen_media::EncodeIntent::default(),
            typing_mode: arcen_input::text::TypingMode::default(),
            option_key_mode: arcen_input::text::OptionKeyPreference::default(),
            paste: arcen_input::text::PasteSettings::default(),
            keyboard_capture: arcen_input::keyboard::KeyboardCapturePolicy::default(),
        }
    }
}

/// Why the Deck's configuration could not be read or written.
#[derive(Debug)]
pub enum ClientConfigError {
    /// Neither an override nor the platform names a config directory.
    ConfigUnavailable,
    /// File I/O failed.
    Io(std::io::Error),
    /// A file is not valid JSON.
    Parse(serde_json::Error),
    /// A file is JSON of the wrong shape.
    Invalid(&'static str),
    /// The saved connection is gone.
    ConnectionMissing(String),
    /// The saved connection id is taken.
    ConnectionConflict(String),
    /// Another process changed the saved connection.
    ConnectionChanged(String),
    /// Another process changed the saved connections.
    StaleSnapshot,
    /// Another Deck holds the saved-connections lock.
    ConfigBusy,
}

impl std::fmt::Display for ClientConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConfigUnavailable => {
                formatter.write_str("client configuration location is unavailable")
            }
            Self::Io(error) => write!(formatter, "client configuration I/O failed: {error}"),
            Self::Parse(error) => write!(formatter, "client configuration is invalid: {error}"),
            Self::Invalid(reason) => write!(formatter, "client configuration is invalid: {reason}"),
            Self::ConnectionMissing(id) => {
                write!(formatter, "saved connection no longer exists: {id}")
            }
            Self::ConnectionConflict(id) => {
                write!(formatter, "saved connection id already exists: {id}")
            }
            Self::ConnectionChanged(id) => {
                write!(
                    formatter,
                    "saved connection was changed in another process: {id}"
                )
            }
            Self::StaleSnapshot => {
                formatter.write_str("saved connections changed in another process")
            }
            Self::ConfigBusy => formatter
                .write_str("another Deck instance is updating saved connections; try again"),
        }
    }
}

impl std::error::Error for ClientConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Parse(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ClientConfigError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for ClientConfigError {
    fn from(error: serde_json::Error) -> Self {
        Self::Parse(error)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ClientConfigRepository {
    pub root: Option<std::path::PathBuf>,
}

impl ClientConfigRepository {
    pub const CONNECTION_LOCK_TIMEOUT: Duration = Duration::from_millis(100);
    pub const CONNECTION_LOCK_RETRY: Duration = Duration::from_millis(5);

    /// The repository rooted at the resolved config directory
    /// ([`crate::directories::DeckDirectories::config`], which a shell
    /// gets from `DeckServices::directories`). `None` keeps everything in
    /// memory.
    #[must_use]
    pub fn new(root: Option<std::path::PathBuf>) -> Self {
        Self { root }
    }

    /// The repository rooted at `root`.
    #[must_use]
    pub fn at(root: std::path::PathBuf) -> Self {
        Self { root: Some(root) }
    }

    #[must_use]
    pub fn settings_file(&self) -> Option<std::path::PathBuf> {
        Some(self.root.as_ref()?.join("settings.json"))
    }

    #[must_use]
    pub fn connections_file(&self) -> Option<std::path::PathBuf> {
        Some(self.root.as_ref()?.join("connections.json"))
    }

    #[must_use]
    pub fn connections_lock_file(&self) -> Option<std::path::PathBuf> {
        Some(self.root.as_ref()?.join("connections.json.lock"))
    }

    #[must_use]
    pub fn remembered_username_file(&self) -> Option<std::path::PathBuf> {
        Some(self.root.as_ref()?.join("remembered_username"))
    }

    /// Host identities the user chose to remember.
    ///
    /// Separate from `connections.json` because a pin belongs to an endpoint,
    /// not to a bookmark: someone who types a host into quick-connect faces the
    /// same substitution risk as someone who clicks a saved entry.
    #[must_use]
    pub fn trusted_pins_file(&self) -> Option<std::path::PathBuf> {
        Some(self.root.as_ref()?.join("trusted_pins.json"))
    }

    pub fn lock_connections(&self) -> Result<std::fs::File, ClientConfigError> {
        let path = self
            .connections_lock_file()
            .ok_or(ClientConfigError::ConfigUnavailable)?;
        let parent = path.parent().ok_or(ClientConfigError::ConfigUnavailable)?;
        std::fs::create_dir_all(parent)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let deadline = Instant::now() + Self::CONNECTION_LOCK_TIMEOUT;
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(ClientConfigError::ConfigBusy);
                    }
                    std::thread::sleep(Self::CONNECTION_LOCK_RETRY.min(remaining));
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
        Ok(file)
    }

    pub fn mutate_connections(
        &self,
        mutation: impl FnOnce(&mut SavedConnectionsStore) -> Result<(), ClientConfigError>,
    ) -> Result<SavedConnectionsStore, ClientConfigError> {
        let _lock = self.lock_connections()?;
        let mut store = load_saved_connections(self)?
            .unwrap_or_else(|| SavedConnectionsStore::with_connections(Vec::new()));
        mutation(&mut store)?;
        store.version = store.version.max(SAVED_CONNECTIONS_VERSION);
        write_saved_connections(self, &mut store)?;
        Ok(store)
    }

    pub fn set_last_selected(&self, id: &str) -> Result<SavedConnectionsStore, ClientConfigError> {
        self.mutate_connections(|store| {
            if !store.connections.iter().any(|saved| saved.id == id) {
                return Err(ClientConfigError::ConnectionMissing(id.to_string()));
            }
            store.last_selected_connection_id = Some(id.to_string());
            Ok(())
        })
    }

    pub fn set_remembered_username(
        &self,
        id: &str,
        username: Option<String>,
    ) -> Result<SavedConnectionsStore, ClientConfigError> {
        self.mutate_connections(|store| {
            let saved = store
                .connections
                .iter_mut()
                .find(|saved| saved.id == id)
                .ok_or_else(|| ClientConfigError::ConnectionMissing(id.to_string()))?;
            saved.settings.remembered_username = username;
            saved.settings.unknown.remove("remembered_username");
            Ok(())
        })
    }

    pub fn set_tablet_mode_requested(
        &self,
        id: &str,
        mode: TabletModeMsg,
    ) -> Result<SavedConnectionsStore, ClientConfigError> {
        self.mutate_connections(|store| {
            let saved = store
                .connections
                .iter_mut()
                .find(|saved| saved.id == id)
                .ok_or_else(|| ClientConfigError::ConnectionMissing(id.to_string()))?;
            saved.settings.tablet_mode_requested = mode;
            Ok(())
        })
    }

    pub fn set_displays_mode(
        &self,
        id: &str,
        mode: Option<DisplaysMode>,
    ) -> Result<SavedConnectionsStore, ClientConfigError> {
        self.mutate_connections(|store| {
            let saved = store
                .connections
                .iter_mut()
                .find(|saved| saved.id == id)
                .ok_or_else(|| ClientConfigError::ConnectionMissing(id.to_string()))?;
            saved.settings.displays_mode = mode;
            Ok(())
        })
    }

    pub fn add_connection(
        &self,
        connection: SavedConnectionSummary,
    ) -> Result<SavedConnectionsStore, ClientConfigError> {
        self.mutate_connections(|store| {
            if store
                .connections
                .iter()
                .any(|saved| saved.id == connection.id)
            {
                return Err(ClientConfigError::ConnectionConflict(connection.id.clone()));
            }
            store.last_selected_connection_id = Some(connection.id.clone());
            store.connections.push(connection);
            Ok(())
        })
    }

    pub fn update_connection(
        &self,
        id: &str,
        expected: &SavedConnectionSummary,
        connection: SavedConnectionSummary,
    ) -> Result<SavedConnectionsStore, ClientConfigError> {
        if connection.id != id {
            return Err(ClientConfigError::ConnectionConflict(connection.id.clone()));
        }
        self.mutate_connections(|store| {
            let saved = store
                .connections
                .iter_mut()
                .find(|saved| saved.id == id)
                .ok_or_else(|| ClientConfigError::ConnectionMissing(id.to_string()))?;
            if saved != expected {
                return Err(ClientConfigError::ConnectionChanged(id.to_string()));
            }
            *saved = connection;
            Ok(())
        })
    }

    pub fn delete_connection(
        &self,
        id: &str,
        expected: &SavedConnectionSummary,
    ) -> Result<SavedConnectionsStore, ClientConfigError> {
        self.mutate_connections(|store| {
            let index = store
                .connections
                .iter()
                .position(|saved| saved.id == id)
                .ok_or_else(|| ClientConfigError::ConnectionMissing(id.to_string()))?;
            if &store.connections[index] != expected {
                return Err(ClientConfigError::ConnectionChanged(id.to_string()));
            }
            store.connections.remove(index);
            if store.last_selected_connection_id.as_deref() == Some(id) {
                store.last_selected_connection_id = None;
            }
            Ok(())
        })
    }

    pub fn save_connections_snapshot(
        &self,
        snapshot: &SavedConnectionsStore,
    ) -> Result<SavedConnectionsStore, ClientConfigError> {
        let _lock = self.lock_connections()?;
        let current = load_saved_connections(self)?;
        let current_fingerprint = current.as_ref().and_then(|store| store.fingerprint);
        if current_fingerprint != snapshot.fingerprint {
            return Err(ClientConfigError::StaleSnapshot);
        }
        let mut refreshed = snapshot.clone();
        refreshed.version = refreshed.version.max(SAVED_CONNECTIONS_VERSION);
        write_saved_connections(self, &mut refreshed)?;
        Ok(refreshed)
    }
}

/// Remote desktop UI scale, expressed as a Windows display-scaling percentage.
///
/// The client reports a coherent stream scale and synthetic physical size.
/// Windows Pier uses both as the requested display-scale contract, applying the
/// matching Windows scale step on NVIDIA GRID and retaining EDID DPI as
/// verifiable metadata. Reporting the Mac panel's true size would tie a
/// point-sized stream to the panel's backing DPI and make the remote UI far too
/// large.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteUiScale {
    /// Use the stream's own logical-to-physical scale and a synthetic 96-DPI
    /// point-sized panel instead of leaking the Mac panel's backing DPI.
    Automatic,
    Percent(u32),
}

impl RemoteUiScale {
    pub const ALL: [RemoteUiScale; 7] = [
        RemoteUiScale::Automatic,
        RemoteUiScale::Percent(100),
        RemoteUiScale::Percent(125),
        RemoteUiScale::Percent(150),
        RemoteUiScale::Percent(175),
        RemoteUiScale::Percent(200),
        RemoteUiScale::Percent(250),
    ];

    #[must_use]
    pub fn from_percent(percent: u32) -> Self {
        if percent == 0 {
            RemoteUiScale::Automatic
        } else {
            RemoteUiScale::Percent(percent.clamp(100, 400))
        }
    }

    #[must_use]
    pub fn as_percent(self) -> u32 {
        match self {
            RemoteUiScale::Automatic => 0,
            RemoteUiScale::Percent(percent) => percent,
        }
    }

    /// One-line explanation shown next to the selector in the Displays panel.
    #[must_use]
    pub fn label(self) -> String {
        match self {
            RemoteUiScale::Automatic => "Automatic (match this display)".to_string(),
            RemoteUiScale::Percent(percent) => format!("{percent}%"),
        }
    }
}

#[must_use]
pub const fn tablet_mode_allows_typed_pen(mode: TabletModeMsg) -> bool {
    matches!(mode, TabletModeMsg::LocalTermination)
}

/// Whether this session negotiates typed pen input: local setting on, host
/// speaks input protocol v3+, and the host's `InputCapabilitiesMsg.pen` is
/// `Available`. Fails closed (`false`) on any missing piece — an older host,
/// a host with no pen capability, or the local setting off all fall back to
/// the mouse-emulation duplicates the tablet driver already produces, with
/// pressure honestly reported unavailable rather than guessed at.
#[must_use]
pub fn pen_negotiation_result(
    requested_mode: TabletModeMsg,
    input_protocol_version: u32,
    pen_capability: arcen_protocol::messages::InputCapabilityAvailability,
) -> bool {
    matches!(requested_mode, TabletModeMsg::LocalTermination)
        && input_protocol_version >= arcen_protocol::messages::PEN_INPUT_PROTOCOL_VERSION
        && pen_capability == arcen_protocol::messages::InputCapabilityAvailability::Available
}

/// The tablet mode a session starts in from its hello, before the host's
/// `TabletModeResult` confirms it: Local termination only when both ends
/// can carry a typed pen; anything else is mouse compatibility.
#[must_use]
pub fn provisional_tablet_mode_active(
    requested: TabletModeMsg,
    input_protocol_version: u32,
    local_pen_capability: arcen_protocol::messages::InputCapabilityAvailability,
    host_pen_capability: arcen_protocol::messages::InputCapabilityAvailability,
) -> TabletModeMsg {
    match requested {
        TabletModeMsg::LocalTermination
            if input_protocol_version >= arcen_protocol::messages::PEN_INPUT_PROTOCOL_VERSION
                && local_pen_capability
                    == arcen_protocol::messages::InputCapabilityAvailability::Available
                && host_pen_capability
                    == arcen_protocol::messages::InputCapabilityAvailability::Available =>
        {
            TabletModeMsg::LocalTermination
        }
        TabletModeMsg::DisabledMouseCompat => TabletModeMsg::DisabledMouseCompat,
        TabletModeMsg::LocalTermination | TabletModeMsg::UsbTabletBridge => {
            TabletModeMsg::DisabledMouseCompat
        }
    }
}

/// This machine's capability in the hello's terms.
#[must_use]
pub const fn protocol_capability(
    availability: arcen_input::CapabilityAvailability,
) -> arcen_protocol::messages::InputCapabilityAvailability {
    match availability {
        arcen_input::CapabilityAvailability::Available => {
            arcen_protocol::messages::InputCapabilityAvailability::Available
        }
        arcen_input::CapabilityAvailability::Unavailable => {
            arcen_protocol::messages::InputCapabilityAvailability::Unavailable
        }
        arcen_input::CapabilityAvailability::Unknown => {
            arcen_protocol::messages::InputCapabilityAvailability::Unknown
        }
    }
}
