// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Drives a scene through egui passes as a window would — the same raw
//! input a shell hands egui, at the manual's window size and scale, with
//! the appearance the window system reports — and keeps what the last pass
//! painted and where egui says each widget is.

use egui::accesskit;

use super::scenes::{Click, Scene};
use super::{DocShotShell, DocTheme, PIXELS_PER_POINT, WINDOW_POINTS};

/// Passes run after the first and after each click, so menus, pickers,
/// focus and layout have settled (Reduce motion makes animation instant).
const SETTLE_PASSES: usize = 8;
/// The time between passes: a 60 Hz display.
const PASS_SECONDS: f32 = 1.0 / 60.0;

/// One widget egui reported, with the name a hotspot can use.
#[derive(Debug, Clone, PartialEq)]
pub struct Widget {
    /// Its accessibility role.
    pub role: accesskit::Role,
    /// Its accessible name, if any (a button's text, a label's text).
    pub label: Option<String>,
    /// Its value (a field's text, a label's text).
    pub value: Option<String>,
    /// Its placeholder (a field's hint).
    pub placeholder: Option<String>,
    /// Where it is, in points.
    pub rect: egui::Rect,
}

impl Widget {
    /// Whether `name` is its name, its value or its placeholder.
    #[must_use]
    pub fn is_named(&self, name: &str) -> bool {
        self.texts().any(|text| text == name)
    }

    /// Its name, value and placeholder, those it has.
    pub fn texts(&self) -> impl Iterator<Item = &str> {
        [&self.label, &self.value, &self.placeholder]
            .into_iter()
            .filter_map(|text| text.as_deref())
    }

    /// Whether the user can operate it (a button, field, picker, box).
    #[must_use]
    pub fn is_control(&self) -> bool {
        use accesskit::Role;
        matches!(
            self.role,
            Role::Button
                | Role::CheckBox
                | Role::ComboBox
                | Role::TextInput
                | Role::PasswordInput
                | Role::RadioButton
                | Role::Link
                | Role::Slider
                | Role::SpinButton
        )
    }
}

