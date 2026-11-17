// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Deck's layout on a joined console's own outputs (ADR 0025 §6).
//!
//! While a Deck that matches its layout is attached, the console takes the
//! Deck's monitors: one output per Deck monitor, in the Deck's order, each
//! at the Deck monitor's size, place, rotation and refresh, the Deck's
//! primary as primary. The console's own outputs are what the X server
//! reports (forced virtual heads on a headless host, the desk's monitors
//! on a workstation); this module only decides, from those facts, which
//! output shows which monitor and how, and renders the decision as one
//! NVIDIA `MetaMode` so it is applied atomically. The platform adapter reads
//! the outputs, applies the `MetaMode` and restores the console's own
//! layout afterwards.
//!
//! A monitor takes an output mode of exactly its native size when the
//! output lists one (the refresh nearest the Deck's). Otherwise the output
//! keeps a mode it lists — its preferred mode, the panel's own timing — and
//! shows the Deck's size through a scaling viewport (`ViewPortIn`), so a
//! desk monitor is never driven with a timing it did not offer. Outputs
//! the layout does not use are switched off.

use arcen_media::Rotation;

/// Where the installer's forced virtual outputs live (a headless console).
pub const VIRTUAL_OUTPUTS_CONF_PATH: &str = "/etc/X11/xorg.conf.d/10-arcen-console-outputs.conf";

/// The first line of Arcen's file; nothing else is ever replaced or removed.
pub const VIRTUAL_OUTPUTS_MARKER: &str =
    "# Arcen console virtual outputs (install-arcen-pier --console-outputs).";

/// Most virtual outputs the installer forces: the Deck's monitor limit.
pub const MAX_VIRTUAL_OUTPUTS: usize = arcen_media::MAX_MULTI_MONITOR_COUNT;

/// Whether `head` is an NVIDIA display device token the file may name
/// (`DFP-N`, `DP-N`, `HDMI-N`, `DVI-D-N`, `CRT-N`).
#[must_use]
pub fn is_forced_head_token(head: &str) -> bool {
    let Some((kind, index)) = head.rsplit_once('-') else {
        return false;
    };
    matches!(kind, "DFP" | "DP" | "HDMI" | "DVI-D" | "DVI-I" | "CRT")
        && !index.is_empty()
        && index.len() <= 2
        && index.bytes().all(|byte| byte.is_ascii_digit())
}

/// The heads to force for `count` outputs: `DFP-0` … (NVIDIA's names on a
/// virtual GPU and on most data-centre GPUs).
#[must_use]
pub fn default_forced_heads(count: usize) -> Vec<String> {
    (0..count.min(MAX_VIRTUAL_OUTPUTS))
        .map(|index| format!("DFP-{index}"))
        .collect()
}

/// The `xorg.conf.d` file that makes the console's X server (the login
/// screen's and every user session's) see `heads` as connected monitors,
/// with only the first one lit at start. An `OutputClass` matched on the
/// NVIDIA kernel driver merges into whatever `xorg.conf` the host has; the
/// Deck's layout lights the others while it is attached. `None` for an
/// empty or invalid list.
#[must_use]
pub fn virtual_outputs_conf(heads: &[String]) -> Option<String> {
    if heads.is_empty()
        || heads.len() > MAX_VIRTUAL_OUTPUTS
        || !heads.iter().all(|head| is_forced_head_token(head))
    {
        return None;
    }
    Some(format!(
        "{VIRTUAL_OUTPUTS_MARKER}\n\
         # Forces the console's NVIDIA outputs below as connected so a Deck that matches its\n\
         # layout gets one output per monitor. Remove with --no-console-outputs or --uninstall.\n\
         Section \"OutputClass\"\n\
         \x20   Identifier \"arcen-console-outputs\"\n\
         \x20   MatchDriver \"nvidia-drm\"\n\
         \x20   Option \"ConnectedMonitor\" \"{}\"\n\
         \x20   Option \"MetaModes\" \"{}: nvidia-auto-select +0+0\"\n\
         EndSection\n",
        heads.join(","),
        heads[0]
    ))
}

/// Whether `text` is a file [`virtual_outputs_conf`] wrote.
#[must_use]
pub fn is_virtual_outputs_conf(text: &str) -> bool {
    text.lines().next() == Some(VIRTUAL_OUTPUTS_MARKER)
}

/// One video mode an output lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputMode {
    pub width: u32,
    pub height: u32,
    /// Refresh in millihertz (`0` when unknown).
    pub refresh_mhz: u32,
}

/// One output of the console's X screen, in `RandR` order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleOutput {
    /// `RandR` output name, e.g. `DVI-D-1` or `DP-2`.
    pub name: String,
    /// Whether something is connected (a monitor, or a head the driver
    /// was told is connected).
    pub connected: bool,
    /// Modes it lists.
    pub modes: Vec<OutputMode>,
    /// Index into `modes` of the preferred mode, if any.
    pub preferred: Option<usize>,
}

