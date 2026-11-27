// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! What a Pier's displays are, as the operating system reads them back, and
//! the rules that judge a session by it (ADR 0025 §14).
//!
//! The contract has three parts, and each one is decided here rather than in
//! a platform directory:
//!
//! 1. **Leave only what was there.** A Pier reads [`DisplayTruth`] before a
//!    Deck attaches and again after it leaves; [`restore_differences`] is
//!    empty only when every output is back exactly: present, active,
//!    primary, position, mode and refresh, rotation, EDID, colour mode, the
//!    HDR switch and the bit depth. A value the Pier could not read on one
//!    side is a difference that cannot be proved either way, never a match.
//! 2. **Set what the Deck asks for.** A display is reused as it is only when
//!    it already serves the request, refresh included
//!    ([`mode_already_serves`]); a size match alone never skips the change.
//!    A display served at another mode says why ([`DisplayTruthReason`]).
//! 3. **Claim only what was set up.** A session is served as HDR only when
//!    every display it streams reads back HDR active on that exact output
//!    ([`hdr_truth`]); anything else is served as Grading, with the reason.
//!
//! Each Pier's adapter only reads its OS (Windows `DisplayConfig`, Xorg
//! `RandR`, macOS `CoreGraphics`) into these types; `arcen-pier diagnose
//! displays --json` prints the same snapshot ([`DisplayTruth::to_json`]) so
//! labs and the release gate compare one format on every Pier.

use std::fmt::Write as _;

use arcen_media::Rotation;

/// How the operating system composes an output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColourMode {
    /// Ordinary SDR composition.
    Sdr,
    /// Wide colour gamut without HDR (Windows `WCG`).
    Wcg,
    /// HDR composition (Windows `activeColorMode = HDR`, an EDR or PQ
    /// display on macOS).
    Hdr,
    /// The colour mode could not be read.
    Unknown,
}

impl ColourMode {
    /// Stable token for logs and the diagnostic.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Sdr => "sdr",
            Self::Wcg => "wcg",
            Self::Hdr => "hdr",
            Self::Unknown => "unknown",
        }
    }
}

/// An output's mode: its pixel size and refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeTruth {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Refresh in millihertz; `0` when the OS reports none.
    pub refresh_mhz: u32,
}

impl ModeTruth {
    /// A mode at a whole-hertz refresh (`0`: unknown).
    #[must_use]
    pub const fn at_hz(width: u32, height: u32, refresh_hz: u32) -> Self {
        Self {
            width,
            height,
            refresh_mhz: refresh_hz.saturating_mul(1000),
        }
    }

    /// The refresh rounded to whole hertz (`0`: unknown).
    #[must_use]
    pub const fn refresh_hz(self) -> u32 {
        self.refresh_mhz.saturating_add(500) / 1000
    }
}

/// One output as the operating system reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputTruth {
    /// What identifies the output across reads: the Windows adapter and
    /// target, the `RandR` output name, the macOS vendor, model and serial.
    pub id: String,
    /// What the OS calls it now (`\\.\DISPLAY1`, `DP-0`, a display id);
    /// shown, never compared, because such names move.
    pub label: String,
    /// The output shows part of the desktop.
    pub active: bool,
    /// The output is the desktop's primary (main) display.
    pub primary: bool,
    /// Desktop position of the output's top-left corner.
    pub x: i32,
    /// Desktop position of the output's top-left corner.
    pub y: i32,
    /// The output's mode; `None` while inactive or unreadable.
    pub mode: Option<ModeTruth>,
    /// The output's rotation.
    pub rotation: Rotation,
    /// The EDID the output carries now; `None` when it has none or it could
    /// not be read.
    pub edid: Option<Vec<u8>>,
    /// How the OS composes the output.
    pub colour: ColourMode,
    /// The OS's per-display HDR switch (Windows "Use HDR"); `None` where the
    /// OS has none or it could not be read.
    pub hdr_switch: Option<bool>,
    /// Bits per colour channel on the output's link; `None` when unknown.
    pub bits_per_channel: Option<u8>,
}

