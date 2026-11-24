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

use std::fmt::Write as _;

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

/// The `--console-outputs` value an installer recommends on a host whose
/// NVIDIA GPU drives no monitor (`nvidia_monitor_connected` `Some(false)`;
/// `None` when the host cannot tell), so a Deck that matches its layout has
/// outputs to use: the GPU's real heads as the Pier's start-time probe found
/// them (`discovered`, e.g. `DFP-0,DFP-2,DFP-4,DFP-6` on a board whose
/// outputs are those heads), else a count. `None`: nothing to recommend.
#[must_use]
pub fn headless_console_outputs_advice(
    nvidia_monitor_connected: Option<bool>,
    discovered: &[String],
) -> Option<String> {
    if nvidia_monitor_connected != Some(false) {
        return None;
    }
    let heads: Vec<&String> = discovered
        .iter()
        .filter(|head| is_forced_head_token(head))
        .take(MAX_VIRTUAL_OUTPUTS)
        .collect();
    Some(if heads.len() >= 2 {
        heads
            .iter()
            .map(|head| head.as_str())
            .collect::<Vec<_>>()
            .join(",")
    } else {
        MAX_VIRTUAL_OUTPUTS.to_string()
    })
}

/// Whether `text` is a file [`virtual_outputs_conf`] wrote.
#[must_use]
pub fn is_virtual_outputs_conf(text: &str) -> bool {
    text.lines().next() == Some(VIRTUAL_OUTPUTS_MARKER)
}

/// The heads a file [`virtual_outputs_conf`] wrote forces (its
/// `ConnectedMonitor` list), so an installer can write the same file again.
/// `None` when `text` is not Arcen's file or its list is not valid heads.
#[must_use]
pub fn forced_heads(text: &str) -> Option<Vec<String>> {
    if !is_virtual_outputs_conf(text) {
        return None;
    }
    let list = text.lines().find_map(|line| {
        line.trim()
            .strip_prefix("Option \"ConnectedMonitor\" \"")?
            .strip_suffix('"')
    })?;
    let heads: Vec<String> = list.split(',').map(str::to_owned).collect();
    (heads.len() <= MAX_VIRTUAL_OUTPUTS && heads.iter().all(|head| is_forced_head_token(head)))
        .then_some(heads)
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
    /// A console shown as it is (another product's desktop) shows no
    /// output a Deck monitor could be mapped to; Arcen does not change it.
    ShownLayoutDiffers { requested: usize, shown: usize },
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
            Self::ShownLayoutDiffers { .. } => "console_layout_differs",
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
            Self::ShownLayoutDiffers { requested, shown } => write!(
                formatter,
                "the desktop shows {shown} monitors for the Deck's {requested}, and is not changed"
            ),
        }
    }
}

impl std::error::Error for ConsoleLayoutError {}

/// One output of a console's X server that shows a picture now: its mode's
/// native size, rotation, refresh and place in the X screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShownOutput {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
    pub rotation: Rotation,
    /// Refresh in millihertz (`0` when unknown).
    pub refresh_mhz: u32,
    pub primary: bool,
}

impl ShownOutput {
    /// This output's extent in the X screen (axes swapped when rotated a
    /// quarter turn).
    #[must_use]
    pub const fn screen_extent(&self) -> (u32, u32) {
        match self.rotation {
            Rotation::Degrees90 | Rotation::Degrees270 => (self.height, self.width),
            Rotation::Degrees0 | Rotation::Degrees180 => (self.width, self.height),
        }
    }

    /// A point of this output's picture (`0.0..=1.0` across it) as a point
    /// of the whole X screen of `screen` pixels (`0.0..=1.0` across that):
    /// where a single-display Deck's pointer lands when it is shown only
    /// this output ([`single_display_output`]).
    #[must_use]
    pub fn screen_point(&self, x: f64, y: f64, screen: (u32, u32)) -> (f64, f64) {
        let (width, height) = self.screen_extent();
        let across = |origin: i32, extent: u32, at: f64, total: u32| {
            ((f64::from(origin) + at.clamp(0.0, 1.0) * f64::from(extent)) / f64::from(total.max(1)))
                .clamp(0.0, 1.0)
        };
        (
            across(self.x, width, x, screen.0),
            across(self.y, height, y, screen.1),
        )
    }
}

