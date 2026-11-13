// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! **Colour check**: gathers what only this Mac can know — the display's
//! reference mode, colour profile and HDR headroom, and the presentation
//! layer's EDR state and colour space — reads a decoded frame back for the
//! exactness measurement, and draws the checklist. Every rule and every word
//! of the checklist come from `arcen_media::colour_check`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arcen_media::colour_check::{
    pattern_file_name, report_file_name, utc_stamps, ColourCheckReport, ColourFix, DisplayFacts,
    Exactness, ExactnessMeasurement, FrameFacts, ReportHeader, Section, Status,
};
use arcen_media::test_pattern::{
    measure_biplanar, measure_biplanar_as, measure_rgb_roundtrip, BiplanarLayout, PatternSignal,
    TestPattern,
};
use arcen_media::ChromaSubsampling;

use crate::pipeline::video_decoder::DecodedVideoFrame;

/// How long a display reading is reused while the panel is open.
const DISPLAY_REFRESH: Duration = Duration::from_secs(1);

/// The panel's state for the life of the app.
pub struct ColourCheck {
    /// The panel is shown.
    pub open: bool,
    /// A fix reconnected: the panel steps aside for sign-in and comes back
    /// with the new session's hello.
    pub reopen_on_session: bool,
    /// The pattern the host is asked to show.
    pub pattern: TestPattern,
    exactness: Exactness,
    measure_requested: bool,
    /// Bumped whenever a reading in flight stops applying: a new session, a
    /// session's end, another pattern, or a newer measurement. A result is
    /// kept only when it carries the generation current at delivery.
    generation: u64,
    pending: Arc<Mutex<PendingResult>>,
    /// A one-line notice: copied, saved, applied.
    pub notice: Option<String>,
    /// Why the last connection ended, as the transport said it.
    pub last_close: Option<String>,
    display: Option<(Instant, Option<DisplayFacts>)>,
    /// The last decoded frame's own description.
    last_frame: Option<FrameFacts>,
}

impl Default for ColourCheck {
    fn default() -> Self {
        Self {
            open: false,
            reopen_on_session: false,
            pattern: TestPattern::GreyRamp,
            exactness: Exactness::NotRun,
            measure_requested: false,
            generation: 0,
            pending: Arc::new(Mutex::new(PendingResult::default())),
            notice: None,
            last_close: None,
            display: None,
            last_frame: None,
        }
    }
}

/// A finished measurement and the generation it was started under.
#[derive(Default)]
struct PendingResult {
    current: u64,
    result: Option<(u64, Exactness)>,
}

impl PendingResult {
    /// Keeps `result` only when no newer generation has begun.
    fn deliver(&mut self, generation: u64, result: Exactness) -> bool {
        if generation == self.current {
            self.result = Some((generation, result));
            true
        } else {
            false
        }
    }

    /// The finished result for `generation`, if any; anything older is
    /// dropped.
    fn take(&mut self, generation: u64) -> Option<Exactness> {
        match self.result.take() {
            Some((delivered, result)) if delivered == generation => Some(result),
            _ => None,
        }
    }
}

/// What the user asked for in the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColourCheckAction {
    Apply(ColourFix),
    Copy,
    Save,
    Close,
}

impl ColourCheck {
    /// A new session starts with nothing measured.
    pub fn reset_session(&mut self) {
        if std::mem::take(&mut self.reopen_on_session) {
            self.open = true;
        }
        self.exactness = Exactness::NotRun;
        self.measure_requested = false;
        self.last_frame = None;
        self.invalidate();
    }

    /// Starts a new generation: nothing in flight may report into it.
    fn invalidate(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.current = self.generation;
        pending.result = None;
        self.generation
    }

    /// Measures the next decoded frame against [`Self::pattern`].
    pub fn request_measure(&mut self) {
        // Whatever is still in flight is older than this request.
        self.invalidate();
        self.exactness = Exactness::Waiting(self.pattern);
        self.measure_requested = true;
    }