impl OutputTruth {
    /// An inactive output with nothing read yet.
    #[must_use]
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            active: false,
            primary: false,
            x: 0,
            y: 0,
            mode: None,
            rotation: Rotation::Degrees0,
            edid: None,
            colour: ColourMode::Unknown,
            hdr_switch: None,
            bits_per_channel: None,
        }
    }

    /// A short, stable fingerprint of the EDID (FNV-1a 64 and its length):
    /// enough to see that two reads carry the same bytes, not a security
    /// hash.
    #[must_use]
    pub fn edid_fingerprint(&self) -> Option<String> {
        self.edid.as_deref().map(edid_fingerprint)
    }
}

/// [`OutputTruth::edid_fingerprint`] for raw bytes.
#[must_use]
pub fn edid_fingerprint(edid: &[u8]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in edid {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("fnv1a64:{hash:016x}:{}", edid.len())
}

/// Every output a Pier's OS reports, active or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayTruth {
    /// Which reader produced it (`windows-displayconfig`, `xorg-randr`,
    /// `macos-coregraphics`).
    pub source: &'static str,
    /// The outputs, in the order the OS listed them.
    pub outputs: Vec<OutputTruth>,
}

impl DisplayTruth {
    /// An empty snapshot from `source`.
    #[must_use]
    pub const fn new(source: &'static str) -> Self {
        Self {
            source,
            outputs: Vec::new(),
        }
    }

    /// The output with this identity.
    #[must_use]
    pub fn output(&self, id: &str) -> Option<&OutputTruth> {
        self.outputs.iter().find(|output| output.id == id)
    }

    /// The snapshot as text, one output per line: what `arcen-pier diagnose
    /// displays` prints without `--json`.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut text = format!("source {}\n", self.source);
        for output in &self.outputs {
            let mode = output.mode.map_or_else(
                || "none".to_owned(),
                |mode| format!("{}x{}@{}mHz", mode.width, mode.height, mode.refresh_mhz),
            );
            let _ = writeln!(
                text,
                "{} [{}] active={} primary={} at={},{} mode={} rotation={} colour={} \
                 hdr_switch={} bits={} edid={}",
                output.label,
                output.id,
                output.active,
                output.primary,
                output.x,
                output.y,
                mode,
                rotation_degrees(output.rotation),
                output.colour.token(),
                output
                    .hdr_switch
                    .map_or_else(|| "unread".to_owned(), |on| on.to_string()),
                output
                    .bits_per_channel
                    .map_or_else(|| "unread".to_owned(), |bits| bits.to_string()),
                output
                    .edid_fingerprint()
                    .unwrap_or_else(|| "unread".to_owned()),
            );
        }
        text
    }

    /// The snapshot as one JSON object: the format `arcen-pier diagnose
    /// displays --json` prints on every Pier. EDIDs appear as a fingerprint
    /// with what they declare, never as their bytes.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut json = String::new();
        json.push_str("{\"source\":");
        push_json_string(&mut json, self.source);
        json.push_str(",\"outputs\":[");
        for (index, output) in self.outputs.iter().enumerate() {
            if index > 0 {
                json.push(',');
            }
            push_output_json(&mut json, output);
        }
        json.push_str("]}");
        json
    }
}

fn push_output_json(json: &mut String, output: &OutputTruth) {
    json.push_str("{\"id\":");
    push_json_string(json, &output.id);
    json.push_str(",\"label\":");
    push_json_string(json, &output.label);
    let _ = write!(
        json,
        ",\"active\":{},\"primary\":{},\"x\":{},\"y\":{},\"mode\":",
        output.active, output.primary, output.x, output.y
    );
    match output.mode {
        Some(mode) => {
            let _ = write!(
                json,
                "{{\"width\":{},\"height\":{},\"refresh_mhz\":{}}}",
                mode.width, mode.height, mode.refresh_mhz
            );
        }
        None => json.push_str("null"),
    }
    let _ = write!(
        json,
        ",\"rotation\":{},\"edid\":",
        rotation_degrees(output.rotation)
    );
    match output.edid.as_deref() {
        Some(edid) => {
            json.push_str("{\"fingerprint\":");
            push_json_string(json, &edid_fingerprint(edid));
            json.push_str(",\"manufacturer\":");
            match crate::edid::manufacturer(edid).map(manufacturer_letters) {
                Some(letters) => push_json_string(json, &letters),
                None => json.push_str("null"),
            }
            let _ = write!(
                json,
                ",\"arcen\":{},\"hdr10\":{}}}",
                crate::edid::is_arcen(edid),
                crate::edid::declares_hdr10(edid)
            );
        }
        None => json.push_str("null"),
    }
    json.push_str(",\"colour\":");
    push_json_string(json, output.colour.token());
    json.push_str(",\"hdr_switch\":");
    match output.hdr_switch {
        Some(on) => {
            let _ = write!(json, "{on}");
        }
        None => json.push_str("null"),
    }
    json.push_str(",\"bits_per_channel\":");
    match output.bits_per_channel {
        Some(bits) => {
            let _ = write!(json, "{bits}");
        }
        None => json.push_str("null"),
    }
    json.push('}');
}

