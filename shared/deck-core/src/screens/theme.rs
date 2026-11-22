// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

use egui::{Color32, Visuals};

pub const BG_PRIMARY: Color32 = Color32::from_rgb(0x0A, 0x0A, 0x10);
pub const BG_SECONDARY: Color32 = Color32::from_rgb(0x12, 0x12, 0x1C);

pub const ACCENT: Color32 = Color32::from_rgb(0xC9, 0x78, 0x50);
pub const GOLD: Color32 = Color32::from_rgb(0xD4, 0xA8, 0x55);

pub const DANGER: Color32 = Color32::from_rgb(0xE5, 0x48, 0x4D);
pub const WARNING: Color32 = Color32::from_rgb(0xF5, 0xA6, 0x23);
pub const SUCCESS: Color32 = Color32::from_rgb(0x46, 0xA7, 0x58);
pub const INFO: Color32 = Color32::from_rgb(0x3B, 0x9E, 0xFF);

pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(0xEC, 0xEC, 0xF1);
pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(0x8B, 0x8B, 0xA3);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(0x4E, 0x4E, 0x6A);
pub const BORDER: Color32 = Color32::from_rgb(0x26, 0x26, 0x40);

// ── The light palette's inks (`Palette::light`) ─────────────────────────

/// Primary text on Home and its forms.
const LIGHT_TEXT: Color32 = Color32::from_rgb(0x20, 0x22, 0x20);

/// Labels and secondary text: 6.8:1 on white, 6.3:1 on the sidebar.
const LIGHT_TEXT_SECONDARY: Color32 = Color32::from_rgb(0x5E, 0x5B, 0x55);

/// Every input's 1-point border: 3.8:1 on white and 3.5:1 on the sidebar,
/// above WCAG's 3:1 for the parts of a control that identify it.
const FIELD_BORDER: Color32 = Color32::from_rgb(0x86, 0x83, 0x7C);

/// The focus outline's width.
pub const FOCUS_RING_WIDTH: f32 = 2.0;

/// The type scale (ratio about 1.2, every step 1.15 to 1.23):
/// labels and secondary text.
pub const TEXT_LABEL: f32 = 13.0;
/// Body text and values.
pub const TEXT_BODY: f32 = 15.0;
/// The primary button and section titles.
pub const TEXT_STRONG: f32 = 18.0;
/// Form titles.
pub const TEXT_FORM_TITLE: f32 = 22.0;
/// A connection's name above its details.
pub const TEXT_HOST_TITLE: f32 = 26.0;

/// The scale, smallest first.
pub const TYPE_SCALE: [f32; 5] = [
    TEXT_LABEL,
    TEXT_BODY,
    TEXT_STRONG,
    TEXT_FORM_TITLE,
    TEXT_HOST_TITLE,
];

// ── One control system (Home and its forms) ──────────────────────────────

/// The ink every hover and pressed tint on a light surface is mixed from.
const INK: Color32 = LIGHT_TEXT;

/// The primary button's fill: the accent 18 % darker, so its white text
/// keeps 4.5:1 (on the accent itself it is 3.3:1).
pub const PRIMARY_FILL: Color32 = Color32::from_rgb(0xA5, 0x62, 0x42);

/// A secondary button's (and the disclosure's) hover: ink at this share
/// over white.
pub const HOVER_TINT_PERCENT: u8 = 5;
/// A secondary button's pressed fill: ink at this share over white.
pub const PRESSED_TINT_PERCENT: u8 = 9;
/// The primary button's hover: its fill this much darker.
pub const PRIMARY_HOVER_DARKEN_PERCENT: u8 = 8;
/// The primary button's pressed fill: its fill this much darker.
pub const PRIMARY_PRESSED_DARKEN_PERCENT: u8 = 14;

/// Every input, selector and button but the primary: exactly this tall.
pub const CONTROL_HEIGHT: f32 = 36.0;

/// The one primary button on a scene (Connect, Save, Add, OK).
pub const PRIMARY_HEIGHT: f32 = 44.0;

/// The primary button's text.
pub const PRIMARY_TEXT: f32 = 17.0;

/// Every input's, selector's and button's corner radius.
pub const CONTROL_RADIUS: f32 = 6.0;

/// Every button's text: the body size, primary and secondary alike.
pub const BUTTON_TEXT: f32 = TEXT_BODY;