/// The output a single-display Deck is shown on a desktop shown as it is
/// (another product's, ADR 0025 §8): the one its primary maps to by
/// [`match_shown_layout`]'s rule, the desktop's primary output (the first
/// shown when none is). `None` when the desktop shows fewer than two
/// outputs: its whole screen is then that one picture.
#[must_use]
pub fn single_display_output(shown: &[ShownOutput]) -> Option<&ShownOutput> {
    (shown.len() > 1).then(|| &shown[host_primary_index(shown)])
}

/// The desktop's primary output, else the first shown.
fn host_primary_index(shown: &[ShownOutput]) -> usize {
    shown.iter().position(|output| output.primary).unwrap_or(0)
}

/// How a host output shows the Deck monitor it was matched to. The Deck
/// fits each picture into its window (1:1 when the sizes are equal, scaled
/// otherwise, letterboxed only when the aspect ratios differ); nothing here
/// changes the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "four independent observations, each reported on its own"
)]
pub struct ShownFit {
    /// The output's extent equals the Deck monitor's.
    pub same_size: bool,
    /// The aspect ratios differ (more than 0.5 %): the Deck letterboxes.
    pub aspect_differs: bool,
    /// The output is rotated differently from the Deck monitor.
    pub rotation_differs: bool,
    /// The refresh rates differ by more than half a hertz (both known).
    pub refresh_differs: bool,
}

impl ShownFit {
    fn between(request: &ConsoleHeadRequest, output: &ShownOutput) -> Self {
        let (deck_width, deck_height) = request.screen_extent();
        let (width, height) = output.screen_extent();
        let cross = |a: u32, b: u32| u64::from(a) * u64::from(b);
        let left = cross(width, deck_height);
        let right = cross(deck_width, height);
        let refresh_mhz = u64::from(request.refresh_hz) * 1000;
        Self {
            same_size: (width, height) == (deck_width, deck_height),
            // |w·H − W·h| > 0.5 % of W·h, in integers.
            aspect_differs: left.abs_diff(right) * 200 > right.max(1),
            rotation_differs: output.rotation != request.rotation,
            refresh_differs: output.refresh_mhz != 0
                && refresh_mhz != 0
                && u64::from(output.refresh_mhz).abs_diff(refresh_mhz) > 500,
        }
    }

    /// Whether the picture is shown exactly as the Deck monitor would be.
    #[must_use]
    pub const fn exact(&self) -> bool {
        self.same_size && !self.aspect_differs && !self.rotation_differs && !self.refresh_differs
    }
}

/// One Deck monitor and the host output that shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShownMatch {
    /// Index of the Deck monitor in the requests.
    pub request: usize,
    pub output: ShownOutput,
    pub fit: ShownFit,
}

/// A Deck's monitors mapped onto a desktop shown as it is (another
/// product's desktop, ADR 0025 §8): each Deck monitor that has a host
/// output gets that output's own picture, in its own window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShownLayoutMatch {
    /// In request order; the Deck's primary is always one of them.
    pub matches: Vec<ShownMatch>,
    /// Host outputs no Deck monitor shows (the host has more monitors).
    pub not_shown: Vec<String>,
    /// Deck monitors without a host output (the host has fewer); they get
    /// no window.
    pub without_output: Vec<usize>,
}

impl ShownLayoutMatch {
    /// Whether the desktop is exactly the Deck's layout: every monitor
    /// matched, same sizes, rotations and relative places, nothing left
    /// over.
    #[must_use]
    pub fn exact(&self, requests: &[ConsoleHeadRequest]) -> bool {
        if !self.not_shown.is_empty() || !self.without_output.is_empty() {
            return false;
        }
        let request_origin = origin(requests.iter().map(|request| (request.x, request.y)));
        let shown_origin = origin(
            self.matches
                .iter()
                .map(|matched| (matched.output.x, matched.output.y)),
        );
        self.matches.iter().all(|matched| {
            let request = &requests[matched.request];
            matched.fit.same_size
                && !matched.fit.rotation_differs
                && (
                    matched.output.x - shown_origin.0,
                    matched.output.y - shown_origin.1,
                ) == (request.x - request_origin.0, request.y - request_origin.1)
        })
    }