/// One Deck monitor to show, as the topology planner placed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsoleHeadRequest {
    /// Native (pre-rotation) width.
    pub width: u32,
    /// Native (pre-rotation) height.
    pub height: u32,
    /// Placement in the X screen (non-negative, the bounding box's origin
    /// at 0,0).
    pub x: i32,
    pub y: i32,
    pub rotation: Rotation,
    pub refresh_hz: u32,
    pub primary: bool,
}

impl ConsoleHeadRequest {
    /// The monitor's extent in the X screen (axes swapped when rotated a
    /// quarter turn).
    #[must_use]
    pub const fn screen_extent(&self) -> (u32, u32) {
        match self.rotation {
            Rotation::Degrees90 | Rotation::Degrees270 => (self.height, self.width),
            Rotation::Degrees0 | Rotation::Degrees180 => (self.width, self.height),
        }
    }
}

/// How one output shows one Deck monitor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleHeadPlan {
    pub output: String,
    /// The output mode driven (one the output lists).
    pub mode: OutputMode,
    /// The Deck monitor's native size, scanned into `mode`.
    pub viewport_in: (u32, u32),
    pub x: i32,
    pub y: i32,
    pub rotation: Rotation,
    pub primary: bool,
    /// Whether the mode differs from the viewport (the output scales).
    pub scaled: bool,
}

impl ConsoleHeadPlan {
    /// This head's extent in the X screen.
    #[must_use]
    pub const fn screen_extent(&self) -> (u32, u32) {
        match self.rotation {
            Rotation::Degrees90 | Rotation::Degrees270 => (self.viewport_in.1, self.viewport_in.0),
            Rotation::Degrees0 | Rotation::Degrees180 => (self.viewport_in.0, self.viewport_in.1),
        }
    }
}

/// The console's layout while the Deck is attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleLayoutPlan {
    /// One head per Deck monitor, in the Deck's (request) order, which is
    /// also the outputs' `RandR` order: capture addresses heads by that
    /// position.
    pub heads: Vec<ConsoleHeadPlan>,
    /// Connected outputs the layout switches off.
    pub unused: Vec<String>,
}

/// Why the console cannot take the Deck's layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsoleLayoutError {
    /// No monitors were requested.
    Empty,
    /// The console has fewer connected outputs than the Deck has monitors.
    NotEnoughOutputs { requested: usize, available: usize },
    /// An output lists no mode at all.
    NoModes { output: String },
    /// Exactly one requested monitor must be primary.
    PrimaryCount(usize),
}

impl ConsoleLayoutError {
    /// Stable token for logs and refusals.
    #[must_use]
    pub const fn token(&self) -> &'static str {
        match self {
            Self::Empty => "console_layout_empty",
            Self::NotEnoughOutputs { .. } => "console_outputs_insufficient",
            Self::NoModes { .. } => "console_output_without_modes",
            Self::PrimaryCount(_) => "console_layout_primary",
        }
    }
}

impl std::fmt::Display for ConsoleLayoutError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => formatter.write_str("no monitors requested"),
            Self::NotEnoughOutputs {
                requested,
                available,
            } => write!(
                formatter,
                "the console has {available} connected outputs for {requested} monitors"
            ),
            Self::NoModes { output } => write!(formatter, "output {output} lists no modes"),
            Self::PrimaryCount(count) => {
                write!(
                    formatter,
                    "{count} monitors are marked primary (one required)"
                )
            }
        }
    }
}

impl std::error::Error for ConsoleLayoutError {}

/// Plans `requests` onto the console's `outputs`: the i-th Deck monitor on
/// the i-th connected output (`RandR` order), every other connected output
/// off.
///
/// # Errors
///
/// [`ConsoleLayoutError`] when the console cannot show every monitor; the
/// caller then serves the console as it is (one picture), saying why.
pub fn plan_console_layout(
    requests: &[ConsoleHeadRequest],
    outputs: &[ConsoleOutput],
) -> Result<ConsoleLayoutPlan, ConsoleLayoutError> {
    if requests.is_empty() {
        return Err(ConsoleLayoutError::Empty);
    }
    let primaries = requests.iter().filter(|request| request.primary).count();
    if primaries != 1 {
        return Err(ConsoleLayoutError::PrimaryCount(primaries));
    }
    let connected: Vec<&ConsoleOutput> = outputs.iter().filter(|output| output.connected).collect();
    if connected.len() < requests.len() {
        return Err(ConsoleLayoutError::NotEnoughOutputs {
            requested: requests.len(),
            available: connected.len(),
        });
    }
    let mut heads = Vec::with_capacity(requests.len());
    for (request, output) in requests.iter().zip(&connected) {
        let mode = choose_mode(output, request.width, request.height, request.refresh_hz)?;
        heads.push(ConsoleHeadPlan {
            output: output.name.clone(),
            mode,
            viewport_in: (request.width, request.height),
            x: request.x,
            y: request.y,
            rotation: request.rotation,
            primary: request.primary,
            scaled: (mode.width, mode.height) != (request.width, request.height),
        });
    }
    let unused = connected
        .iter()
        .skip(requests.len())
        .map(|output| output.name.clone())
        .collect();
    Ok(ConsoleLayoutPlan { heads, unused })
}