fn push_json_string(json: &mut String, value: &str) {
    json.push('"');
    for character in value.chars() {
        match character {
            '"' => json.push_str("\\\""),
            '\\' => json.push_str("\\\\"),
            '\n' => json.push_str("\\n"),
            '\r' => json.push_str("\\r"),
            '\t' => json.push_str("\\t"),
            control if u32::from(control) < 0x20 => {
                let _ = write!(json, "\\u{:04x}", u32::from(control));
            }
            other => json.push(other),
        }
    }
    json.push('"');
}

const fn rotation_degrees(rotation: Rotation) -> u16 {
    match rotation {
        Rotation::Degrees0 => 0,
        Rotation::Degrees90 => 90,
        Rotation::Degrees180 => 180,
        Rotation::Degrees270 => 270,
    }
}

fn manufacturer_letters(packed: u16) -> String {
    [10_u16, 5, 0]
        .iter()
        .map(|shift| {
            let code = (packed >> shift) & 0x1f;
            u8::try_from(code)
                .ok()
                .filter(|code| (1..=26).contains(code))
                .map_or('?', |code| char::from(b'A' + code - 1))
        })
        .collect()
}

/// What `arcen-pier diagnose …` asks for, on every Pier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnoseRequest {
    /// `-h` / `--help`.
    Help,
    /// `displays [--json]`.
    Displays {
        /// One JSON object ([`DisplayTruth::to_json`]) rather than text.
        json: bool,
    },
}

/// The usage line every Pier prints for `diagnose`.
pub const DIAGNOSE_USAGE: &str = "arcen-pier diagnose displays [--json]";

/// Parses the arguments after `diagnose`.
///
/// # Errors
///
/// An unknown argument, or no subject.
pub fn parse_diagnose(args: &[String]) -> Result<DiagnoseRequest, String> {
    let mut json = false;
    let mut displays = false;
    for argument in args {
        match argument.as_str() {
            "-h" | "--help" => return Ok(DiagnoseRequest::Help),
            "--json" => json = true,
            "displays" if !displays => displays = true,
            other => return Err(format!("unknown diagnose argument: {other}")),
        }
    }
    if displays {
        Ok(DiagnoseRequest::Displays { json })
    } else {
        Err("diagnose needs a subject: displays".to_owned())
    }
}

impl DisplayTruth {
    /// What `diagnose displays` prints: [`Self::to_json`] with a newline, or
    /// [`Self::to_text`].
    #[must_use]
    pub fn render(&self, json: bool) -> String {
        if json {
            let mut text = self.to_json();
            text.push('\n');
            text
        } else {
            self.to_text()
        }
    }
}

/// Which property of an output differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayField {
    /// The output is listed on one side only.
    Present,
    /// Shows part of the desktop or not.
    Active,
    /// Primary or not.
    Primary,
    /// Desktop position.
    Position,
    /// Pixel size and refresh.
    Mode,
    /// Rotation.
    Rotation,
    /// The EDID bytes.
    Edid,
    /// SDR, WCG or HDR composition.
    Colour,
    /// The OS's HDR switch.
    HdrSwitch,
    /// Bits per colour channel.
    BitDepth,
}

impl DisplayField {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::Active => "active",
            Self::Primary => "primary",
            Self::Position => "position",
            Self::Mode => "mode",
            Self::Rotation => "rotation",
            Self::Edid => "edid",
            Self::Colour => "colour",
            Self::HdrSwitch => "hdr_switch",
            Self::BitDepth => "bits_per_channel",
        }
    }
}

/// One way the displays after a Deck left differ from before it attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreDifference {
    /// The output's identity.
    pub output: String,
    /// What differs.
    pub field: DisplayField,
    /// The value before the attachment.
    pub before: String,
    /// The value after it.
    pub after: String,
    /// `false` when one side could not be read: not proved different, and
    /// not proved the same either.
    pub provable: bool,
}