    /// Measures the picture a presentation layer shows now: `buffer`, which
    /// carries `video`'s coding. A still picture sends no new frame, so this
    /// is what Measure reads whenever a layer holds one.
    #[cfg(target_os = "macos")]
    pub fn measure_buffer(
        &mut self,
        buffer: &apple_cf::cv::CVPixelBuffer,
        video: arcen_media::VideoConfiguration,
        ctx: &egui::Context,
    ) {
        self.measure_requested = false;
        self.exactness = Exactness::Waiting(self.pattern);
        self.spawn(FrameCopy::from_pixel_buffer(buffer, video), ctx);
    }

    /// Records a decoded frame, and measures it when a measurement waits
    /// for one.
    pub fn note_frame(&mut self, frame: &DecodedVideoFrame, ctx: &egui::Context) {
        // Only a native frame carries its header's own axes; without them
        // there is nothing to compare with the hello.
        #[cfg(target_os = "macos")]
        {
            self.last_frame = frame.native.as_ref().map(|native| FrameFacts {
                video: native.video,
                width: frame.width,
                height: frame.height,
                pixel_format: frame.pixel_format.clone(),
            });
        }
        if self.measure_requested {
            self.measure_frame(frame, ctx);
        }
    }

    /// Forgets the last frame when the session ends.
    pub fn clear_frame(&mut self) {
        self.last_frame = None;
        self.measure_requested = false;
        if matches!(self.exactness, Exactness::Waiting(_)) {
            self.exactness = Exactness::NotRun;
        }
        self.invalidate();
    }

    /// The last decoded frame's own description.
    #[must_use]
    pub fn frame_facts(&self) -> Option<FrameFacts> {
        self.last_frame.clone()
    }

    /// Changes the pattern; a reading of another pattern no longer applies.
    pub fn select_pattern(&mut self, pattern: TestPattern) {
        if pattern != self.pattern {
            self.pattern = pattern;
            self.exactness = Exactness::NotRun;
            self.measure_requested = false;
            self.invalidate();
        }
    }

    /// Whether the next decoded frame should be read back.
    #[must_use]
    pub const fn wants_frame(&self) -> bool {
        self.measure_requested
    }

    /// The current exactness state, after picking up a finished measurement.
    pub fn exactness(&mut self) -> Exactness {
        let done = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take(self.generation);
        if let Some(done) = done {
            self.exactness = done;
        }
        self.exactness.clone()
    }

    /// Copies what the measurement needs out of `frame` (the planes, or the
    /// RGBA fallback) and measures it off the UI thread. Repaints `ctx` when
    /// done.
    pub fn measure_frame(&mut self, frame: &DecodedVideoFrame, ctx: &egui::Context) {
        self.measure_requested = false;
        self.exactness = Exactness::Waiting(self.pattern);
        self.spawn(FrameCopy::take(frame), ctx);
    }

    fn spawn(&mut self, job: Result<FrameCopy, String>, ctx: &egui::Context) {
        // Only this, the latest, measurement may report — including when
        // the frame could not be copied and nothing is spawned.
        let generation = self.invalidate();
        let pattern = self.pattern;
        let job = match job {
            Ok(job) => job,
            Err(why) => {
                self.exactness = Exactness::Unreadable(pattern, why);
                return;
            }
        };
        let slot = Arc::clone(&self.pending);
        let repaint = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("arcen-colour-check".to_owned())
            .spawn(move || {
                let result = job.measure(pattern);
                if let Exactness::Measured(measurement) = &result {
                    tracing::info!(
                        target: crate::logging::target::VIDEO,
                        pattern = pattern.token(),
                        width = measurement.width,
                        height = measurement.height,
                        rgb_max = measurement.rgb.max_error,
                        rgb_mean = measurement.rgb.mean_error,
                        code_max = measurement.codes.map(|codes| codes.max_error),
                        code_scale = measurement.codes.map(|codes| codes.max_code),
                        "colour check exactness measured",
                    );
                }
                let kept = slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .deliver(generation, result);
                if kept {
                    repaint.request_repaint();
                }
            });
        if spawned.is_err() {
            self.exactness =
                Exactness::Unreadable(pattern, "could not start the measurement".into());
        }
    }