/// The mode an output drives for a monitor of native `width`x`height`:
/// exactly that size when listed (refresh nearest `refresh_hz`), otherwise
/// the preferred mode (or the largest listed), scaled.
fn choose_mode(
    output: &ConsoleOutput,
    width: u32,
    height: u32,
    refresh_hz: u32,
) -> Result<OutputMode, ConsoleLayoutError> {
    let wanted_mhz = refresh_hz.saturating_mul(1000);
    let exact = output
        .modes
        .iter()
        .filter(|mode| mode.width == width && mode.height == height)
        .min_by_key(|mode| mode.refresh_mhz.abs_diff(wanted_mhz));
    if let Some(mode) = exact {
        return Ok(*mode);
    }
    output
        .preferred
        .and_then(|index| output.modes.get(index))
        .or_else(|| {
            output
                .modes
                .iter()
                .max_by_key(|mode| u64::from(mode.width) * u64::from(mode.height))
        })
        .copied()
        .ok_or_else(|| ConsoleLayoutError::NoModes {
            output: output.name.clone(),
        })
}

/// NVIDIA's `MetaMode` rotation token for a clockwise display rotation (a
/// clockwise quarter turn is NVIDIA's `right`).
#[must_use]
pub const fn nvidia_rotation_token(rotation: Rotation) -> &'static str {
    match rotation {
        Rotation::Degrees0 => "normal",
        Rotation::Degrees90 => "right",
        Rotation::Degrees180 => "inverted",
        Rotation::Degrees270 => "left",
    }
}