impl std::fmt::Display for RestoreDifference {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} {}: {} -> {}{}",
            self.output,
            self.field.token(),
            self.before,
            self.after,
            if self.provable { "" } else { " (unread)" }
        )
    }
}

/// How the displays after a Deck left differ from before it attached;
/// empty means they are back exactly (contract part 1).
///
/// Outputs are matched by [`OutputTruth::id`]. An inactive output on both
/// sides is compared by its EDID only (an inactive head has no mode or
/// place). A value unread on one side only is a difference that is not
/// provable; unread on both sides is not compared.
#[must_use]
pub fn restore_differences(before: &DisplayTruth, after: &DisplayTruth) -> Vec<RestoreDifference> {
    let mut differences = Vec::new();
    for was in &before.outputs {
        let Some(now) = after.output(&was.id) else {
            differences.push(difference(
                &was.id,
                DisplayField::Present,
                "present",
                "absent",
                true,
            ));
            continue;
        };
        compare_output(was, now, &mut differences);
    }
    for now in &after.outputs {
        if before.output(&now.id).is_none() {
            differences.push(difference(
                &now.id,
                DisplayField::Present,
                "absent",
                "present",
                true,
            ));
        }
    }
    differences
}

fn difference(
    output: &str,
    field: DisplayField,
    before: impl Into<String>,
    after: impl Into<String>,
    provable: bool,
) -> RestoreDifference {
    RestoreDifference {
        output: output.to_owned(),
        field,
        before: before.into(),
        after: after.into(),
        provable,
    }
}

fn compare_output(was: &OutputTruth, now: &OutputTruth, out: &mut Vec<RestoreDifference>) {
    let id = was.id.as_str();
    let edid = |edid: Option<&[u8]>| edid.map_or_else(|| "unread".to_owned(), edid_fingerprint);
    compare_known(
        id,
        DisplayField::Edid,
        was.edid.as_deref(),
        now.edid.as_deref(),
        |value| edid(Some(value)),
        out,
    );
    if was.active != now.active {
        out.push(difference(
            id,
            DisplayField::Active,
            was.active.to_string(),
            now.active.to_string(),
            true,
        ));
        return;
    }
    if !was.active {
        return;
    }
    if was.primary != now.primary {
        out.push(difference(
            id,
            DisplayField::Primary,
            was.primary.to_string(),
            now.primary.to_string(),
            true,
        ));
    }
    if (was.x, was.y) != (now.x, now.y) {
        out.push(difference(
            id,
            DisplayField::Position,
            format!("{},{}", was.x, was.y),
            format!("{},{}", now.x, now.y),
            true,
        ));
    }
    compare_mode(id, was.mode, now.mode, out);
    if was.rotation != now.rotation {
        out.push(difference(
            id,
            DisplayField::Rotation,
            rotation_degrees(was.rotation).to_string(),
            rotation_degrees(now.rotation).to_string(),
            true,
        ));
    }
    let colour = |mode: ColourMode| (mode != ColourMode::Unknown).then_some(mode);
    compare_known(
        id,
        DisplayField::Colour,
        colour(was.colour),
        colour(now.colour),
        |mode| mode.token().to_owned(),
        out,
    );
    compare_known(
        id,
        DisplayField::HdrSwitch,
        was.hdr_switch,
        now.hdr_switch,
        |on| on.to_string(),
        out,
    );
    compare_known(
        id,
        DisplayField::BitDepth,
        was.bits_per_channel,
        now.bits_per_channel,
        |bits| bits.to_string(),
        out,
    );
}