/// The padding either side of a button's longest label: buttons side by
/// side are as wide as that label needs plus this on each side.
pub const BUTTON_PADDING_X: f32 = 20.0;

/// The space between two buttons side by side.
pub const BUTTON_GAP: f32 = 12.0;

/// Edit beside a connection's name.
pub const EDIT_BUTTON_WIDTH: f32 = 96.0;

/// Every scene's one column, centred in its pane: the inputs, Connect and
/// every row of buttons end on its right edge.
pub const COLUMN_MAX: f32 = 420.0;

// ── Spacing ──────────────────────────────────────────────────────────────

/// The spacing scale, smallest first.
pub const SPACE_SCALE: [f32; 7] = [4.0, 8.0, 12.0, 16.0, 24.0, 32.0, 48.0];

/// The space between the orange bar and the first line of both panes: the
/// sidebar's header and every scene's title.
pub const SCENE_TOP_INSET: f32 = 48.0;
/// A field's label to the field.
pub const LABEL_TO_FIELD: f32 = 6.0;
/// A field (or its help line) to the next field's label.
pub const FIELD_TO_LABEL: f32 = 20.0;
/// Between groups: Password to Connect, fields to the disclosure, the
/// disclosure to the buttons, the address line to the first field.
pub const GROUP_GAP: f32 = 32.0;
/// A title to the line under it.
pub const TITLE_TO_SUBTITLE: f32 = 4.0;
/// A form's title to its first field.
pub const TITLE_TO_FIELDS: f32 = 24.0;
/// Connect to the options under it.
pub const CONNECT_TO_OPTIONS: f32 = 16.0;
/// Between the options under Connect.
pub const OPTION_GAP: f32 = 10.0;
/// A selector to its help line.
pub const FIELD_TO_HELP: f32 = 6.0;
/// The sidebar list's inset from its edges.
pub const LIST_INSET: f32 = 16.0;
/// A sidebar row's padding above and below its two lines.
pub const ROW_PADDING_Y: f32 = 12.0;
/// The sidebar header to the search field.
pub const HEADER_TO_SEARCH: f32 = 24.0;

/// `ink` at `percent` % over white: a hover or pressed fill on a white
/// surface.
#[must_use]
pub fn tint(ink: Color32, percent: u8) -> Color32 {
    tint_on(Color32::WHITE, ink, percent)
}

/// `ink` at `percent` % over the opaque `surface`.
#[must_use]
pub fn tint_on(surface: Color32, ink: Color32, percent: u8) -> Color32 {
    over(surface, ink, percent_alpha(percent))
}

/// `colour` `percent` % darker (black at that share over it).
#[must_use]
pub fn darken(colour: Color32, percent: u8) -> Color32 {
    over(colour, Color32::BLACK, percent_alpha(percent))
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a share of 0..=100 % is 0..=255"
)]
fn percent_alpha(percent: u8) -> u8 {
    (f32::from(percent.min(100)) * 255.0 / 100.0).round() as u8
}

// ── The two palettes ─────────────────────────────────────────────────────

/// Every colour a screen paints, for one appearance. The Deck follows the
/// operating system's light or dark appearance ([`apply`]); a screen reads
/// the palette in force with [`colours`] and never names a colour itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Whether this is the dark palette.
    pub dark: bool,
    /// The scenes' background (Home's right side, Settings' pages).
    pub bg: Color32,
    /// The sidebars (Home's list, Settings' sections).
    pub panel: Color32,
    /// Menus, popups, dropdown lists, tooltips and boxes over a scene.
    pub raised: Color32,
    /// An input's fill.
    pub field: Color32,
    /// Text.
    pub text: Color32,
    /// Labels and secondary text.
    pub text2: Color32,
    /// Rules, panel edges and frames: subtle; in the dark palette no line
    /// is lighter, apart from text, the accent, the focus ring and a
    /// control's outline.
    pub border: Color32,
    /// A control's 1-point outline (an input, a list picker, a secondary
    /// button, a check box): 3:1 or more on every surface it sits on
    /// (WCAG 1.4.11).
    pub field_border: Color32,
    /// A checkbox's or radio button's box, and a scroll bar's handle.
    pub control: Color32,
    /// A quiet control under the pointer.
    pub hover: Color32,
    /// A quiet control pressed.
    pub pressed: Color32,
    /// A selected row: the accent tinted over the panel.
    pub selected: Color32,
    /// The brand, the selection bar, the chosen option and the focus ring.
    pub accent: Color32,
    /// Text in the accent (a chosen option): 4.5:1 on every surface.
    pub accent_text: Color32,
    /// The primary button (white text 4.5:1 in every state).
    pub primary: Color32,
    pub primary_hover: Color32,
    pub primary_pressed: Color32,
    /// Text on the primary button.
    pub on_primary: Color32,
    /// Errors under a field.
    pub error: Color32,
    /// A warning ("Not encrypted").
    pub warning: Color32,
    /// Something that went well (a passed check, "Ready").
    pub success: Color32,
    /// Information ("You’re signed in").
    pub info: Color32,
}