impl ConsoleLayoutPlan {
    /// The whole layout as one NVIDIA `CurrentMetaMode` value: every used
    /// output's mode, X-screen viewport, position and rotation. Outputs
    /// the `MetaMode` does not name are switched off.
    #[must_use]
    pub fn nvidia_metamode(&self) -> String {
        self.heads
            .iter()
            .map(|head| {
                let (screen_w, screen_h) = head.screen_extent();
                format!(
                    "{}: {}x{} @{}x{} {:+}{:+} {{ViewPortIn={}x{}, ViewPortOut={}x{}+0+0, Rotation={}}}",
                    head.output,
                    head.mode.width,
                    head.mode.height,
                    screen_w,
                    screen_h,
                    head.x,
                    head.y,
                    screen_w,
                    screen_h,
                    head.mode.width,
                    head.mode.height,
                    nvidia_rotation_token(head.rotation)
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The output that must be `RandR` primary.
    #[must_use]
    pub fn primary_output(&self) -> Option<&str> {
        self.heads
            .iter()
            .find(|head| head.primary)
            .map(|head| head.output.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(name: &str, connected: bool, modes: &[(u32, u32, u32)]) -> ConsoleOutput {
        ConsoleOutput {
            name: name.to_owned(),
            connected,
            modes: modes
                .iter()
                .map(|&(width, height, refresh_mhz)| OutputMode {
                    width,
                    height,
                    refresh_mhz,
                })
                .collect(),
            preferred: Some(0),
        }
    }

    fn vgx(name: &str) -> ConsoleOutput {
        output(
            name,
            true,
            &[
                (2560, 1600, 59_860),
                (7680, 4320, 59_990),
                (1920, 1200, 59_950),
                (1920, 1200, 74_930),
            ],
        )
    }

    fn request(width: u32, height: u32, x: i32, y: i32, primary: bool) -> ConsoleHeadRequest {
        ConsoleHeadRequest {
            width,
            height,
            x,
            y,
            rotation: Rotation::Degrees0,
            refresh_hz: 60,
            primary,
        }
    }

    #[test]
    fn the_field_layout_takes_two_heads_at_their_own_sizes() {
        // Primary 2560x1600 at 0,0 and a 1920x1200 left of and below it at
        // -1920,400: the planner has translated the box to 0,0.
        let plan = plan_console_layout(
            &[
                request(2560, 1600, 1920, 0, true),
                request(1920, 1200, 0, 400, false),
            ],
            &[
                vgx("DVI-D-0"),
                vgx("DVI-D-1"),
                vgx("DVI-D-2"),
                vgx("DVI-D-3"),
            ],
        )
        .expect("plan");
        assert_eq!(plan.heads.len(), 2);
        assert!(plan.heads.iter().all(|head| !head.scaled));
        assert_eq!(plan.heads[1].mode.refresh_mhz, 59_950, "nearest refresh");
        assert_eq!(plan.unused, ["DVI-D-2", "DVI-D-3"]);
        assert_eq!(plan.primary_output(), Some("DVI-D-0"));
        assert_eq!(
            plan.nvidia_metamode(),
            "DVI-D-0: 2560x1600 @2560x1600 +1920+0 {ViewPortIn=2560x1600, ViewPortOut=2560x1600+0+0, Rotation=normal}, \
             DVI-D-1: 1920x1200 @1920x1200 +0+400 {ViewPortIn=1920x1200, ViewPortOut=1920x1200+0+0, Rotation=normal}"
        );
    }

    #[test]
    fn a_size_the_output_does_not_list_is_scaled_from_its_preferred_mode() {
        let plan = plan_console_layout(
            &[request(3024, 1964, 0, 0, true)],
            &[output(
                "DP-1",
                true,
                &[(2560, 1440, 59_950), (3840, 2160, 60_000)],
            )],
        )
        .expect("plan");
        let head = &plan.heads[0];
        assert!(head.scaled);
        assert_eq!((head.mode.width, head.mode.height), (2560, 1440));
        assert!(plan.nvidia_metamode().starts_with(
            "DP-1: 2560x1440 @3024x1964 +0+0 {ViewPortIn=3024x1964, ViewPortOut=2560x1440+0+0"
        ));
    }

    #[test]
    fn a_rotated_monitor_states_its_screen_extent_and_rotation() {
        let mut portrait = request(1920, 1200, 2560, 0, false);
        portrait.rotation = Rotation::Degrees90;
        let plan = plan_console_layout(
            &[request(2560, 1600, 0, 0, true), portrait],
            &[vgx("DVI-D-0"), vgx("DVI-D-1")],
        )
        .expect("plan");
        assert_eq!(plan.heads[1].screen_extent(), (1200, 1920));
        assert!(plan.nvidia_metamode().ends_with(
            "DVI-D-1: 1920x1200 @1200x1920 +2560+0 {ViewPortIn=1200x1920, ViewPortOut=1920x1200+0+0, Rotation=right}"
        ));
    }

    #[test]
    fn disconnected_outputs_are_not_used_and_too_few_refuse() {
        let outputs = [vgx("DVI-D-0"), output("DVI-D-1", false, &[])];
        let error = plan_console_layout(
            &[
                request(2560, 1600, 0, 0, true),
                request(1920, 1200, 2560, 0, false),
            ],
            &outputs,
        )
        .expect_err("one connected output");
        assert_eq!(
            error,
            ConsoleLayoutError::NotEnoughOutputs {
                requested: 2,
                available: 1
            }
        );
        assert_eq!(error.token(), "console_outputs_insufficient");
    }

    #[test]
    fn the_virtual_outputs_file_forces_heads_and_lights_the_first() {
        let heads = default_forced_heads(4);
        assert_eq!(heads, ["DFP-0", "DFP-1", "DFP-2", "DFP-3"]);
        let text = virtual_outputs_conf(&heads).expect("file");
        assert!(is_virtual_outputs_conf(&text));
        assert!(text.contains("Option \"ConnectedMonitor\" \"DFP-0,DFP-1,DFP-2,DFP-3\""));
        assert!(text.contains("Option \"MetaModes\" \"DFP-0: nvidia-auto-select +0+0\""));
        assert!(text.contains("MatchDriver \"nvidia-drm\""));
        assert!(!is_virtual_outputs_conf("Section \"OutputClass\"\n"));
        assert_eq!(virtual_outputs_conf(&[]), None);
        assert_eq!(
            virtual_outputs_conf(&default_forced_heads(9)).map(|_| ()),
            Some(())
        );
        assert_eq!(virtual_outputs_conf(&["DFP-0\"; rm".to_owned()]), None);
        assert!(is_forced_head_token("DVI-D-1") && is_forced_head_token("DP-10"));
        assert!(
            !is_forced_head_token("DFP-")
                && !is_forced_head_token("eDP-1")
                && !is_forced_head_token("DFP-123")
        );
    }

    #[test]
    fn exactly_one_primary_and_modes_are_required() {
        assert_eq!(
            plan_console_layout(&[request(10, 10, 0, 0, false)], &[vgx("A")]),
            Err(ConsoleLayoutError::PrimaryCount(0))
        );
        assert_eq!(
            plan_console_layout(&[], &[vgx("A")]),
            Err(ConsoleLayoutError::Empty)
        );
        assert_eq!(
            plan_console_layout(&[request(10, 10, 0, 0, true)], &[output("A", true, &[])]),
            Err(ConsoleLayoutError::NoModes {
                output: "A".to_owned()
            })
        );
    }
}