fn compare_mode(
    id: &str,
    was: Option<ModeTruth>,
    now: Option<ModeTruth>,
    out: &mut Vec<RestoreDifference>,
) {
    let show = |mode: Option<ModeTruth>| {
        mode.map_or_else(
            || "unread".to_owned(),
            |mode| format!("{}x{}@{}mHz", mode.width, mode.height, mode.refresh_mhz),
        )
    };
    match (was, now) {
        (None, None) => {}
        (Some(was_mode), Some(now_mode)) => {
            let sized = (was_mode.width, was_mode.height) == (now_mode.width, now_mode.height);
            let refresh_known = was_mode.refresh_mhz != 0 && now_mode.refresh_mhz != 0;
            let refreshed = was_mode.refresh_hz() == now_mode.refresh_hz();
            if !sized || (refresh_known && !refreshed) {
                out.push(difference(
                    id,
                    DisplayField::Mode,
                    show(was),
                    show(now),
                    true,
                ));
            } else if !refresh_known && was_mode.refresh_mhz != now_mode.refresh_mhz {
                out.push(difference(
                    id,
                    DisplayField::Mode,
                    show(was),
                    show(now),
                    false,
                ));
            }
        }
        _ => out.push(difference(
            id,
            DisplayField::Mode,
            show(was),
            show(now),
            false,
        )),
    }
}

fn compare_known<T: PartialEq + Copy>(
    id: &str,
    field: DisplayField,
    was: Option<T>,
    now: Option<T>,
    show: impl Fn(T) -> String,
    out: &mut Vec<RestoreDifference>,
) {
    let text = |value: Option<T>| value.map_or_else(|| "unread".to_owned(), &show);
    match (was, now) {
        (None, None) => {}
        (Some(was_value), Some(now_value)) if was_value == now_value => {}
        (Some(_), Some(_)) => out.push(difference(id, field, text(was), text(now), true)),
        _ => out.push(difference(id, field, text(was), text(now), false)),
    }
}

/// Whether a display already serves a request (contract part 2): the same
/// pixel size, and the requested refresh whenever the display offers it at
/// that size. A request without a refresh (`0`) takes any.
///
/// A display that does not offer the refresh at the size cannot be set to
/// it, so it serves the size at the refresh it has; the caller reports that
/// refresh, never the requested one.
#[must_use]
pub const fn mode_already_serves(
    requested: ModeTruth,
    current: ModeTruth,
    refresh_offered: bool,
) -> bool {
    requested.width == current.width
        && requested.height == current.height
        && (requested.refresh_mhz == 0
            || !refresh_offered
            || requested.refresh_hz() == current.refresh_hz())
}

/// Whether a session's streamed displays read back HDR (contract part 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HdrTruth {
    /// Every streamed display reads back HDR composition.
    Active,
    /// This streamed display does not.
    NotActive {
        /// The output's identity.
        output: String,
        /// What it reads back.
        colour: ColourMode,
    },
    /// This streamed display is not among the outputs read.
    Missing {
        /// The output's identity.
        output: String,
    },
    /// The session names no streamed display.
    NoOutputs,
}

impl HdrTruth {
    /// HDR may be claimed.
    #[must_use]
    pub const fn proven(&self) -> bool {
        matches!(self, Self::Active)
    }
}

/// [`HdrTruth`] of the displays `streamed` (identities) in `measured`: HDR
/// only when every one is active and reads back HDR composition.
#[must_use]
pub fn hdr_truth(streamed: &[&str], measured: &DisplayTruth) -> HdrTruth {
    if streamed.is_empty() {
        return HdrTruth::NoOutputs;
    }
    for id in streamed {
        match measured.output(id) {
            None => {
                return HdrTruth::Missing {
                    output: (*id).to_owned(),
                };
            }
            Some(output) if !output.active || output.colour != ColourMode::Hdr => {
                return HdrTruth::NotActive {
                    output: (*id).to_owned(),
                    colour: if output.active {
                        output.colour
                    } else {
                        ColourMode::Unknown
                    },
                };
            }
            Some(_) => {}
        }
    }
    HdrTruth::Active
}

/// The typed reasons a Pier gives when its displays are not what the
/// contract asks for. Each leads the Pier's message (`code: detail`) and is
/// listed in `docs/operations/error-codes.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayTruthReason {
    /// A recovery journal from an earlier session could not be applied. It
    /// is set aside untouched as the record of what that session changed,
    /// the Deck is told, and the new session starts from the displays as
    /// they are, with a journal of its own.
    RestorePending,
    /// After a Deck left, the displays read back differently from before it
    /// attached.
    RestoreIncomplete,
    /// The display offers no mode of the requested size, so it is served at
    /// the mode it has.
    ModeUnavailable,
    /// The display could not be created at the requested size, so the
    /// host's own display is served at the size it has.
    DisplayNotCreated,
}