    /// This Mac's display, read at most once a second.
    pub fn display_facts(&mut self) -> Option<DisplayFacts> {
        if let Some((read_at, facts)) = &self.display {
            if read_at.elapsed() < DISPLAY_REFRESH {
                return facts.clone();
            }
        }
        let facts = read_display_facts();
        self.display = Some((Instant::now(), facts.clone()));
        facts
    }
}

/// What a measurement needs from one decoded frame, owned so it can leave
/// the UI thread.
enum FrameCopy {
    Planes {
        layout: BiplanarLayout,
        transform: arcen_media::video::ColorTransform,
        luma: Vec<u8>,
        chroma: Vec<u8>,
    },
    Rgba {
        width: usize,
        height: usize,
        bgra: Vec<u8>,
    },
}

impl FrameCopy {
    fn take(frame: &DecodedVideoFrame) -> Result<Self, String> {
        #[cfg(target_os = "macos")]
        if let Some(native) = &frame.native {
            return Self::from_pixel_buffer(&native.pixel_buffer, native.video);
        }
        if frame.rgba.len() >= frame.width * frame.height * 4 && frame.width > 0 {
            let mut bgra = frame.rgba.clone();
            for pixel in bgra.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
            return Ok(Self::Rgba {
                width: frame.width,
                height: frame.height,
                bgra,
            });
        }
        Err(format!(
            "the decoded frame ({}) carries no readable pixels",
            frame.pixel_format
        ))
    }

    #[cfg(target_os = "macos")]
    fn from_pixel_buffer(
        buffer: &apple_cf::cv::CVPixelBuffer,
        video: arcen_media::VideoConfiguration,
    ) -> Result<Self, String> {
        let format = buffer.pixel_format().to_be_bytes();
        let fourcc = String::from_utf8_lossy(&format).into_owned();
        let width = buffer.width();
        let height = buffer.height();
        let guard = buffer
            .lock(apple_cf::cv::CVPixelBufferLockFlags::READ_ONLY)
            .map_err(|status| format!("{fourcc}: CVPixelBufferLockBaseAddress failed: {status}"))?;
        if guard.plane_count() != 2 {
            return Err(format!(
                "{fourcc} has {} planes; the measurement reads two-plane Y′CbCr",
                guard.plane_count()
            ));
        }
        let luma = guard
            .plane_data(0)
            .ok_or_else(|| format!("{fourcc}: no luma plane"))?
            .to_vec();
        let chroma = guard
            .plane_data(1)
            .ok_or_else(|| format!("{fourcc}: no chroma plane"))?
            .to_vec();
        let chroma_width = guard.width_of_plane(1);
        let chroma_height = guard.height_of_plane(1);
        let sampling = match (
            chroma_width * 2 <= width + 1,
            chroma_height * 2 <= height + 1,
        ) {
            (false, false) => ChromaSubsampling::Yuv444,
            (true, false) => ChromaSubsampling::Yuv422,
            (true, true) => ChromaSubsampling::Yuv420,
            (false, true) => return Err(format!("{fourcc}: unexpected chroma plane shape")),
        };
        let layout = BiplanarLayout {
            width,
            height,
            chroma: sampling,
            // `x420`, `x422`, `x444`, `xf20`, `xf22`, `xf44`: 16-bit words.
            wide: format[0] == b'x',
            luma_stride: guard.bytes_per_row_of_plane(0),
            chroma_stride: guard.bytes_per_row_of_plane(1),
        };
        let transform =
            arcen_media::video::ColorTransform::new(video.matrix, video.range, video.bit_depth);
        Ok(Self::Planes {
            layout,
            transform,
            luma,
            chroma,
        })
    }