impl Palette {
    /// The light palette.
    #[must_use]
    pub fn light() -> Self {
        let panel = Color32::from_rgb(0xFA, 0xF7, 0xF1);
        Self {
            dark: false,
            bg: Color32::WHITE,
            panel,
            raised: Color32::WHITE,
            field: Color32::WHITE,
            text: LIGHT_TEXT,
            text2: LIGHT_TEXT_SECONDARY,
            border: Color32::from_rgb(0xCB, 0xC5, 0xB9),
            field_border: FIELD_BORDER,
            control: Color32::from_rgb(0xE6, 0xE3, 0xDD),
            hover: tint(INK, HOVER_TINT_PERCENT),
            pressed: tint(INK, PRESSED_TINT_PERCENT),
            selected: over(panel, ACCENT, SELECTED_TINT_ALPHA),
            accent: ACCENT,
            accent_text: Color32::from_rgb(0x9A, 0x55, 0x33),
            primary: PRIMARY_FILL,
            primary_hover: darken(PRIMARY_FILL, PRIMARY_HOVER_DARKEN_PERCENT),
            primary_pressed: darken(PRIMARY_FILL, PRIMARY_PRESSED_DARKEN_PERCENT),
            on_primary: Color32::WHITE,
            error: Color32::from_rgb(0xC0, 0x20, 0x20),
            warning: Color32::from_rgb(0x8A, 0x4B, 0x00),
            success: Color32::from_rgb(0x27, 0x6B, 0x32),
            info: Color32::from_rgb(0x2F, 0x5F, 0xA8),
        }
    }

    /// The dark palette: neutral greys for a dim grading suite.
    #[must_use]
    pub fn dark() -> Self {
        let bg = Color32::from_rgb(0x1E, 0x1F, 0x21);
        let panel = Color32::from_rgb(0x26, 0x27, 0x2A);
        let text = Color32::from_rgb(0xE8, 0xE6, 0xE3);
        let border = Color32::from_rgb(0x45, 0x47, 0x4B);
        Self {
            dark: true,
            bg,
            panel,
            raised: Color32::from_rgb(0x2E, 0x30, 0x33),
            field: Color32::from_rgb(0x2A, 0x2B, 0x2E),
            text,
            text2: Color32::from_rgb(0xA8, 0xA6, 0xA2),
            border,
            field_border: Color32::from_rgb(0x82, 0x85, 0x8A),
            control: border,
            hover: tint_on(bg, text, 7),
            pressed: tint_on(bg, text, 11),
            selected: over(panel, ACCENT, SELECTED_TINT_ALPHA),
            accent: ACCENT,
            accent_text: Color32::from_rgb(0xE0, 0x9A, 0x74),
            primary: PRIMARY_FILL,
            primary_hover: darken(PRIMARY_FILL, PRIMARY_HOVER_DARKEN_PERCENT),
            primary_pressed: darken(PRIMARY_FILL, PRIMARY_PRESSED_DARKEN_PERCENT),
            on_primary: Color32::WHITE,
            error: Color32::from_rgb(0xFF, 0x9C, 0x8F),
            warning: Color32::from_rgb(0xE3, 0xB8, 0x60),
            success: Color32::from_rgb(0x86, 0xC9, 0x8F),
            info: Color32::from_rgb(0x9E, 0xBB, 0xEB),
        }
    }

    /// The palette for a dark or a light appearance.
    #[must_use]
    pub fn of(dark: bool) -> Self {
        if dark { Self::dark() } else { Self::light() }
    }

    /// Every ink text is drawn in on the scenes' surfaces.
    #[cfg(test)]
    #[must_use]
    pub const fn inks(&self) -> [Color32; 7] {
        [
            self.text,
            self.text2,
            self.accent_text,
            self.error,
            self.warning,
            self.success,
            self.info,
        ]
    }