    /// One line for logs and the Deck: which output shows which Deck
    /// monitor, and how each differs.
    #[must_use]
    pub fn describe(&self, requests: &[ConsoleHeadRequest]) -> String {
        let mut parts: Vec<String> = self
            .matches
            .iter()
            .map(|matched| {
                let request = &requests[matched.request];
                let (width, height) = matched.output.screen_extent();
                let (deck_width, deck_height) = request.screen_extent();
                let mut part = format!(
                    "display {} <- {} {width}x{height}",
                    matched.request + 1,
                    matched.output.name
                );
                if !matched.fit.same_size {
                    let _ = write!(part, " (Deck {deck_width}x{deck_height}, scaled");
                    if matched.fit.aspect_differs {
                        part.push_str(", letterboxed");
                    }
                    part.push(')');
                }
                if matched.fit.rotation_differs {
                    part.push_str(" (rotated differently)");
                }
                if matched.fit.refresh_differs {
                    let _ = write!(
                        part,
                        " ({}.{:03} Hz, Deck {} Hz)",
                        matched.output.refresh_mhz / 1000,
                        matched.output.refresh_mhz % 1000,
                        request.refresh_hz
                    );
                }
                part
            })
            .collect();
        if !self.not_shown.is_empty() {
            parts.push(format!("not shown: {}", self.not_shown.join(", ")));
        }
        if !self.without_output.is_empty() {
            parts.push(format!(
                "no host monitor for display {}",
                self.without_output
                    .iter()
                    .map(|index| (index + 1).to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        parts.join("; ")
    }
}

fn origin(places: impl Iterator<Item = (i32, i32)>) -> (i32, i32) {
    places.fold((i32::MAX, i32::MAX), |(x, y), (px, py)| {
        (x.min(px), y.min(py))
    })
}

/// A rectangle's centre relative to `anchor`'s, in units of `anchor`'s own
/// extent, so a layout at another scale lands on the same numbers.
fn relative_centre(
    (x, y, width, height): (i32, i32, u32, u32),
    (ax, ay, aw, ah): (i32, i32, u32, u32),
) -> (f64, f64) {
    let centre = |origin: i32, extent: u32| f64::from(origin) + f64::from(extent) / 2.0;
    (
        (centre(x, width) - centre(ax, aw)) / f64::from(aw.max(1)),
        (centre(y, height) - centre(ay, ah)) / f64::from(ah.max(1)),
    )
}

/// Maps a Deck's monitors onto a desktop shown as it is — another
/// product's desktop, whose outputs Arcen never changes (ADR 0025 §8) —
/// by arrangement, the way that product laid the desktop out for its own
/// client: the Deck's primary on the desktop's primary output (the first
/// shown output when none is primary), then each further Deck monitor on
/// the output in the same direction from it (left, right, above, below),
/// nearest first, with places measured in each primary's own size so a
/// desktop at another scale maps the same way. Sizes, refresh and rotation
/// may differ: each output is streamed as it is and the Deck fits it
/// ([`ShownFit`]). Host outputs left over are not shown; Deck monitors left
/// over get no window.
///
/// # Errors
///
/// [`ConsoleLayoutError::Empty`] for no requests,
/// [`ConsoleLayoutError::PrimaryCount`] unless exactly one is primary, and
/// [`ConsoleLayoutError::ShownLayoutDiffers`] when the desktop shows
/// nothing.
pub fn match_shown_layout(
    requests: &[ConsoleHeadRequest],
    shown: &[ShownOutput],
) -> Result<ShownLayoutMatch, ConsoleLayoutError> {
    if requests.is_empty() {
        return Err(ConsoleLayoutError::Empty);
    }
    let primaries = requests.iter().filter(|request| request.primary).count();
    if primaries != 1 {
        return Err(ConsoleLayoutError::PrimaryCount(primaries));
    }
    if shown.is_empty() {
        return Err(ConsoleLayoutError::ShownLayoutDiffers {
            requested: requests.len(),
            shown: 0,
        });
    }
    let deck_primary = requests
        .iter()
        .position(|request| request.primary)
        .unwrap_or(0);
    let host_primary = host_primary_index(shown);
    let request_rect = |request: &ConsoleHeadRequest| {
        let (width, height) = request.screen_extent();
        (request.x, request.y, width, height)
    };
    let output_rect = |output: &ShownOutput| {
        let (width, height) = output.screen_extent();
        (output.x, output.y, width, height)
    };
    let deck_anchor = request_rect(&requests[deck_primary]);
    let host_anchor = output_rect(&shown[host_primary]);
    let mut pairs: Vec<(f64, usize, usize)> = Vec::new();
    for (request_index, request) in requests.iter().enumerate() {
        if request_index == deck_primary {
            continue;
        }
        let deck = relative_centre(request_rect(request), deck_anchor);
        for (output_index, output) in shown.iter().enumerate() {
            if output_index == host_primary {
                continue;
            }
            let host = relative_centre(output_rect(output), host_anchor);
            let distance = (deck.0 - host.0).powi(2) + (deck.1 - host.1).powi(2);
            pairs.push((distance, request_index, output_index));
        }
    }
    pairs.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    let mut output_for: Vec<Option<usize>> = vec![None; requests.len()];
    let mut taken = vec![false; shown.len()];
    output_for[deck_primary] = Some(host_primary);
    taken[host_primary] = true;
    for (_, request_index, output_index) in pairs {
        if output_for[request_index].is_none() && !taken[output_index] {
            output_for[request_index] = Some(output_index);
            taken[output_index] = true;
        }
    }
    let mut matches = Vec::new();
    let mut without_output = Vec::new();
    for (request_index, output_index) in output_for.into_iter().enumerate() {
        match output_index {
            Some(output_index) => matches.push(ShownMatch {
                request: request_index,
                output: shown[output_index].clone(),
                fit: ShownFit::between(&requests[request_index], &shown[output_index]),
            }),
            None => without_output.push(request_index),
        }
    }
    let not_shown = shown
        .iter()
        .zip(&taken)
        .filter(|(_, taken)| !**taken)
        .map(|(output, _)| output.name.clone())
        .collect();
    Ok(ShownLayoutMatch {
        matches,
        not_shown,
        without_output,
    })
}

/// Whether `output` can show a Deck monitor: connected, and listing at
/// least one mode. A head forced connected with nothing behind it (another
/// product's `ConnectedMonitor` on a physical board) lists no mode and is
/// left alone.
#[must_use]
pub fn takes_a_monitor(output: &ConsoleOutput) -> bool {
    output.connected && !output.modes.is_empty()
}

/// How many Deck monitors the console's `outputs` can show at once: the
/// joined session's own X server is the authority for a console's
/// multi-monitor offer (ADR 0025 §6), read at each sign-in.
#[must_use]
pub fn console_monitor_capacity(outputs: &[ConsoleOutput]) -> usize {
    outputs
        .iter()
        .filter(|output| takes_a_monitor(output))
        .count()
        .min(MAX_VIRTUAL_OUTPUTS)
}

/// Plans `requests` onto the console's `outputs`: the i-th Deck monitor on
/// the i-th output that can show one ([`takes_a_monitor`], `RandR` order),
/// every other such output off; outputs listing no mode are not touched.
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
    let connected: Vec<&ConsoleOutput> = outputs
        .iter()
        .filter(|output| takes_a_monitor(output))
        .collect();
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

    /// The X screen the layout needs: the bounding box of every head's
    /// extent from the screen's origin.
    #[must_use]
    pub fn screen_size(&self) -> (u32, u32) {
        self.heads.iter().fold((0, 0), |(width, height), head| {
            let (head_width, head_height) = head.screen_extent();
            let right = u32::try_from(head.x.max(0))
                .unwrap_or(0)
                .saturating_add(head_width);
            let bottom = u32::try_from(head.y.max(0))
                .unwrap_or(0)
                .saturating_add(head_height);
            (width.max(right), height.max(bottom))
        })
    }

    /// Whether the layout fits an X screen whose largest size is
    /// `max_width`x`max_height` (`RandR` `GetScreenSizeRange`).
    #[must_use]
    pub fn fits_screen(&self, max_width: u32, max_height: u32) -> bool {
        let (width, height) = self.screen_size();
        width <= max_width && height <= max_height
    }
}

#[cfg(test)]
mod single_display_tests {
    use super::*;

    fn shown(name: &str, x: i32, y: i32, width: u32, height: u32, primary: bool) -> ShownOutput {
        ShownOutput {
            name: name.to_owned(),
            width,
            height,
            x,
            y,
            rotation: Rotation::Degrees0,
            refresh_mhz: 60_000,
            primary,
        }
    }

    #[test]
    fn a_single_display_deck_is_shown_the_primary_output_of_a_wider_desktop() {
        // The field shape: two 2560x1600 outputs side by side, the right one
        // primary; the X screen is 5120x1600.
        let outputs = [
            shown("DP-1", 0, 0, 2560, 1600, false),
            shown("DP-2", 2560, 0, 2560, 1600, true),
        ];
        let output = single_display_output(&outputs).expect("two outputs");
        assert_eq!(output.name, "DP-2");
        assert_eq!(output.screen_point(0.0, 0.0, (5120, 1600)), (0.5, 0.0));
        assert_eq!(output.screen_point(1.0, 1.0, (5120, 1600)), (1.0, 1.0));
        assert_eq!(output.screen_point(0.5, 0.25, (5120, 1600)), (0.75, 0.25));
        // No primary: the first shown, as Match my layout maps its primary.
        let unnamed = [
            shown("DFP-1", 0, 400, 1920, 1200, false),
            shown("DFP-0", 1920, 0, 2560, 1600, false),
        ];
        assert_eq!(
            single_display_output(&unnamed).map(|o| o.name.as_str()),
            Some("DFP-1")
        );
        // One output: the whole screen is that picture, nothing to choose.
        assert_eq!(single_display_output(&outputs[..1]), None);
        assert_eq!(single_display_output(&[]), None);
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
    fn the_screen_a_layout_needs_spans_every_head() {
        // Two 5K monitors and a portrait one: wider than any one encoder,
        // within a console X server's own maximum.
        let mut portrait = request(1920, 1200, 10240, 0, false);
        portrait.rotation = Rotation::Degrees90;
        let plan = plan_console_layout(
            &[
                request(5120, 2880, 0, 0, true),
                request(5120, 2880, 5120, 0, false),
                portrait,
            ],
            &[vgx("DVI-D-0"), vgx("DVI-D-1"), vgx("DVI-D-2")],
        )
        .expect("plan");
        assert_eq!(plan.screen_size(), (10240 + 1200, 2880));
        assert!(plan.fits_screen(30720, 17280));
        assert!(!plan.fits_screen(8192, 8192));
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

    fn shown(name: &str, (width, height): (u32, u32), (x, y): (i32, i32)) -> ShownOutput {
        ShownOutput {
            name: name.to_owned(),
            width,
            height,
            x,
            y,
            rotation: Rotation::Degrees0,
            refresh_mhz: 60_000,
            primary: false,
        }
    }

    #[test]
    fn a_headless_gpu_is_recommended_its_real_heads() {
        let found: Vec<String> = ["DFP-0", "DFP-2", "DFP-4", "DFP-6", "DFP-7"]
            .map(str::to_owned)
            .to_vec();
        assert_eq!(
            headless_console_outputs_advice(Some(false), &found).as_deref(),
            Some("DFP-0,DFP-2,DFP-4,DFP-6")
        );
        assert_eq!(
            headless_console_outputs_advice(Some(false), &[]).as_deref(),
            Some("4")
        );
        assert_eq!(headless_console_outputs_advice(Some(true), &found), None);
        assert_eq!(headless_console_outputs_advice(None, &found), None);
    }

    fn primary(mut output: ShownOutput) -> ShownOutput {
        output.primary = true;
        output
    }

    fn names(matched: &ShownLayoutMatch) -> Vec<(usize, &str)> {
        matched
            .matches
            .iter()
            .map(|matched| (matched.request, matched.output.name.as_str()))
            .collect()
    }

    /// The field desktop: DP-2 1920x1200 at 0,400 left of the primary DP-0
    /// 2560x1600 at 1920,0; DP-4 and DP-6 forced connected and unlit.
    fn field_desktop() -> [ShownOutput; 2] {
        [
            primary(shown("DP-0", (2560, 1600), (1920, 0))),
            shown("DP-2", (1920, 1200), (0, 400)),
        ]
    }

    #[test]
    fn a_desktop_with_the_decks_own_layout_maps_exactly() {
        let deck = [
            request(2560, 1600, 1920, 0, true),
            request(1920, 1200, 0, 400, false),
        ];
        let matched = match_shown_layout(&deck, &field_desktop()).expect("maps");
        assert_eq!(names(&matched), [(0, "DP-0"), (1, "DP-2")]);
        assert!(matched.exact(&deck));
        assert!(matched.matches.iter().all(|matched| matched.fit.exact()));
        assert_eq!(
            matched.describe(&deck),
            "display 1 <- DP-0 2560x1600; display 2 <- DP-2 1920x1200"
        );
        // Shifted as a whole is still the same layout.
        let shifted = [
            request(2560, 1600, 1927, 3, true),
            request(1920, 1200, 7, 403, false),
        ];
        assert!(
            match_shown_layout(&shifted, &field_desktop())
                .expect("maps")
                .exact(&shifted)
        );
    }

    #[test]
    fn a_deck_at_another_scale_maps_by_arrangement_and_is_scaled() {
        // The host was laid out at 2x 3840x2160 side by side; the Deck
        // reports 2x 1920x1080 (logical), primary on the left.
        let desktop = [
            primary(shown("DP-0", (3840, 2160), (0, 0))),
            shown("DP-2", (3840, 2160), (3840, 0)),
        ];
        let deck = [
            request(1920, 1080, 0, 0, true),
            request(1920, 1080, 1920, 0, false),
        ];
        let matched = match_shown_layout(&deck, &desktop).expect("maps");
        assert_eq!(names(&matched), [(0, "DP-0"), (1, "DP-2")]);
        assert!(!matched.exact(&deck));
        assert!(matched.matches.iter().all(|matched| !matched.fit.same_size
            && !matched.fit.aspect_differs
            && !matched.fit.rotation_differs));
        assert_eq!(
            matched.describe(&deck),
            "display 1 <- DP-0 3840x2160 (Deck 1920x1080, scaled); display 2 <- DP-2 3840x2160 (Deck 1920x1080, scaled)"
        );
    }

    #[test]
    fn the_secondary_follows_its_direction_not_the_randr_order() {
        // RandR lists the right-hand output first; the Deck's second
        // monitor is on the left of its primary, a little lower and smaller.
        let desktop = [
            shown("DP-4", (1920, 1080), (4480, 0)),
            primary(shown("DP-0", (2560, 1440), (1920, 0))),
            shown("DP-2", (1920, 1200), (0, 400)),
        ];
        let deck = [
            request(1920, 1080, 0, 300, false),
            request(2560, 1440, 1920, 0, true),
        ];
        let matched = match_shown_layout(&deck, &desktop).expect("maps");
        assert_eq!(names(&matched), [(0, "DP-2"), (1, "DP-0")]);
        assert_eq!(matched.not_shown, ["DP-4"]);
        assert!(matched.without_output.is_empty());
        let fit = matched.matches[0].fit;
        assert!(!fit.same_size && fit.aspect_differs);
        assert!(
            matched
                .describe(&deck)
                .ends_with("display 2 <- DP-0 2560x1440; not shown: DP-4")
        );
    }

    #[test]
    fn four_host_monitors_for_two_deck_displays_show_the_nearest_two() {
        // A 2x2 desktop; the Deck has its primary and one display above it.
        let desktop = [
            primary(shown("DP-0", (1920, 1080), (0, 1080))),
            shown("DP-2", (1920, 1080), (1920, 1080)),
            shown("DP-4", (1920, 1080), (0, 0)),
            shown("DP-6", (1920, 1080), (1920, 0)),
        ];
        let deck = [
            request(2560, 1440, 0, 1440, true),
            request(2560, 1440, 0, 0, false),
        ];
        let matched = match_shown_layout(&deck, &desktop).expect("maps");
        assert_eq!(names(&matched), [(0, "DP-0"), (1, "DP-4")]);
        assert_eq!(matched.not_shown, ["DP-2", "DP-6"]);
    }

    #[test]
    fn fewer_host_monitors_leave_deck_displays_without_a_window() {
        let desktop = [primary(shown("DP-0", (2560, 1440), (0, 0)))];
        let deck = [
            request(1920, 1080, 0, 0, false),
            request(2560, 1440, 1920, 0, true),
        ];
        let matched = match_shown_layout(&deck, &desktop).expect("maps");
        assert_eq!(names(&matched), [(1, "DP-0")]);
        assert_eq!(matched.without_output, [0]);
        assert_eq!(
            matched.describe(&deck),
            "display 2 <- DP-0 2560x1440; no host monitor for display 1"
        );
    }

    #[test]
    fn without_a_primary_output_the_first_shown_is_the_primary() {
        let desktop = [
            shown("DP-0", (1920, 1080), (1920, 0)),
            shown("DP-2", (1920, 1080), (0, 0)),
        ];
        let deck = [
            request(1920, 1080, 1920, 0, true),
            request(1920, 1080, 0, 0, false),
        ];
        let matched = match_shown_layout(&deck, &desktop).expect("maps");
        assert_eq!(names(&matched), [(0, "DP-0"), (1, "DP-2")]);
        assert!(matched.exact(&deck));
    }

    #[test]
    fn refresh_and_rotation_differences_are_reported_never_refused() {
        let mut portrait = shown("DP-2", (1920, 1200), (2560, 0));
        portrait.rotation = Rotation::Degrees90;
        let mut main = primary(shown("DP-0", (2560, 1600), (0, 0)));
        main.refresh_mhz = 59_972;
        let mut deck_main = request(2560, 1600, 0, 0, true);
        deck_main.refresh_hz = 120;
        let deck = [deck_main, request(1920, 1200, 2560, 0, false)];
        let matched = match_shown_layout(&deck, &[main, portrait]).expect("maps");
        assert!(matched.matches[0].fit.refresh_differs);
        assert!(matched.matches[1].fit.rotation_differs);
        assert!(matched.matches[1].fit.aspect_differs);
        assert_eq!(matched.matches[1].output.screen_extent(), (1200, 1920));
        let line = matched.describe(&deck);
        assert!(line.contains("(59.972 Hz, Deck 120 Hz)"), "{line}");
        assert!(line.contains("(rotated differently)"), "{line}");
    }

    #[test]
    fn nothing_shown_or_no_primary_is_refused() {
        let deck = [request(1920, 1080, 0, 0, true)];
        let error = match_shown_layout(&deck, &[]).expect_err("nothing shown");
        assert_eq!(error.token(), "console_layout_differs");
        assert_eq!(
            match_shown_layout(&[], &field_desktop()),
            Err(ConsoleLayoutError::Empty)
        );
        assert_eq!(
            match_shown_layout(&[request(10, 10, 0, 0, false)], &field_desktop()),
            Err(ConsoleLayoutError::PrimaryCount(0))
        );
    }

    #[test]
    fn forced_outputs_without_modes_take_no_monitor_and_are_left_alone() {
        // Another product's server: two desk monitors, two heads forced
        // connected with nothing behind them, in RandR order between them.
        let outputs = [
            output("DP-0", true, &[(2560, 1600, 59_972)]),
            output("DP-1", false, &[]),
            output("DP-4", true, &[]),
            output("DP-2", true, &[(1920, 1200, 59_950)]),
            output("DP-6", true, &[]),
        ];
        assert_eq!(console_monitor_capacity(&outputs), 2);
        let plan = plan_console_layout(
            &[
                request(2560, 1600, 1920, 0, true),
                request(1920, 1200, 0, 400, false),
            ],
            &outputs,
        )
        .expect("two monitors on the two desk outputs");
        let names: Vec<_> = plan.heads.iter().map(|head| head.output.as_str()).collect();
        assert_eq!(names, ["DP-0", "DP-2"]);
        assert!(plan.unused.is_empty());
        assert!(plan.heads.iter().all(|head| !head.scaled));
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
        assert_eq!(forced_heads(&text), Some(heads));
        assert_eq!(forced_heads("Section \"OutputClass\"\n"), None);
        let edited = text.replace("DFP-0,DFP-1,DFP-2,DFP-3", "DFP-0,/etc/x");
        assert_eq!(forced_heads(&edited), None);
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
            Err(ConsoleLayoutError::NotEnoughOutputs {
                requested: 1,
                available: 0
            })
        );
    }
}