    fn measure(self, pattern: TestPattern) -> Exactness {
        match self {
            Self::Planes {
                layout,
                transform,
                luma,
                chroma,
            } => match measure_biplanar(pattern, transform, layout, &luma, &chroma) {
                Some(accuracy) => Exactness::Measured(Box::new(ExactnessMeasurement {
                    pattern,
                    width: layout.width,
                    height: layout.height,
                    rgb: accuracy.rgb,
                    codes: accuracy.codes,
                    // A host may serve the desktop re-encoded from sRGB to
                    // BT.709; measuring that too says which one arrived.
                    reencoded: measure_biplanar_as(
                        pattern,
                        PatternSignal::SrgbReencodedBt709,
                        transform,
                        layout,
                        &luma,
                        &chroma,
                    )
                    .map(|accuracy| accuracy.rgb),
                })),
                None => {
                    Exactness::Unreadable(pattern, "the planes are shorter than the frame".into())
                }
            },
            Self::Rgba {
                width,
                height,
                bgra,
            } => match measure_rgb_roundtrip(pattern, width, height, &bgra) {
                Some(rgb) => Exactness::Measured(Box::new(ExactnessMeasurement {
                    pattern,
                    width,
                    height,
                    rgb,
                    codes: None,
                    reencoded: None,
                })),
                None => Exactness::Unreadable(pattern, "the frame is shorter than its size".into()),
            },
        }
    }
}

/// The NSScreen showing the root window, as AppKit states it.
#[cfg(target_os = "macos")]
fn read_display_facts() -> Option<DisplayFacts> {
    use objc2::runtime::AnyObject;
    use objc2_foundation::{MainThreadMarker, NSString};

    let mtm = MainThreadMarker::new()?;
    let window = crate::ui::video_render::find_root_window(mtm)?;
    let screen = window
        .screen()
        .or_else(|| objc2_app_kit::NSScreen::mainScreen(mtm))?;
    // SAFETY: documented read-only `NSScreen` properties of a live screen,
    // read on the main thread; `colorSpace` returns an `NSColorSpace` (or
    // nil) whose `localizedName` is an `NSString` (or nil).
    let (current, potential, reference, profile) = unsafe {
        let current: f64 =
            objc2::msg_send![&*screen, maximumExtendedDynamicRangeColorComponentValue];
        let potential: f64 = objc2::msg_send![
            &*screen,
            maximumPotentialExtendedDynamicRangeColorComponentValue
        ];
        let reference: f64 = objc2::msg_send![
            &*screen,
            maximumReferenceExtendedDynamicRangeColorComponentValue
        ];
        let space: *mut AnyObject = objc2::msg_send![&*screen, colorSpace];
        let profile = if space.is_null() {
            None
        } else {
            let name: *mut NSString = objc2::msg_send![space, localizedName];
            name.as_ref().map(ToString::to_string)
        };
        (current, potential, reference, profile)
    };
    let name = screen.localizedName().to_string();
    #[allow(clippy::cast_possible_truncation)]
    Some(DisplayFacts {
        name: (!name.trim().is_empty()).then_some(name),
        profile,
        wide_gamut: Some(screen.canRepresentDisplayGamut(objc2_app_kit::NSDisplayGamut::P3)),
        edr_current: Some(current as f32),
        edr_potential: Some(potential as f32),
        edr_reference: Some(reference as f32),
    })
}

#[cfg(not(target_os = "macos"))]
fn read_display_facts() -> Option<DisplayFacts> {
    None
}

/// `~/Documents/Arcen/Colour Checks`, created on demand.
fn output_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    let dir = PathBuf::from(home)
        .join("Documents")
        .join("Arcen")
        .join("Colour Checks");
    std::fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    Ok(dir)
}

/// Now, as the report's two UTC stamps.
#[must_use]
pub fn now_stamps() -> (String, String) {
    utc_stamps(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs()),
    )
}

/// The report's heading for now.
#[must_use]
pub fn report_header(generated_utc: String) -> ReportHeader {
    ReportHeader {
        deck: crate::transport::diagnose::deck_label(),
        generated_utc,
    }
}

/// Writes `text` as a timestamped report and returns its path.
pub fn save_report(text: &str, compact_utc: &str) -> Result<PathBuf, String> {
    let path = output_dir()?.join(report_file_name(compact_utc));
    std::fs::write(&path, text).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(path)
}