    /// Every surface text sits on, the hover and pressed fills included.
    #[cfg(test)]
    #[must_use]
    pub fn surfaces(&self) -> [Color32; 8] {
        [
            self.bg,
            self.panel,
            self.raised,
            self.field,
            self.hover,
            self.pressed,
            self.selected,
            tint_on(self.panel, self.text, 7),
        ]
    }
}

/// How much accent a selected row gets over its panel (about 15 %).
pub const SELECTED_TINT_ALPHA: u8 = 38;

/// The palette in force where `ui` draws.
#[must_use]
pub fn colours(ui: &egui::Ui) -> Palette {
    Palette::of(ui.visuals().dark_mode)
}

/// The palette in force for `ctx` (for a box or window drawn on it).
#[must_use]
pub fn colours_of(ctx: &egui::Context) -> Palette {
    Palette::of(ctx.global_style().visuals.dark_mode)
}

/// egui's visuals for `palette`: panels, windows, menus, popups, dropdown
/// lists, tooltips, separators, scroll bars and every stock widget take
/// their colours from it, so nothing egui draws itself stays light in the
/// dark palette.
#[must_use]
pub fn visuals(palette: &Palette) -> Visuals {
    let p = palette;
    let mut visuals = if p.dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    visuals.override_text_color = None;
    visuals.weak_text_color = Some(p.text2);
    visuals.hyperlink_color = p.text;
    visuals.panel_fill = p.bg;
    visuals.window_fill = p.raised;
    visuals.window_stroke = egui::Stroke::new(1.0, p.border);
    visuals.extreme_bg_color = p.field;
    visuals.text_edit_bg_color = Some(p.field);
    visuals.faint_bg_color = p.panel;
    visuals.code_bg_color = p.panel;
    visuals.warn_fg_color = p.warning;
    visuals.error_fg_color = p.error;
    visuals.selection.bg_fill = over(p.field, p.accent, 90);
    visuals.selection.stroke = egui::Stroke::new(1.0, p.text);
    visuals.text_cursor.stroke = egui::Stroke::new(2.0, p.text);
    // A disabled control fades, but its text still reads at 4.5:1.
    visuals.disabled_alpha = DISABLED_ALPHA;
    let shadow = Color32::from_black_alpha(if p.dark { 110 } else { 40 });
    visuals.popup_shadow.color = shadow;
    visuals.window_shadow.color = shadow;
    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_fill = p.bg;
    widgets.noninteractive.weak_bg_fill = p.bg;
    widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, p.border);
    widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, p.text);
    for (state, fill, weak) in [
        (&mut widgets.inactive, p.control, p.raised),
        (&mut widgets.hovered, p.hover, p.hover),
        (&mut widgets.active, p.pressed, p.pressed),
        (&mut widgets.open, p.hover, p.hover),
    ] {
        state.bg_fill = fill;
        state.weak_bg_fill = weak;
        state.bg_stroke = egui::Stroke::new(1.0, p.field_border);
        state.fg_stroke = egui::Stroke::new(1.5, p.text);
        state.expansion = 0.0;
    }
    visuals
}

/// How opaque a disabled control is: faded, its text still 4.5:1.
const DISABLED_ALPHA: f32 = 0.65;

/// egui's own animation time (popups fading, lists sliding open) while
/// motion is not reduced.
const ANIMATION_SECONDS: f32 = 1.0 / 12.0;

fn reduce_motion_id() -> egui::Id {
    egui::Id::new("arcen_reduce_motion")
}

/// Tells the screens whether the operating system asks for reduced motion
/// (a shell reads it each pass: macOS "Reduce motion", Windows "Show
/// animations in Windows" off). While it is on, every animation is
/// instant: egui's own and the screens' (the list picker's slide).
pub fn set_reduce_motion(ctx: &egui::Context, reduce: bool) {
    if reduce_motion(ctx) == reduce
        && ctx
            .data(|data| data.get_temp::<bool>(reduce_motion_id()))
            .is_some()
    {
        return;
    }
    ctx.data_mut(|data| data.insert_temp(reduce_motion_id(), reduce));
    ctx.all_styles_mut(|style| {
        style.animation_time = if reduce { 0.0 } else { ANIMATION_SECONDS };
    });
}

/// Whether the operating system asks for reduced motion (see
/// [`set_reduce_motion`]); off until a shell says otherwise.
#[must_use]
pub fn reduce_motion(ctx: &egui::Context) -> bool {
    ctx.data(|data| data.get_temp::<bool>(reduce_motion_id()))
        .unwrap_or(false)
}