/// How a hotspot or a click names its widget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Name {
    /// Any widget with exactly this name, value or placeholder.
    Is(&'static str),
    /// A control (button, field, picker, box) with exactly this name.
    Control(&'static str),
    /// Any widget whose name, value or placeholder starts with this.
    StartsWith(&'static str),
    /// The control right under the widget with this caption (a form
    /// field and its label above it).
    Under(&'static str),
}

impl Name {
    /// What it names, for messages and the geometry file.
    #[must_use]
    pub const fn text(self) -> &'static str {
        match self {
            Self::Is(text) | Self::Control(text) | Self::StartsWith(text) | Self::Under(text) => {
                text
            }
        }
    }

    /// How it reads in the geometry file: the text, with how it matches.
    #[must_use]
    pub fn describe(self) -> String {
        match self {
            Self::Is(text) => text.to_owned(),
            Self::Control(text) => format!("control {text}"),
            Self::StartsWith(text) => format!("starts {}", text.trim_end()),
            Self::Under(text) => format!("control under {text}"),
        }
    }
}

/// The gap a caption may leave above its control, in points.
const CAPTION_GAP: f32 = 16.0;

fn reading_order(found: &mut [&Widget]) {
    found.sort_by(|a, b| {
        (a.rect.min.y, a.rect.min.x)
            .partial_cmp(&(b.rect.min.y, b.rect.min.x))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

/// The widgets `name` names, top to bottom, then left to right.
#[must_use]
pub fn find(widgets: &[Widget], name: Name) -> Vec<&Widget> {
    // Laid out: a finite rectangle (text without fonts, as in a unit test,
    // is zero wide).
    let shown = |widget: &&Widget| {
        widget.rect.is_finite() && widget.rect.width() >= 0.0 && widget.rect.height() >= 0.0
    };
    let mut found: Vec<&Widget> = match name {
        Name::Is(text) => widgets
            .iter()
            .filter(shown)
            .filter(|w| w.is_named(text))
            .collect(),
        Name::Control(text) => widgets
            .iter()
            .filter(shown)
            .filter(|w| w.is_control() && w.is_named(text))
            .collect(),
        Name::StartsWith(prefix) => widgets
            .iter()
            .filter(shown)
            .filter(|w| w.texts().any(|text| text.starts_with(prefix)))
            .collect(),
        Name::Under(caption) => {
            let mut captions = find(widgets, Name::Is(caption));
            captions.retain(|w| !w.is_control());
            captions
                .into_iter()
                .filter_map(|caption| {
                    let mut below: Vec<&Widget> = widgets
                        .iter()
                        .filter(shown)
                        .filter(|w| {
                            w.is_control()
                                && w.rect.min.y >= caption.rect.max.y - 1.0
                                && w.rect.min.y <= caption.rect.max.y + CAPTION_GAP
                                && w.rect.x_range().contains(caption.rect.min.x + 1.0)
                        })
                        .collect();
                    reading_order(&mut below);
                    below.first().copied()
                })
                .collect()
        }
    };
    reading_order(&mut found);
    found.dedup_by(|a, b| std::ptr::eq(*a, *b));
    found
}

/// What one scene looked like on its last pass.
#[derive(Debug)]
pub struct Capture {
    /// The scene's id.
    pub scene: &'static str,
    /// What to paint, tessellated at [`PIXELS_PER_POINT`].
    pub primitives: Vec<egui::ClippedPrimitive>,
    /// Every texture change since the context started, in order.
    pub textures: Vec<egui::TexturesDelta>,
    /// The widgets of the last pass.
    pub widgets: Vec<Widget>,
    /// The scale egui painted at.
    pub pixels_per_point: f32,
}

struct Runner<'a> {
    ctx: egui::Context,
    shell: &'a dyn DocShotShell,
    theme: DocTheme,
    time: f64,
    textures: Vec<egui::TexturesDelta>,
    widgets: Vec<Widget>,
    last: Option<egui::FullOutput>,
}

impl Runner<'_> {
    fn raw_input(&self, events: Vec<egui::Event>) -> egui::RawInput {
        let mut viewports = egui::ViewportIdMap::default();
        viewports.insert(
            egui::ViewportId::ROOT,
            egui::ViewportInfo {
                native_pixels_per_point: Some(PIXELS_PER_POINT),
                focused: Some(true),
                ..egui::ViewportInfo::default()
            },
        );
        egui::RawInput {
            viewport_id: egui::ViewportId::ROOT,
            viewports,
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(WINDOW_POINTS[0], WINDOW_POINTS[1]),
            )),
            max_texture_side: Some(8192),
            time: Some(self.time),
            predicted_dt: PASS_SECONDS,
            events,
            focused: true,
            system_theme: Some(self.theme.egui()),
            ..egui::RawInput::default()
        }
    }

    fn pass(&mut self, scene: &mut Scene, events: Vec<egui::Event>) {
        let input = self.raw_input(events);
        // Both Decks follow the system's Reduce motion each pass; the manual
        // shows settled screens, so animation is instant.
        crate::screens::theme::set_reduce_motion(&self.ctx, true);
        let shell = self.shell;
        let output = self.ctx.run_ui(input, |ui| scene.draw(ui, shell));
        self.time += f64::from(PASS_SECONDS);
        self.textures.push(output.textures_delta.clone());
        if let Some(update) = &output.platform_output.accesskit_update {
            self.widgets = widgets(update);
        }
        self.last = Some(output);
    }

    fn settle(&mut self, scene: &mut Scene) {
        for _ in 0..SETTLE_PASSES {
            self.pass(scene, Vec::new());
        }
    }

    fn click(&mut self, scene: &mut Scene, click: Click) -> Result<(), String> {
        let found = find(&self.widgets, click.name);
        let target = found.get(click.nth).ok_or_else(|| {
            format!(
                "{}: no widget named \u{201c}{}\u{201d} (#{}) to click",
                scene.id(),
                click.name.text(),
                click.nth
            )
        })?;
        let at = target.rect.center();
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        self.pass(scene, vec![egui::Event::PointerMoved(at)]);
        self.pass(scene, vec![button(true)]);
        self.pass(scene, vec![button(false)]);
        // The pointer leaves: no hover and no tooltip in the picture.
        self.pass(scene, vec![egui::Event::PointerGone]);
        self.settle(scene);
        Ok(())
    }
}

/// Runs `scene` in `shell`'s setup and root frame with `theme`, clicks
/// through its script and keeps the last pass.
///
/// # Errors
///
/// A click whose widget the scene does not show.
pub fn capture(
    shell: &dyn DocShotShell,
    scene: &mut Scene,
    theme: DocTheme,
) -> Result<Capture, String> {
    let ctx = egui::Context::default();
    shell.setup(&ctx);
    // Where each widget is: the accessibility tree egui builds anyway.
    ctx.enable_accesskit();
    scene.keep_names_unresolved();
    let mut runner = Runner {
        ctx,
        shell,
        theme,
        time: 0.0,
        textures: Vec::new(),
        widgets: Vec::new(),
        last: None,
    };
    runner.settle(scene);
    for click in scene.script().to_vec() {
        runner.click(scene, click)?;
    }
    let output = runner
        .last
        .take()
        .ok_or_else(|| format!("{}: no pass ran", scene.id()))?;
    let pixels_per_point = output.pixels_per_point;
    let primitives = runner.ctx.tessellate(output.shapes, pixels_per_point);
    Ok(Capture {
        scene: scene.id(),
        primitives,
        textures: runner.textures,
        widgets: runner.widgets,
        pixels_per_point,
    })
}

fn widgets(update: &accesskit::TreeUpdate) -> Vec<Widget> {
    update
        .nodes
        .iter()
        .filter_map(|(_, node)| {
            // A label's text runs repeat it: the label is the widget.
            if node.role() == accesskit::Role::TextRun {
                return None;
            }
            let bounds = node.bounds()?;
            #[allow(
                clippy::cast_possible_truncation,
                reason = "egui's own coordinates, widened to f64 by accesskit"
            )]
            let rect = egui::Rect::from_min_max(
                egui::pos2(bounds.x0 as f32, bounds.y0 as f32),
                egui::pos2(bounds.x1 as f32, bounds.y1 as f32),
            );
            let text = |value: Option<&str>| value.map(str::to_owned).filter(|v| !v.is_empty());
            Some(Widget {
                role: node.role(),
                label: text(node.label()),
                value: text(node.value()),
                placeholder: text(node.placeholder()),
                rect,
            })
        })
        .collect()
}