/// Writes `pattern` at the stream's size as a PNG and returns its path.
pub fn save_pattern(pattern: TestPattern, width: usize, height: usize) -> Result<PathBuf, String> {
    let png = pattern
        .render_png(width, height)
        .ok_or_else(|| format!("cannot render a {width}×{height} pattern"))?;
    let path = output_dir()?.join(pattern_file_name(pattern, width, height));
    std::fs::write(&path, png).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(path)
}

fn status_colour(status: Status) -> egui::Color32 {
    match status {
        Status::Pass => egui::Color32::from_rgb(0x1E, 0x8E, 0x3E),
        Status::Info => egui::Color32::from_rgb(0x1A, 0x73, 0xE8),
        Status::Warn => egui::Color32::from_rgb(0xC2, 0x7C, 0x0E),
        Status::Fail => egui::Color32::from_rgb(0xC0, 0x20, 0x20),
    }
}

/// Draws the panel as a window over whatever screen is showing.
pub fn render(
    ctx: &egui::Context,
    check: &mut ColourCheck,
    report: &ColourCheckReport,
) -> Option<ColourCheckAction> {
    let mut action = None;
    let mut open = check.open;
    let screen = ctx.content_rect();
    egui::Window::new("Colour check")
        .id(egui::Id::new("arcen-colour-check"))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(560.0)
        .default_height((screen.height() - 80.0).clamp(320.0, 900.0))
        .default_pos(screen.right_top() + egui::vec2(-600.0, 40.0))
        .frame(
            egui::Frame::window(&ctx.global_style())
                .fill(egui::Color32::from_rgb(0xF6, 0xF7, 0xFB))
                .inner_margin(egui::Margin::symmetric(14, 12)),
        )
        .show(ctx, |ui| {
            ui.visuals_mut().override_text_color = Some(egui::Color32::from_rgb(0x20, 0x20, 0x24));
            let overall = report.overall();
            ui.label(
                egui::RichText::new(format!("{} {}", overall.symbol(), report.summary()))
                    .strong()
                    .size(14.0)
                    .color(status_colour(overall)),
            );
            if let Some(host) = &report.host_label {
                ui.label(egui::RichText::new(host).size(11.0));
            }
            ui.label(
                egui::RichText::new(
                    "Read-only on the host: the Deck never changes a host setting.",
                )
                .size(11.0)
                .color(egui::Color32::from_rgb(0x60, 0x60, 0x68)),
            );
            ui.horizontal_wrapped(|ui| {
                let recommended = report.recommended_fixes();
                if let Some(first) = recommended.first() {
                    if ui
                        .button(egui::RichText::new(format!("Apply: {}", first.label())).strong())
                        .clicked()
                    {
                        action = Some(ColourCheckAction::Apply(*first));
                    }
                }
                if ui.button("Copy report").clicked() {
                    action = Some(ColourCheckAction::Copy);
                }
                if ui.button("Save report").clicked() {
                    action = Some(ColourCheckAction::Save);
                }
            });
            if let Some(notice) = &check.notice {
                ui.label(egui::RichText::new(notice).size(11.5).italics());
            }
            ui.separator();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for section in Section::ALL {
                        let mut items = report.section(section).peekable();
                        if items.peek().is_none() {
                            continue;
                        }
                        ui.add_space(6.0);
                        ui.label(egui::RichText::new(section.title()).strong().size(13.5));
                        if section == Section::Exactness {
                            ui.horizontal(|ui| {
                                ui.label("Pattern:");
                                let mut selected = check.pattern;
                                egui::ComboBox::from_id_salt("colour-check-pattern")
                                    .selected_text(selected.token())
                                    .show_ui(ui, |ui| {
                                        for pattern in TestPattern::ALL {
                                            ui.selectable_value(
                                                &mut selected,
                                                *pattern,
                                                format!(
                                                    "{} — {}",
                                                    pattern.token(),
                                                    pattern.exposes()
                                                ),
                                            );
                                        }
                                    });
                                check.select_pattern(selected);
                            });
                        }
                        for item in items {
                            let status = item.status();
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    egui::RichText::new(status.symbol())
                                        .strong()
                                        .size(14.0)
                                        .color(status_colour(status)),
                                );
                                ui.label(egui::RichText::new(item.title).strong().size(12.5));
                                ui.label(
                                    egui::RichText::new(item.finding.code())
                                        .monospace()
                                        .size(9.5)
                                        .color(egui::Color32::from_rgb(0x9A, 0x9A, 0xA0)),
                                );
                            });
                            ui.indent(item.finding.code(), |ui| {
                                ui.label(egui::RichText::new(item.finding.meaning()).size(12.0));
                                for detail in &item.details {
                                    ui.label(
                                        egui::RichText::new(detail)
                                            .monospace()
                                            .size(10.5)
                                            .color(egui::Color32::from_rgb(0x50, 0x50, 0x58)),
                                    );
                                }
                                let advice = item.finding.action();
                                if !advice.is_empty() && status != Status::Pass {
                                    ui.label(
                                        egui::RichText::new(format!("What to do: {advice}"))
                                            .size(11.5)
                                            .color(status_colour(status)),
                                    );
                                }
                                if !item.fixes.is_empty() {
                                    ui.horizontal_wrapped(|ui| {
                                        for fix in &item.fixes {
                                            let clicked = match fix {
                                                ColourFix::ReadDoc(link) => ui
                                                    .link(format!("📖 {}", link.title))
                                                    .on_hover_text(link.url())
                                                    .clicked(),
                                                other => ui.small_button(other.label()).clicked(),
                                            };
                                            if clicked {
                                                action = Some(ColourCheckAction::Apply(*fix));
                                            }
                                        }
                                    });
                                }
                            });
                            ui.add_space(4.0);
                        }
                    }
                });
        });
    if !open {
        action = Some(ColourCheckAction::Close);
    }
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba_frame(pattern: TestPattern, width: usize, height: usize) -> DecodedVideoFrame {
        let bgra = pattern.render_bgra(width, height);
        let mut rgba = bgra;
        for pixel in rgba.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        DecodedVideoFrame {
            width,
            height,
            rgba,
            timestamp_ms: 0,
            pixel_format: "RGBA-direct".to_owned(),
            backend: "test",
            #[cfg(target_os = "macos")]
            native: None,
        }
    }

    #[test]
    fn an_rgba_frame_showing_the_pattern_measures_exact_off_the_ui_thread() {
        let ctx = egui::Context::default();
        let mut check = ColourCheck::default();
        check.select_pattern(TestPattern::SaturatedPrimaries);
        check.request_measure();
        assert!(check.wants_frame());
        check.note_frame(&rgba_frame(TestPattern::SaturatedPrimaries, 48, 16), &ctx);
        assert!(!check.wants_frame());
        let deadline = Instant::now() + Duration::from_secs(10);
        let measured = loop {
            if let Exactness::Measured(measurement) = check.exactness() {
                break measurement;
            }
            assert!(Instant::now() < deadline, "measurement never finished");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(measured.rgb.is_exact());
        assert_eq!(measured.rgb.pixels, 48 * 16);
        assert!(measured.codes.is_none());
    }

    #[test]
    fn a_frame_without_pixels_is_reported_unreadable_rather_than_measured() {
        let ctx = egui::Context::default();
        let mut check = ColourCheck::default();
        check.request_measure();
        let mut frame = rgba_frame(TestPattern::GreyRamp, 8, 8);
        frame.rgba.clear();
        check.note_frame(&frame, &ctx);
        assert!(matches!(check.exactness(), Exactness::Unreadable(..)));
    }

    fn measured(pattern: TestPattern) -> Exactness {
        Exactness::Unreadable(pattern, format!("stand-in result for {}", pattern.token()))
    }

    #[test]
    fn a_result_from_an_older_generation_is_never_delivered_or_taken() {
        let mut pending = PendingResult::default();
        pending.current = 2;
        assert!(!pending.deliver(1, measured(TestPattern::GreyRamp)));
        assert_eq!(pending.take(2), None);
        assert!(pending.deliver(2, measured(TestPattern::GreyRamp)));
        // A newer generation began between delivery and the UI's read.
        assert_eq!(pending.take(3), None);
    }

    #[test]
    fn a_measurement_finishing_after_a_reconnect_or_pattern_change_is_dropped() {
        let ctx = egui::Context::default();
        for change in ["reconnect", "pattern", "session end"] {
            let mut check = ColourCheck::default();
            check.request_measure();
            check.note_frame(&rgba_frame(TestPattern::GreyRamp, 1920, 1080), &ctx);
            match change {
                "reconnect" => check.reset_session(),
                "pattern" => check.select_pattern(TestPattern::ChromaDetail),
                _ => check.clear_frame(),
            }
            // Give the worker ample time to finish and try to deliver.
            std::thread::sleep(Duration::from_millis(1500));
            assert_eq!(check.exactness(), Exactness::NotRun, "{change}");
        }
    }

    #[test]
    fn only_the_latest_of_two_measurements_reports() {
        let ctx = egui::Context::default();
        let mut check = ColourCheck::default();
        check.select_pattern(TestPattern::SaturatedPrimaries);
        check.request_measure();
        check.note_frame(&rgba_frame(TestPattern::GreyRamp, 1920, 1080), &ctx);
        check.request_measure();
        check.note_frame(&rgba_frame(TestPattern::SaturatedPrimaries, 48, 16), &ctx);
        let deadline = Instant::now() + Duration::from_secs(10);
        let measured = loop {
            if let Exactness::Measured(measurement) = check.exactness() {
                break measurement;
            }
            assert!(Instant::now() < deadline, "measurement never finished");
            std::thread::sleep(Duration::from_millis(5));
        };
        // The second frame showed the pattern; the first did not.
        assert!(measured.rgb.is_exact());
        std::thread::sleep(Duration::from_millis(1500));
        assert!(matches!(check.exactness(), Exactness::Measured(m) if m.rgb.is_exact()));
    }

    /// Starts a measurement that takes long enough to still be running when
    /// the test acts.
    fn start_slow_measurement(check: &mut ColourCheck, ctx: &egui::Context) {
        check.request_measure();
        check.note_frame(&rgba_frame(TestPattern::GreyRamp, 3840, 2160), ctx);
    }

    #[test]
    fn a_failed_copy_after_a_pending_older_measurement_stays_unreadable() {
        let ctx = egui::Context::default();
        let mut check = ColourCheck::default();
        start_slow_measurement(&mut check, &ctx);
        check.request_measure();
        let mut empty = rgba_frame(TestPattern::GreyRamp, 8, 8);
        empty.rgba.clear();
        check.note_frame(&empty, &ctx);
        assert!(matches!(check.exactness(), Exactness::Unreadable(..)));
        std::thread::sleep(Duration::from_millis(2500));
        assert!(
            matches!(check.exactness(), Exactness::Unreadable(..)),
            "the older measurement replaced the newer failure"
        );
    }

    #[test]
    fn measure_while_waiting_for_the_next_frame_ignores_the_older_measurement() {
        let ctx = egui::Context::default();
        let mut check = ColourCheck::default();
        start_slow_measurement(&mut check, &ctx);
        // Measure again; no new frame arrives.
        check.request_measure();
        std::thread::sleep(Duration::from_millis(2500));
        assert_eq!(check.exactness(), Exactness::Waiting(TestPattern::GreyRamp));
        assert!(check.wants_frame());
    }

    #[test]
    fn choosing_another_pattern_discards_a_reading_of_the_previous_one() {
        let mut check = ColourCheck::default();
        check.request_measure();
        check.select_pattern(TestPattern::ChromaDetail);
        assert_eq!(check.exactness(), Exactness::NotRun);
        assert!(!check.wants_frame());
    }
}