/// How long an animation of `seconds` takes now: none while motion is
/// reduced.
#[must_use]
pub fn motion_seconds(ctx: &egui::Context, seconds: f32) -> f32 {
    if reduce_motion(ctx) { 0.0 } else { seconds }
}

/// Themes the Deck: both palettes into egui, egui following the operating
/// system's light or dark appearance (live: when the system switches, the
/// next pass draws in the other palette), the scroll bars drawn from the
/// palette rather than the text colour, and the symbol font.
pub fn apply(ctx: &egui::Context) {
    for (theme, palette) in [
        (egui::Theme::Light, Palette::light()),
        (egui::Theme::Dark, Palette::dark()),
    ] {
        ctx.set_visuals_of(theme, visuals(&palette));
        ctx.style_mut_of(theme, |style| {
            style.spacing.scroll.foreground_color = false;
            style.animation_time = ANIMATION_SECONDS;
        });
    }
    ctx.set_theme(egui::ThemePreference::System);
    // Until the window system has said which appearance it has (a shell
    // passes it from the first pass), the light palette.
    ctx.options_mut(|options| options.fallback_theme = egui::Theme::Light);
    ctx.set_fonts(fonts_with_symbol_fallback());
}

/// The bundled font that draws the key and arrow symbols the text font
/// lacks.
pub const SYMBOL_FALLBACK_FONT: &str = "Hack";

/// Symbols the text uses beyond Latin-1 that the Deck's text font draws,
/// once [`apply`] has added [`SYMBOL_FALLBACK_FONT`] behind it. Anything
/// else outside Latin-1 draws as an empty box: spell the key out instead
/// ("Control", "Option"). The macOS Deck's tests check this list against
/// the real fonts.
pub const DRAWN_SYMBOLS: &str = "⌘⇧←→↑↓↻▸›‹·…—–×“”‘’•°⚠⛔📂📖";

/// egui's bundled fonts, with [`SYMBOL_FALLBACK_FONT`] (bundled already,
/// for monospace text) as the last fallback of proportional text: the text
/// font has no arrows, ⇧ or ▸, which drew as empty boxes. ⌃ and ⌥ are in
/// none of the bundled fonts, so the screens spell them out.
#[must_use]
pub fn fonts_with_symbol_fallback() -> egui::FontDefinitions {
    let mut fonts = egui::FontDefinitions::default();
    if fonts.font_data.contains_key(SYMBOL_FALLBACK_FONT)
        && let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional)
        && !family.iter().any(|name| name == SYMBOL_FALLBACK_FONT)
    {
        family.push(SYMBOL_FALLBACK_FONT.to_owned());
    }
    fonts
}

/// `top` at `alpha` (0–255) painted over the opaque `base`.
#[must_use]
pub fn over(base: Color32, top: Color32, alpha: u8) -> Color32 {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a blend of two values in 0..=255 stays in it"
    )]
    fn blend(base: u8, top: u8, alpha: u8) -> u8 {
        let (base, top) = (f32::from(base), f32::from(top));
        (base + (top - base) * f32::from(alpha) / 255.0).round() as u8
    }
    Color32::from_rgb(
        blend(base.r(), top.r(), alpha),
        blend(base.g(), top.g(), alpha),
        blend(base.b(), top.b(), alpha),
    )
}