impl DisplayTruthReason {
    /// Every reason, for references and tests.
    pub const ALL: [Self; 4] = [
        Self::RestorePending,
        Self::RestoreIncomplete,
        Self::ModeUnavailable,
        Self::DisplayNotCreated,
    ];

    /// Stable token.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::RestorePending => "display_restore_pending",
            Self::RestoreIncomplete => "display_restore_incomplete",
            Self::ModeUnavailable => "display_mode_unavailable",
            Self::DisplayNotCreated => "display_not_created",
        }
    }

    /// `code: detail`, the form a refusal or a log line leads with.
    #[must_use]
    pub fn message(self, detail: &str) -> String {
        format!("{}: {detail}", self.code())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(id: &str) -> OutputTruth {
        OutputTruth {
            active: true,
            primary: true,
            mode: Some(ModeTruth::at_hz(2560, 1440, 60)),
            edid: Some(vec![0, 1, 2, 3]),
            colour: ColourMode::Sdr,
            hdr_switch: Some(false),
            bits_per_channel: Some(8),
            ..OutputTruth::new(id, "DISPLAY1")
        }
    }

    fn truth(outputs: Vec<OutputTruth>) -> DisplayTruth {
        DisplayTruth {
            source: "test",
            outputs,
        }
    }

    #[test]
    fn displays_put_back_exactly_have_no_difference() {
        let before = truth(vec![output("a"), OutputTruth::new("b", "DISPLAY2")]);
        let mut after = before.clone();
        after.outputs[0].label = "DISPLAY3".to_owned();
        assert!(restore_differences(&before, &after).is_empty());
    }

    #[test]
    fn a_restore_that_matches_only_size_and_primary_is_not_a_restore() {
        let before = truth(vec![output("a")]);
        let mut after = before.clone();
        let now = &mut after.outputs[0];
        now.mode = Some(ModeTruth::at_hz(2560, 1440, 30));
        now.edid = Some(vec![9, 9]);
        now.colour = ColourMode::Hdr;
        now.hdr_switch = Some(true);
        now.bits_per_channel = Some(10);
        let fields = restore_differences(&before, &after)
            .iter()
            .map(|difference| (difference.field, difference.provable))
            .collect::<Vec<_>>();
        assert_eq!(
            fields,
            vec![
                (DisplayField::Edid, true),
                (DisplayField::Mode, true),
                (DisplayField::Colour, true),
                (DisplayField::HdrSwitch, true),
                (DisplayField::BitDepth, true),
            ]
        );
    }

    #[test]
    fn an_unread_value_is_never_counted_as_restored() {
        let before = truth(vec![output("a")]);
        let mut after = before.clone();
        after.outputs[0].colour = ColourMode::Unknown;
        after.outputs[0].edid = None;
        let differences = restore_differences(&before, &after);
        assert_eq!(differences.len(), 2);
        assert!(differences.iter().all(|difference| !difference.provable));
        assert_eq!(differences[0].after, "unread");
    }

    #[test]
    fn a_display_left_behind_or_lost_is_a_difference() {
        let before = truth(vec![output("a")]);
        let mut after = truth(vec![output("a"), output("virtual")]);
        after.outputs[0].primary = false;
        after.outputs[0].x = -1920;
        let differences = restore_differences(&before, &after);
        let fields = differences
            .iter()
            .map(|difference| (difference.output.as_str(), difference.field))
            .collect::<Vec<_>>();
        assert_eq!(
            fields,
            vec![
                ("a", DisplayField::Primary),
                ("a", DisplayField::Position),
                ("virtual", DisplayField::Present),
            ]
        );
        assert_eq!(
            restore_differences(&after, &before)
                .last()
                .map(|difference| (difference.field, difference.after.as_str())),
            Some((DisplayField::Present, "absent"))
        );
    }

    #[test]
    fn an_inactive_head_is_compared_by_its_edid_and_activity_only() {
        let mut head = OutputTruth::new("b", "");
        head.edid = Some(vec![1]);
        let before = truth(vec![head.clone()]);
        let mut after = before.clone();
        after.outputs[0].x = 100;
        assert!(restore_differences(&before, &after).is_empty());
        after.outputs[0].active = true;
        assert_eq!(
            restore_differences(&before, &after)[0].field,
            DisplayField::Active
        );
    }

    #[test]
    fn a_same_size_display_is_reused_only_at_the_requested_refresh() {
        let asked = ModeTruth::at_hz(1920, 1080, 120);
        let running = ModeTruth::at_hz(1920, 1080, 60);
        assert!(!mode_already_serves(asked, running, true));
        assert!(mode_already_serves(asked, running, false));
        assert!(mode_already_serves(
            asked,
            ModeTruth {
                refresh_mhz: 119_880,
                ..running
            },
            true
        ));
        assert!(!mode_already_serves(
            asked,
            ModeTruth::at_hz(1920, 1200, 120),
            false
        ));
        assert!(mode_already_serves(
            ModeTruth::at_hz(1920, 1080, 0),
            running,
            true
        ));
    }

    #[test]
    fn hdr_is_claimed_only_when_every_streamed_display_reads_back_hdr() {
        let mut hdr = output("a");
        hdr.colour = ColourMode::Hdr;
        let sdr = output("b");
        let measured = truth(vec![hdr, sdr]);
        assert!(hdr_truth(&["a"], &measured).proven());
        assert_eq!(
            hdr_truth(&["a", "b"], &measured),
            HdrTruth::NotActive {
                output: "b".to_owned(),
                colour: ColourMode::Sdr
            }
        );
        assert_eq!(
            hdr_truth(&["c"], &measured),
            HdrTruth::Missing {
                output: "c".to_owned()
            }
        );
        assert_eq!(hdr_truth(&[], &measured), HdrTruth::NoOutputs);
        let mut dark = measured.clone();
        dark.outputs[0].active = false;
        assert!(!hdr_truth(&["a"], &dark).proven());
    }

    #[test]
    fn the_snapshot_prints_as_one_json_object_without_edid_bytes() {
        let edid = crate::edid::generate_hdr10(crate::edid::EdidRequest {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            width_mm: 0.0,
            height_mm: 0.0,
            scale: 1.0,
            product_id: 1,
            serial: 0,
            color: None,
        })
        .expect("edid");
        let mut first = output("adapter \"0\"\\target");
        first.edid = Some(edid.to_vec());
        first.rotation = Rotation::Degrees90;
        let json = truth(vec![first, OutputTruth::new("b", "\n")]).to_json();
        assert!(json.starts_with(
            "{\"source\":\"test\",\"outputs\":[{\"id\":\"adapter \\\"0\\\"\\\\target\""
        ));
        assert!(json.contains("\"manufacturer\":\"ARN\",\"arcen\":true,\"hdr10\":true"));
        assert!(json.contains("\"rotation\":90"));
        assert!(json.contains("\"mode\":{\"width\":2560,\"height\":1440,\"refresh_mhz\":60000}"));
        assert!(json.contains("\"label\":\"\\n\",\"active\":false"));
        assert!(json.contains("\"mode\":null"));
        assert!(json.ends_with("\"hdr_switch\":null,\"bits_per_channel\":null}]}"));
        let text = truth(vec![output("a")]).to_text();
        assert_eq!(
            text,
            "source test\nDISPLAY1 [a] active=true primary=true at=0,0 mode=2560x1440@60000mHz \
             rotation=0 colour=sdr hdr_switch=false bits=8 edid=fnv1a64:\
             4475327f98e05411:4\n"
        );
    }

    #[test]
    fn diagnose_takes_displays_and_an_optional_json_flag() {
        let args = |list: &[&str]| list.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            parse_diagnose(&args(&["displays"])),
            Ok(DiagnoseRequest::Displays { json: false })
        );
        assert_eq!(
            parse_diagnose(&args(&["--json", "displays"])),
            Ok(DiagnoseRequest::Displays { json: true })
        );
        assert_eq!(parse_diagnose(&args(&["-h"])), Ok(DiagnoseRequest::Help));
        for refused in [&[][..], &["displays", "displays"], &["outputs"]] {
            assert!(parse_diagnose(&args(refused)).is_err(), "{refused:?}");
        }
        assert!(truth(Vec::new()).render(true).ends_with("]}\n"));
    }

    #[test]
    fn every_reason_has_a_distinct_code_that_leads_its_message() {
        let mut codes = DisplayTruthReason::ALL
            .map(DisplayTruthReason::code)
            .to_vec();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), DisplayTruthReason::ALL.len());
        assert_eq!(
            DisplayTruthReason::RestorePending.message("journal kept"),
            "display_restore_pending: journal kept"
        );
    }
}