/// WCAG 2.x relative luminance of an sRGB colour.
#[must_use]
pub fn relative_luminance(colour: Color32) -> f32 {
    let linear = |channel: u8| {
        let channel = f32::from(channel) / 255.0;
        if channel <= 0.040_45 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(colour.r()) + 0.7152 * linear(colour.g()) + 0.0722 * linear(colour.b())
}

/// WCAG 2.x contrast ratio between two colours (1 to 21).
#[must_use]
pub fn contrast_ratio(a: Color32, b: Color32) -> f32 {
    let (a, b) = (relative_luminance(a), relative_luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// What the screens' tests check every frame against.
#[cfg(test)]
pub(crate) mod checks {
    /// Each text a pass drew: the text, its colour and the opaque colour
    /// under it (every fill painted before it there, over white).
    pub(crate) fn texts_on_their_ground(
        shapes: &[egui::Shape],
        base: egui::Color32,
    ) -> Vec<(String, egui::Color32, egui::Color32)> {
        // Premultiplied `top` over the opaque `ground`.
        fn composite(ground: egui::Color32, top: egui::Color32) -> egui::Color32 {
            let keep = u16::from(255 - top.a());
            let mix = |g: u8, t: u8| {
                u8::try_from(u16::from(t) + (u16::from(g) * keep + 127) / 255).unwrap_or(255)
            };
            egui::Color32::from_rgb(
                mix(ground.r(), top.r()),
                mix(ground.g(), top.g()),
                mix(ground.b(), top.b()),
            )
        }
        let mut fills: Vec<(egui::Rect, egui::Color32)> = Vec::new();
        let mut out = Vec::new();
        for shape in shapes {
            match shape {
                egui::Shape::Rect(rect) if rect.fill.a() > 0 => fills.push((rect.rect, rect.fill)),
                egui::Shape::Text(text) if !text.galley.job.text.trim().is_empty() => {
                    let at = text.pos + text.galley.rect.center().to_vec2();
                    let ground = fills
                        .iter()
                        .filter(|(rect, _)| rect.expand(0.5).contains(at))
                        .fold(base, |ground, (_, fill)| composite(ground, *fill));
                    let colour = text.override_text_color.unwrap_or_else(|| {
                        match text.galley.job.sections.first().map(|s| s.format.color) {
                            Some(colour) if colour != egui::Color32::PLACEHOLDER => colour,
                            _ => text.fallback_color,
                        }
                    });
                    // A faded (disabled) text over what is under it.
                    let colour = if colour.a() == 255 {
                        colour
                    } else {
                        composite(ground, colour)
                    };
                    out.push((text.galley.job.text.clone(), colour, ground));
                }
                _ => {}
            }
        }
        out
    }

    /// Every colour a pass painted that is not text: rectangle fills and
    /// strokes, lines, paths and circles (transparent ones over `base`).
    pub(crate) fn painted_colours(
        shapes: &[egui::Shape],
        base: egui::Color32,
    ) -> Vec<egui::Color32> {
        let opaque = |colour: egui::Color32| {
            if colour.a() == 255 {
                colour
            } else {
                let [r, g, b, a] = colour.to_array();
                let keep = u16::from(255 - a);
                let mix = |base: u8, top: u8| {
                    u8::try_from(u16::from(top) + (u16::from(base) * keep + 127) / 255)
                        .unwrap_or(255)
                };
                egui::Color32::from_rgb(mix(base.r(), r), mix(base.g(), g), mix(base.b(), b))
            }
        };
        let mut out = Vec::new();
        for shape in shapes {
            match shape {
                egui::Shape::Rect(rect) if rect.blur_width == 0.0 => {
                    // A translucent black fill is a dim behind a box, never
                    // a line.
                    let dim = rect.fill.a() < 255 && rect.fill.to_opaque() == egui::Color32::BLACK;
                    if rect.fill.a() > 0 && !dim {
                        out.push(opaque(rect.fill));
                    }
                    if rect.stroke.width > 0.0 && rect.stroke.color.a() > 0 {
                        out.push(opaque(rect.stroke.color));
                    }
                }
                egui::Shape::LineSegment { stroke, .. } if stroke.color.a() > 0 => {
                    out.push(opaque(stroke.color));
                }
                egui::Shape::Path(path) => {
                    if path.fill.a() > 0 {
                        out.push(opaque(path.fill));
                    }
                    if let egui::epaint::PathStroke {
                        color: egui::epaint::ColorMode::Solid(colour),
                        width,
                        ..
                    } = &path.stroke
                        && *width > 0.0
                        && colour.a() > 0
                    {
                        out.push(opaque(*colour));
                    }
                }
                egui::Shape::Circle(circle) if circle.fill.a() > 0 => {
                    out.push(opaque(circle.fill));
                }
                _ => {}
            }
        }
        out
    }

    /// Whether `colour` is one a line or fill may be: no more contrast with
    /// the background than the border has (in the dark palette, no lighter
    /// than the border), unless it is an ink (text and icons), the accent,
    /// the primary or a control's outline (which must stand out: 3:1).
    pub(crate) fn quiet_enough(colours: &super::Palette, colour: egui::Color32) -> bool {
        use super::contrast_ratio;
        // A disabled control's outline: the outline, faded.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a share of 255"
        )]
        let disabled_outline = super::over(
            colours.bg,
            colours.field_border,
            (super::DISABLED_ALPHA * 255.0).round() as u8,
        );
        let loud = [
            colours.accent,
            colours.primary,
            colours.primary_hover,
            colours.primary_pressed,
            colours.on_primary,
        ];
        colours.inks().contains(&colour)
            || loud.contains(&colour)
            || colour == colours.field_border
            || (contrast_ratio(colour, colours.bg) - contrast_ratio(disabled_outline, colours.bg))
                .abs()
                < 0.1
            || contrast_ratio(colour, colours.bg)
                <= contrast_ratio(colours.border, colours.bg) + 0.01
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_type_scale_steps_by_about_1_2() {
        for pair in TYPE_SCALE.windows(2) {
            let ratio = pair[1] / pair[0];
            assert!(
                (1.125..=1.25).contains(&ratio),
                "{} to {}: {ratio:.3}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn tints_and_darkening_go_the_right_way() {
        // Ink at 5 % over white is nearly white, never nearly black.
        let hover = tint(INK, HOVER_TINT_PERCENT);
        assert!(relative_luminance(hover) > 0.85, "{hover:?}");
        assert!(relative_luminance(tint(INK, PRESSED_TINT_PERCENT)) < relative_luminance(hover));
        assert_eq!(darken(PRIMARY_FILL, 0), PRIMARY_FILL);
        assert!(relative_luminance(darken(PRIMARY_FILL, 8)) < relative_luminance(PRIMARY_FILL));
        assert_eq!(darken(ACCENT, 18), PRIMARY_FILL);
        for pair in SPACE_SCALE.windows(2) {
            assert!(pair[0] < pair[1]);
        }
    }

    #[test]
    fn inputs_and_their_focus_ring_stand_out_from_every_surface() {
        let sidebar = Color32::from_rgb(0xFA, 0xF7, 0xF1);
        for surface in [Color32::WHITE, sidebar] {
            for (what, colour, minimum) in [
                ("border", FIELD_BORDER, 3.0),
                ("focus ring", ACCENT, 3.0),
                ("label", LIGHT_TEXT_SECONDARY, 4.5),
                ("text", LIGHT_TEXT, 4.5),
            ] {
                let ratio = contrast_ratio(colour, surface);
                assert!(ratio >= minimum, "{what}: {ratio:.2}:1 on {surface:?}");
            }
        }
    }

    /// Every shape one pass drew, flattened.
    fn flat(output: egui::FullOutput) -> Vec<egui::Shape> {
        fn flatten(shape: egui::Shape, out: &mut Vec<egui::Shape>) {
            match shape {
                egui::Shape::Vec(shapes) => shapes.into_iter().for_each(|s| flatten(s, out)),
                shape => out.push(shape),
            }
        }
        let mut shapes = Vec::new();
        for clipped in output.shapes {
            flatten(clipped.shape, &mut shapes);
        }
        shapes
    }

    #[test]
    fn the_system_appearance_rethemes_menus_lists_popups_and_tooltips() {
        let ctx = egui::Context::default();
        apply(&ctx);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));
        let mut choice = 0_u8;
        let where_ = std::cell::Cell::new((egui::Pos2::ZERO, egui::Pos2::ZERO, egui::Pos2::ZERO));
        let mut pass = |system: egui::Theme, events: Vec<egui::Event>| {
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(screen),
                    system_theme: Some(system),
                    events,
                    ..egui::RawInput::default()
                },
                |ui| {
                    let menu = ui.button("Menu");
                    egui::Popup::menu(&menu).show(|ui| {
                        let _ = ui.button("Menu item");
                    });
                    let list = egui::ComboBox::from_id_salt("list")
                        .selected_text("Chosen")
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut choice, 0, "List item");
                        })
                        .response;
                    let tip = ui.button("Tip here");
                    // The tooltip egui shows under the pointer, kept open.
                    tip.show_tooltip_text("A tooltip");
                    where_.set((menu.rect.center(), list.rect.center(), tip.rect.center()));
                },
            );
            flat(output)
        };
        let click = |at: egui::Pos2| {
            vec![
                egui::Event::PointerMoved(at),
                egui::Event::PointerButton {
                    pos: at,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerButton {
                    pos: at,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ]
        };
        for system in [egui::Theme::Light, egui::Theme::Dark, egui::Theme::Light] {
            let palette = Palette::of(system == egui::Theme::Dark);
            let _ = pass(system, Vec::new());
            let (menu, list, tip) = where_.get();
            // Each floating surface, opened in turn: its frame is the
            // palette's raised surface and its text the palette's ink.
            for (what, events, item) in [
                ("menu", click(menu), "Menu item"),
                ("list", click(list), "List item"),
                ("tooltip", vec![egui::Event::PointerMoved(tip)], "A tooltip"),
            ] {
                let _ = pass(system, events);
                // Popups fade in over a few passes.
                let hold = if what == "tooltip" {
                    vec![egui::Event::PointerMoved(tip)]
                } else {
                    Vec::new()
                };
                let mut shapes = Vec::new();
                for _ in 0..12 {
                    shapes = pass(system, hold.clone());
                }
                let text = shapes
                    .iter()
                    .find_map(|shape| match shape {
                        egui::Shape::Text(text) if text.galley.job.text == item => Some(text),
                        _ => None,
                    })
                    .unwrap_or_else(|| panic!("{system:?}: no {what}"));
                let at = text.pos;
                let frame = shapes
                    .iter()
                    .find_map(|shape| match shape {
                        egui::Shape::Rect(rect)
                            if rect.blur_width == 0.0
                                && rect.fill.a() == 255
                                && rect.rect.contains(at + egui::vec2(1.0, 1.0)) =>
                        {
                            Some(rect.fill)
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| {
                        let near: Vec<_> = shapes
                            .iter()
                            .filter_map(|shape| match shape {
                                egui::Shape::Rect(rect) => {
                                    Some((rect.rect, rect.fill, rect.blur_width))
                                }
                                _ => None,
                            })
                            .collect();
                        panic!("{system:?}: {what} has no frame at {at:?}: {near:?}")
                    });
                assert_eq!(frame, palette.raised, "{system:?}: {what}'s frame");
                let colour = text
                    .galley
                    .job
                    .sections
                    .first()
                    .map(|section| section.format.color)
                    .filter(|colour| *colour != Color32::PLACEHOLDER)
                    .unwrap_or(text.fallback_color);
                assert!(
                    contrast_ratio(colour, palette.raised) >= 4.5,
                    "{system:?}: {what}'s text {colour:?}"
                );
                // Close it again: a click on nothing.
                let _ = pass(system, click(egui::pos2(790.0, 590.0)));
            }
        }
    }

    #[test]
    fn every_ink_keeps_4_5_to_1_on_every_surface_in_both_palettes() {
        for palette in [Palette::light(), Palette::dark()] {
            for surface in palette.surfaces() {
                for ink in palette.inks() {
                    let ratio = contrast_ratio(ink, surface);
                    assert!(ratio >= 4.5, "{ink:?} on {surface:?}: {ratio:.2}");
                }
            }
            for fill in [
                palette.primary,
                palette.primary_hover,
                palette.primary_pressed,
            ] {
                assert!(contrast_ratio(palette.on_primary, fill) >= 4.5);
            }
        }
    }

    #[test]
    fn a_controls_outline_keeps_3_to_1_on_every_surface_in_both_palettes() {
        for palette in [Palette::light(), Palette::dark()] {
            for surface in [
                palette.bg,
                palette.panel,
                palette.raised,
                palette.field,
                palette.hover,
                palette.pressed,
            ] {
                let ratio = contrast_ratio(palette.field_border, surface);
                assert!(
                    ratio >= 3.0,
                    "dark {}: {:?} on {surface:?} is {ratio:.2}:1",
                    palette.dark,
                    palette.field_border
                );
            }
            // Panel edges and frames stay quieter than a control.
            assert!(
                contrast_ratio(palette.border, palette.bg)
                    < contrast_ratio(palette.field_border, palette.bg)
            );
        }
    }

    #[test]
    fn reduced_motion_makes_every_animation_instant() {
        let ctx = egui::Context::default();
        apply(&ctx);
        assert!(!reduce_motion(&ctx));
        assert!(ctx.global_style().animation_time > 0.0);
        assert!(motion_seconds(&ctx, 0.12) > 0.0);
        set_reduce_motion(&ctx, true);
        assert!(reduce_motion(&ctx));
        assert!(motion_seconds(&ctx, 0.12).abs() < f32::EPSILON);
        for theme in [egui::Theme::Light, egui::Theme::Dark] {
            ctx.set_theme(theme);
            assert!(ctx.global_style().animation_time.abs() < f32::EPSILON);
        }
        set_reduce_motion(&ctx, false);
        assert!(ctx.global_style().animation_time > 0.0);
    }
}
