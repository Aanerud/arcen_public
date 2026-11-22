// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Settings: the eight panels (Language, Streaming, Displays, Microphone,
//! Input, Tablet, Logs, Advanced) with the same wording on both Decks.
//! Input, Tablet and Advanced › Input take their words from
//! [`super::input_choices`].
//!
//! The shell hands in a copy of the current values ([`SettingsValues`]) and
//! what the platform knows ([`SettingsFacts`]); [`render_settings`] edits the
//! copy and returns what the user asked for ([`SettingsAction`]). The shell
//! compares, persists and applies, exactly once per pass.

use arcen_input::pen_buttons::{PenButtonAction, PenButtonSettings};
use arcen_input::text::paste::PasteSettings;
use arcen_input::text::{OptionKeyPreference, TypingMode};
use arcen_protocol::messages::{ClientMonitor, CursorMode, TabletModeMsg};

use super::input_choices::{self as choices, Choice};
use super::list_picker::list_picker;
use super::theme;
use super::widgets::{settings_nav_item, settings_nav_section_label};
use super::wording::{DeckPlatform, deck_platform};
use crate::settings::{
    ColorFidelity, ColorFidelitySettings, DisplaysMode, LogLevel, PerformanceMode, RemoteUiScale,
    SecurityMode, StreamingPreset, color_fidelity_bit_depth_label, color_fidelity_chroma_label,
    color_fidelity_codec_label, color_fidelity_matrix_label, color_fidelity_range_label,
    effective_color_fidelity_variant,
};

/// Which settings panel is showing. Mirrors the reference client's left nav.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SettingsTab {
    #[default]
    Language,
    Streaming,
    Displays,
    Microphone,
    Input,
    /// The tablet mode, one choice for every connection (0.20.7), directly
    /// under Input.
    Tablet,
    Logs,
    Advanced,
}

impl SettingsTab {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Language => "Language",
            Self::Streaming => "Streaming",
            Self::Displays => "Displays",
            Self::Microphone => "Microphone",
            Self::Input => "Input",
            Self::Tablet => "Tablet",
            Self::Logs => "Logs",
            Self::Advanced => "Advanced",
        }
    }
}

/// Settings › Streaming description of **Exact still image** (lossless
/// refinement, ADR 0016), in the user's terms.
pub const EXACT_STILL_IMAGE_DESCRIPTION: &str = "When the picture stops moving, the host sends its \
exact pixels, losslessly, and the Deck checks them. Grading and HDR only, when the host offers \
it; uses spare bandwidth only while the picture is still. Applies from the next connection.";

/// What the Canvas screen updates switch does (ADR 0028).
pub const CANVAS_DESCRIPTION: &str = "With a host that encodes in software, this Deck keeps the \
picture and the host sends only what changed: moved areas as copies, text and controls as exact \
tiles, and video only where things move. Applies from the next connection.";

/// Why HDR cannot be chosen. On Windows the usual cause is "Use HDR" being
/// off for the display (PARITY Q-13).
#[must_use]
pub const fn no_hdr_screen(platform: DeckPlatform) -> &'static str {
    match platform {
        DeckPlatform::Mac => {
            "No HDR-capable screen found. Connect an HDR display to use this preset."
        }
        DeckPlatform::Pc => {
            "No HDR-capable screen found. Turn on Use HDR in Windows display settings, or \
             connect an HDR display."
        }
    }
}

/// The values the panels edit. The shell fills it from its state, passes it
/// to [`render_settings`] and reads it back.
#[derive(Debug, Clone, PartialEq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one flag per on/off setting, as the panels show them"
)]
pub struct SettingsValues {
    pub tab: SettingsTab,
    pub log_level: LogLevel,
    pub security: SecurityMode,
    pub performance: PerformanceMode,
    pub color_fidelity: ColorFidelitySettings,
    pub displays: DisplaysMode,
    pub hidpi: bool,
    pub use_notch_area: bool,
    pub remote_ui_scale_percent: u32,
    pub full_color_display_mask: u8,
    pub hidpi_display_mask: u8,
    pub cursor_preference: CursorMode,
    pub typing_mode: TypingMode,
    pub option_key_mode: OptionKeyPreference,
    pub paste_settings: PasteSettings,
    pub clipboard_enabled: bool,
    pub microphone_enabled: bool,
    pub tablet_mode_requested: TabletModeMsg,
    pub pen_buttons: PenButtonSettings,
    pub exact_still_image: bool,
    pub canvas: bool,
    pub keyboard_capture: arcen_input::keyboard::KeyboardCapturePolicy,
    pub capture_mac_system_shortcuts: bool,
    pub keyboard_switch: arcen_input::keyboard::KeyboardSwitchChord,
}

impl SettingsValues {
    /// The panels' values from the saved settings, showing `tab`.
    #[must_use]
    pub fn from_client_settings(
        settings: &crate::settings::ClientSettings,
        tab: SettingsTab,
    ) -> Self {
        Self {
            tab,
            log_level: settings.log_level,
            security: settings.security_mode,
            performance: settings.performance_mode,
            color_fidelity: settings.color_fidelity,
            displays: settings.displays_mode,
            hidpi: settings.hidpi_streaming,
            use_notch_area: settings.fullscreen_uses_notch_area,
            remote_ui_scale_percent: settings.remote_ui_scale_percent,
            full_color_display_mask: settings.full_color_display_mask,
            hidpi_display_mask: settings.hidpi_display_mask,
            cursor_preference: settings.cursor_preference,
            typing_mode: settings.typing_mode,
            option_key_mode: settings.option_key_mode,
            paste_settings: settings.paste,
            clipboard_enabled: settings.clipboard_enabled,
            microphone_enabled: settings.microphone_enabled,
            tablet_mode_requested: settings.tablet_mode_requested,
            pen_buttons: settings.pen_buttons,
            exact_still_image: settings.exact_still_image,
            canvas: settings.canvas,
            keyboard_capture: settings.keyboard_capture,
            capture_mac_system_shortcuts: settings.capture_mac_system_shortcuts,
            keyboard_switch: settings.keyboard_switch,
        }
    }

    /// Writes the panels' values back into `settings`, with what follows
    /// from them (the preset's encode intent, typed pen for Tablet
    /// support). Returns whether anything that is saved changed.
    pub fn apply_to(&self, settings: &mut crate::settings::ClientSettings) -> bool {
        let encode_intent = self.color_fidelity.preset.encode_intent();
        let tablet_input_enabled =
            crate::settings::tablet_mode_allows_typed_pen(self.tablet_mode_requested);
        let changed = Self::from_client_settings(settings, self.tab) != *self
            || settings.encode_intent != encode_intent
            || settings.tablet_input_enabled != tablet_input_enabled;
        settings.log_level = self.log_level;
        settings.security_mode = self.security;
        settings.performance_mode = self.performance;
        settings.color_fidelity = self.color_fidelity;
        settings.encode_intent = encode_intent;
        settings.displays_mode = self.displays;
        settings.hidpi_streaming = self.hidpi;
        settings.fullscreen_uses_notch_area = self.use_notch_area;
        settings.remote_ui_scale_percent = self.remote_ui_scale_percent;
        settings.full_color_display_mask = self.full_color_display_mask;
        settings.hidpi_display_mask = self.hidpi_display_mask;
        settings.cursor_preference = self.cursor_preference;
        settings.typing_mode = self.typing_mode;
        settings.option_key_mode = self.option_key_mode;
        settings.paste = self.paste_settings;
        settings.clipboard_enabled = self.clipboard_enabled;
        settings.microphone_enabled = self.microphone_enabled;
        settings.tablet_mode_requested = self.tablet_mode_requested;
        settings.tablet_input_enabled = tablet_input_enabled;
        settings.pen_buttons = self.pen_buttons;
        settings.exact_still_image = self.exact_still_image;
        settings.canvas = self.canvas;
        settings.keyboard_capture = self.keyboard_capture;
        settings.capture_mac_system_shortcuts = self.capture_mac_system_shortcuts;
        settings.keyboard_switch = self.keyboard_switch;
        changed
    }
}

/// How this Deck takes the system shortcuts.
const fn shortcut_route(platform: DeckPlatform) -> crate::settings::SystemShortcutRoute {
    platform.system_shortcut_route()
}

/// The settings the system-shortcuts switch reads and writes.
fn shortcut_settings(
    keyboard_capture: arcen_input::keyboard::KeyboardCapturePolicy,
    capture_mac_system_shortcuts: bool,
) -> crate::settings::ClientSettings {
    crate::settings::ClientSettings {
        keyboard_capture,
        capture_mac_system_shortcuts,
        ..crate::settings::ClientSettings::default()
    }
}

/// Settings › Input › Keyboard's one switch, as this Deck takes the system
/// shortcuts ([`crate::settings::ClientSettings::system_shortcuts_to_host`]).
fn system_shortcuts_to_host(
    platform: DeckPlatform,
    keyboard_capture: arcen_input::keyboard::KeyboardCapturePolicy,
    capture_mac_system_shortcuts: bool,
) -> bool {
    shortcut_settings(keyboard_capture, capture_mac_system_shortcuts)
        .system_shortcuts_to_host(shortcut_route(platform))
}

/// Turns the switch on or off: on a Mac the event tap and the remote
/// computer's policy, on a PC the keyboard hook's policy.
fn set_system_shortcuts_to_host(
    platform: DeckPlatform,
    on: bool,
    keyboard_capture: &mut arcen_input::keyboard::KeyboardCapturePolicy,
    capture_mac_system_shortcuts: &mut bool,
) {
    let mut settings = shortcut_settings(*keyboard_capture, *capture_mac_system_shortcuts);
    settings.set_system_shortcuts_to_host(shortcut_route(platform), on);
    *keyboard_capture = settings.keyboard_capture;
    *capture_mac_system_shortcuts = settings.capture_mac_system_shortcuts;
}

/// One display in the Displays panel.
#[derive(Debug, Clone)]
pub struct SettingsMonitor {
    pub monitor: ClientMonitor,
    /// What a fullscreen session streams with `HiDPI` streaming off and on,
    /// when the platform can say (`[off, on]`).
    pub fullscreen_size: [Option<[u32; 2]>; 2],
}

impl SettingsMonitor {
    #[must_use]
    pub const fn fullscreen_size(&self, hidpi: bool) -> Option<[u32; 2]> {
        self.fullscreen_size[hidpi as usize]
    }
}

/// Where the Native tablet component stands: its guidance reads as a note
/// while it waits for the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentTint {
    /// Installed and running.
    Ready,
    /// Waiting for the user (approval in system settings).
    Attention,
    /// Something to do (install, start, recheck).
    Action,
}

/// Where the Native tablet component stands, as the Input panel shows it:
/// the button on the option's own row, its guidance, and the last error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeTabletControl {
    pub action_label: String,
    pub guidance: String,
    pub tint: ComponentTint,
    pub ready: bool,
    pub error: Option<String>,
}

impl NativeTabletControl {
    /// The control for a shared component state
    /// ([`arcen_usb_bridge::NativeUsbComponentState`]).
    #[must_use]
    pub fn from_component(
        state: &arcen_usb_bridge::NativeUsbComponentState,
        platform: arcen_usb_bridge::DeckPlatform,
        error: Option<String>,
    ) -> Self {
        use arcen_usb_bridge::NativeUsbComponentState as State;
        let tint = match state {
            State::Ready => ComponentTint::Ready,
            State::AwaitingApproval => ComponentTint::Attention,
            _ => ComponentTint::Action,
        };
        Self {
            action_label: state.action_label(platform).to_owned(),
            guidance: state.guidance(platform),
            tint,
            ready: state.is_ready(),
            error,
        }
    }
}

/// The Native tablet component's control for the Settings screen, through
/// [`crate::DeckServices`]: the state is read once and again after an
/// action (each read may be a system round trip), and the action runs on
/// its own thread because it may wait for an administrator prompt.
#[derive(Debug, Default)]
pub struct NativeTabletComponent {
    state: ComponentReading,
    error: Option<String>,
    running: Option<ActionSlot>,
}

/// Where a finished action leaves its result.
type ActionSlot = std::sync::Arc<std::sync::Mutex<Option<Result<(), String>>>>;

#[derive(Debug, Default)]
enum ComponentReading {
    /// Not read since the last action.
    #[default]
    Unread,
    /// This build has no Native tablet bridge.
    NoBridge,
    Read(arcen_usb_bridge::NativeUsbComponentState),
}

impl NativeTabletComponent {
    /// The control the Input panel shows, or `None` without a bridge.
    pub fn control(
        &mut self,
        services: &dyn crate::DeckServices,
        platform: DeckPlatform,
    ) -> Option<NativeTabletControl> {
        let platform = match platform {
            DeckPlatform::Mac => arcen_usb_bridge::DeckPlatform::MacOs,
            DeckPlatform::Pc => arcen_usb_bridge::DeckPlatform::Windows,
        };
        self.collect();
        if matches!(self.state, ComponentReading::Unread) {
            self.state = services
                .native_usb_component_state()
                .map_or(ComponentReading::NoBridge, ComponentReading::Read);
        }
        let ComponentReading::Read(state) = &self.state else {
            return None;
        };
        Some(NativeTabletControl::from_component(
            state,
            platform,
            self.error.clone(),
        ))
    }

    /// Whether an action is still running.
    #[must_use]
    pub const fn busy(&self) -> bool {
        self.running.is_some()
    }

    /// The control was clicked: runs the state's action on its own thread
    /// and calls `wake` when it finishes. Ignored while one runs.
    pub fn clicked(
        &mut self,
        services: std::sync::Arc<dyn crate::DeckServices>,
        wake: impl FnOnce() + Send + 'static,
    ) {
        if self.running.is_some() {
            return;
        }
        let ComponentReading::Read(state) = &self.state else {
            return;
        };
        let action = state.action();
        self.error = None;
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let result = std::sync::Arc::clone(&slot);
        let spawned = std::thread::Builder::new()
            .name("arcen-native-tablet".to_owned())
            .spawn(move || {
                let outcome = services.run_native_usb_component_action(action);
                if let Ok(mut result) = result.lock() {
                    *result = Some(outcome);
                }
                wake();
            });
        match spawned {
            Ok(_) => self.running = Some(slot),
            Err(error) => self.error = Some(format!("could not start: {error}")),
        }
    }

    /// Takes a finished action's result: its error is shown, and the state
    /// is read again either way.
    fn collect(&mut self) {
        let Some(slot) = &self.running else {
            return;
        };
        let Some(result) = slot.lock().ok().and_then(|mut result| result.take()) else {
            return;
        };
        self.running = None;
        self.state = ComponentReading::Unread;
        if let Err(error) = result {
            tracing::warn!(
                target: crate::log_target::UI,
                %error,
                "Native tablet component action failed"
            );
            // Shown, not only logged: the click had no visible effect
            // otherwise, which reads as a broken button.
            self.error = Some(error);
        }
    }
}

/// What the platform knows that the panels show but do not edit.
#[derive(Debug, Clone)]
pub struct SettingsFacts<'a> {
    /// Whether any screen can show HDR (the HDR preset is greyed otherwise).
    pub hdr_screen: bool,
    /// `Some(has_notch)` where the Deck has a notch setting at all.
    pub notch: Option<bool>,
    pub monitors: &'a [SettingsMonitor],
    /// What the current session says about its host's clipboard policy
    /// ([`crate::clipboard::HostClipboard::connection_notice`], with the
    /// clipboard value the session connected with), shown under Input ›
    /// Clipboard; `None` outside a session.
    pub host_clipboard_notice: Option<&'static str>,
    /// The saved connections, for "N connections use their own".
    pub connections: &'a [crate::config::SavedConnectionSummary],
    /// The Native tablet component, or `None` when this build has none.
    pub native_tablet: Option<NativeTabletControl>,
    /// What the decoder proved, for why Grading or HDR is unavailable
    /// (`None` while unknown, which blocks nothing).
    pub decode: Option<crate::decode_report::DecodeReport>,
    /// Settings › Logs › support bundle, or `None` on a Deck without one.
    pub support_bundle: Option<super::support_bundle::SupportBundleView>,
    /// The selected saved connection's own Displays choice, which wins over
    /// Settings › Displays for it: its name and the mode.
    pub connection_displays: Option<(String, DisplaysMode)>,
    /// Whether this Deck can hold a canvas (its presenter has a canvas
    /// executor, `crate::canvas::CanvasRequest::for_sink`): the Canvas
    /// screen updates switch is shown only then.
    pub canvas_available: bool,
    /// Whether this Deck may take the system shortcuts with an event tap
    /// (macOS: the Accessibility permission); `None` on a Deck that needs
    /// no permission for it.
    pub system_shortcut_permission: Option<bool>,
}

/// What the user asked for on the Settings screen, besides editing values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsAction {
    /// "Done": back to Home.
    Close,
    /// Logs › Show in folder.
    ShowLogFolder,
    /// Displays › Refresh.
    RefreshDisplays,
    /// Displays › "Use this setting for …": the selected saved connection
    /// drops its own Displays choice and follows Settings.
    FollowDisplaysSetting,
    /// Streaming › Colour check….
    OpenColourCheck,
    /// The Native tablet component's button.
    NativeTabletComponent,
    /// Logs › support bundle.
    SupportBundle(super::support_bundle::SupportBundleAction),
    /// Input › Keyboard › "Open System Settings…": ask the system for the
    /// permission the system-shortcut capture needs.
    AllowSystemShortcutPermission,
}

/// Draws the Settings screen. Edits `values` in place and returns what the
/// user asked for, in the order asked.
pub fn render_settings(
    ui: &mut egui::Ui,
    values: &mut SettingsValues,
    facts: &SettingsFacts<'_>,
) -> Vec<SettingsAction> {
    let platform = deck_platform(ui.ctx());
    let mut actions = Vec::new();
    // Work on copies so the nested egui closures don't fight the borrow
    // checker; write back once at the end.
    let SettingsValues {
        mut tab,
        mut log_level,
        mut security,
        mut performance,
        mut color_fidelity,
        mut displays,
        mut hidpi,
        mut use_notch_area,
        mut remote_ui_scale_percent,
        mut full_color_display_mask,
        mut hidpi_display_mask,
        cursor_preference,
        mut typing_mode,
        option_key_mode,
        mut paste_settings,
        mut clipboard_enabled,
        mut microphone_enabled,
        mut tablet_mode_requested,
        mut pen_buttons,
        mut exact_still_image,
        mut canvas,
        mut keyboard_capture,
        mut capture_mac_system_shortcuts,
        mut keyboard_switch,
    } = values.clone();
    let initial_streaming_preset = StreamingPreset::from_settings(performance, color_fidelity);
    let mut streaming_preset = initial_streaming_preset;
    let hdr_screen = facts.hdr_screen;
    let fidelity =
        super::fidelity::fidelity_availability(platform, hdr_screen, facts.decode.as_ref());
    // A preset this decoder cannot decode is shown as what a connection asks
    // for instead (`decodable_preset`); HDR without an HDR screen as Grading.
    let decodable = facts.decode.as_ref().map_or(streaming_preset, |report| {
        crate::decode_report::decodable_preset(&report.capabilities, streaming_preset)
    });
    if decodable != streaming_preset {
        streaming_preset = decodable;
    } else if fidelity.hdr.is_some() && streaming_preset == StreamingPreset::Hdr {
        streaming_preset = StreamingPreset::Grading;
    }
    // What the panel shows when it opens: an unavailable preset is shown as
    // the one a session would fall back to, but only a click saves.
    let shown_streaming_preset = streaming_preset;

    // The same orange bar as Home, so Settings reads as the same app: Help,
    // and Done where Home has the gear. Settings persist live on change,
    // so there is no Save.
    match super::launcher::render_settings_top_bar(ui) {
        Some(super::launcher::SettingsBarAction::Done) => actions.push(SettingsAction::Close),
        Some(super::launcher::SettingsBarAction::Help) => {
            super::launcher::open_user_guide(ui.ctx());
        }
        None => {}
    }
    ui.add_space(theme::SPACE_SCALE[4]);

    let body_height = ui.available_height();
    ui.horizontal_top(|ui| {
        ui.set_min_height(body_height);
        // Sidebar — white column, active item highlighted (reference look).
        // Wrapped in an explicit vertical: a row/Frame inherits the parent's
        // horizontal layout and would otherwise stack items left-to-right.
        ui.add_space(22.0);
        ui.vertical(|ui| {
            ui.set_width(210.0);
            ui.add_space(2.0);
            egui::ScrollArea::vertical()
                .id_salt("settings_nav_scroll")
                .max_height(body_height)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    settings_nav_section_label(ui, "General");
                    for item in [
                        SettingsTab::Language,
                        SettingsTab::Streaming,
                        SettingsTab::Displays,
                    ] {
                        if settings_nav_item(ui, item.label(), item == tab) {
                            tab = item;
                        }
                        ui.add_space(2.0);
                    }

                    ui.add_space(8.0);
                    settings_nav_section_label(ui, "Input and devices");
                    for item in [
                        SettingsTab::Microphone,
                        SettingsTab::Input,
                        SettingsTab::Tablet,
                    ] {
                        if settings_nav_item(ui, item.label(), item == tab) {
                            tab = item;
                        }
                        ui.add_space(2.0);
                    }

                    ui.add_space(8.0);
                    settings_nav_section_label(ui, "System");
                    for item in [SettingsTab::Logs, SettingsTab::Advanced] {
                        if settings_nav_item(ui, item.label(), item == tab) {
                            tab = item;
                        }
                        ui.add_space(2.0);
                    }
                });
        });
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(44.0);
        // The page: one column, at most `PAGE_COLUMN_MAX` wide, centred
        // in what the sections leave; text wraps at its edge.
        let room = ui.available_width();
        let column = (room - 2.0 * 24.0).clamp(0.0, PAGE_COLUMN_MAX);
        ui.add_space(((room - column) / 2.0 - 44.0).max(0.0));
        ui.vertical(|ui| {
            ui.set_width(column);
            egui::ScrollArea::vertical()
                .id_salt("settings_content_scroll")
                .max_height(body_height)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    // Bump the body text size for the whole content column so the
                    // descriptions read at reference proportions without touching
                    // every label individually.
                    ui.style_mut()
                        .text_styles
                        .insert(egui::TextStyle::Body, egui::FontId::proportional(16.0));
                    ui.add_space(16.0);
                    match tab {
                        SettingsTab::Language => {
                            page_title(ui, "Language");
                            ui.label(
                                egui::RichText::new(
                                    "Select the preferred language for your Arcen client.",
                                )
                                .color(theme::colours(ui).text2),
                            );
                            ui.add_space(theme::SPACE_SCALE[4]);
                            let mut language = ();
                            let _ = list_picker(
                                ui,
                                "language",
                                Some("Language"),
                                &mut language,
                                &[((), "English (US)".to_owned())],
                                None,
                                picker_width(ui),
                            );
                        }
                        SettingsTab::Streaming => {
                            page_title(ui, "Streaming");
                            ui.label(
                            egui::RichText::new(
                                "Choose the result you need. Each preset fixes frame rate, colour \
                                 fidelity, and encoder intent together so the request cannot \
                                 become an accidental combination of unrelated switches.",
                            )
                            .color(theme::colours(ui).text2),
                        );
                            ui.add_space(theme::SPACE_SCALE[4]);
                            // The presets this Deck can serve, in a picker; the
                            // others under it, each with why not.
                            let mut available = Vec::new();
                            let mut unavailable = Vec::new();
                            for preset in StreamingPreset::PRIMARY {
                                let why_not = match preset {
                                    StreamingPreset::Grading => fidelity.grading.as_deref(),
                                    StreamingPreset::Hdr => fidelity.hdr.as_deref(),
                                    _ => None,
                                };
                                match why_not {
                                    Some(reason) => unavailable.push((preset, reason)),
                                    None => available.push(preset),
                                }
                            }
                            let rows: Vec<Choice<StreamingPreset>> = available
                                .iter()
                                .map(|preset| Choice {
                                    value: *preset,
                                    title: preset.title().to_owned(),
                                    badge: (*preset == StreamingPreset::Auto)
                                        .then_some("(Default)"),
                                    line: preset.description().to_owned(),
                                })
                                .collect();
                            choice_picker(
                                ui,
                                "streaming_preset",
                                Some("Preset"),
                                &mut streaming_preset,
                                &rows,
                            );
                            for (preset, reason) in unavailable {
                                ui.add_space(6.0);
                                quiet(ui, &format!("{}: {reason}", preset.title()));
                            }
                            if let Some(codecs) = fidelity.codecs.as_deref() {
                                ui.add_space(4.0);
                                ui.label(
                                    egui::RichText::new(codecs).color(theme::colours(ui).text2),
                                );
                            }
                            if streaming_preset != shown_streaming_preset {
                                streaming_preset.apply_to(&mut performance, &mut color_fidelity);
                            }
                            if initial_streaming_preset == StreamingPreset::Custom
                                && streaming_preset == StreamingPreset::Custom
                            {
                                ui.label(
                                egui::RichText::new(
                                    "A developer-defined configuration is active. Choose one of \
                                     the four presets above to return to the supported product \
                                     surface.",
                                )
                                .color(theme::colours(ui).error),
                            );
                            }
                            ui.add_space(theme::GROUP_GAP);
                            ui.checkbox(&mut exact_still_image, "Exact still image");
                            checkbox_help(ui, EXACT_STILL_IMAGE_DESCRIPTION, Ink::Quiet);
                            if facts.canvas_available {
                                ui.add_space(theme::FIELD_TO_LABEL);
                                ui.checkbox(&mut canvas, "Canvas screen updates");
                                checkbox_help(ui, CANVAS_DESCRIPTION, Ink::Quiet);
                            }
                            ui.add_space(theme::GROUP_GAP);
                            if button(ui, "Colour check\u{2026}").clicked() {
                                actions.push(SettingsAction::OpenColourCheck);
                            }
                            help_line(
                                ui,
                                &format!(
                                    "What the host serves, what {} shows, and whether the \
                                     code values arrive exactly.",
                                    platform.this_machine()
                                ),
                                Ink::Quiet,
                            );
                        }
                        SettingsTab::Displays => {
                            page_title(ui, "Displays");
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    egui::RichText::new(choices::DISPLAYS_INTRO)
                                        .color(theme::colours(ui).text2),
                                );
                                learn_more(ui, choices::DISPLAYS_GUIDE_SECTION);
                            });
                            ui.add_space(theme::SPACE_SCALE[4]);
                            let rows: Vec<Choice<DisplaysMode>> = DisplaysMode::ALL
                                .iter()
                                .map(|mode| Choice {
                                    value: *mode,
                                    title: mode.label().to_owned(),
                                    badge: None,
                                    line: choices::displays_line(*mode, platform).to_owned(),
                                })
                                .collect();
                            choice_picker(
                                ui,
                                "displays_mode",
                                Some("Displays"),
                                &mut displays,
                                &rows,
                            );
                            if let Some((connection, mode)) = &facts.connection_displays {
                                help_line(
                                    ui,
                                    &crate::connect_plan::connection_displays_note(
                                        connection, *mode,
                                    ),
                                    Ink::Note,
                                );
                                ui.add_space(theme::SPACE_SCALE[1]);
                                if button(
                                    ui,
                                    &crate::connect_plan::connection_displays_follow_label(
                                        connection,
                                    ),
                                )
                                .clicked()
                                {
                                    actions.push(SettingsAction::FollowDisplaysSetting);
                                }
                            }

                            // A screen with no notch at all (the Windows Deck)
                            // has no notch section (PARITY Q-7).
                            if let Some(has_notch) = facts.notch {
                                group_heading(ui, "Notch area");
                                ui.add_enabled_ui(has_notch, |ui| {
                                    ui.checkbox(
                                        &mut use_notch_area,
                                        "Extend fullscreen under the notch",
                                    );
                                });
                                checkbox_help(
                                    ui,
                                    if has_notch {
                                        "Off: the remote desktop fills the area below the notch, \
                                 1:1 and unscaled, and the notch strip stays black. On: \
                                 Arcen takes the whole panel instead of using macOS \
                                 fullscreen, so the desktop is still 1:1 but extends \
                                 beside and behind the notch — the notch itself covers a \
                                 slice of the remote screen's top edge. Takes effect at \
                                 the next connection."
                                    } else {
                                        "Not applicable: this display has no notch, so fullscreen \
                                 already uses the whole panel."
                                    },
                                    Ink::Quiet,
                                );
                            }

                            group_heading(ui, "Remote UI scale");
                            let mut remote_ui_scale =
                                RemoteUiScale::from_percent(remote_ui_scale_percent);
                            let scales: Vec<(RemoteUiScale, String)> = RemoteUiScale::ALL
                                .iter()
                                .map(|scale| (*scale, scale.label().clone()))
                                .collect();
                            let _ = list_picker(
                                ui,
                                "remote_ui_scale",
                                None,
                                &mut remote_ui_scale,
                                &scales,
                                None,
                                picker_width(ui),
                            );
                            remote_ui_scale_percent = remote_ui_scale.as_percent();
                            help_line(
                                ui,
                                &format!(
                                    "How large the remote desktop's own interface is, on hosts \
                                 that take their scaling from the display Arcen advertises \
                                 — that is Windows. Automatic reports this panel's real \
                                 size, so Windows picks the scaling matching its physical \
                                 density, which on a {} is large; a lower \
                                 percentage trades apparent size for desktop space. This \
                                 changes the remote desktop's layout, never the stream's \
                                 1:1 pixel mapping. Takes effect at the next connection.",
                                    words::dense_panel(platform)
                                ),
                                Ink::Quiet,
                            );
                            help_line(
                                ui,
                                "Linux hosts ignore this. A GNOME session on Xorg offers \
                                 only 100% and 200%, and chooses for itself. To change \
                                 apparent size on a Linux host, use HiDPI streaming below: \
                                 with it off the remote interface is roughly twice the \
                                 size it is with it on.",
                                Ink::Note,
                            );

                            group_heading(ui, words::hidpi_heading(platform));
                            ui.checkbox(&mut hidpi, words::hidpi_checkbox(platform));
                            checkbox_help(ui, words::hidpi_description(platform), Ink::Quiet);

                            group_heading(ui, "Per-display color priority");
                            ui.label(
                            egui::RichText::new(
                                "Advanced: require full-color 4:4:4 on selected display numbers. \
                                 When at least one is selected, unselected displays may use \
                                 host-approved 4:2:0 hardware or software fallback. Pier still \
                                 decides which configured GPU/encoder resources may be used and \
                                 rejects the whole layout if it cannot satisfy every requirement.",
                            )
                            .color(theme::colours(ui).text2),
                        );
                            group_heading(ui, "Detected displays");
                            if button(ui, "Refresh").clicked() {
                                actions.push(SettingsAction::RefreshDisplays);
                            }
                            ui.add_space(theme::SPACE_SCALE[2]);
                            if facts.monitors.is_empty() {
                                ui.label(
                                    egui::RichText::new(
                                        "No displays detected on this platform. The host will fall \
                                     back to its configured default size.",
                                    )
                                    .color(theme::colours(ui).text2),
                                );
                            } else {
                                for (idx, row) in facts.monitors.iter().enumerate() {
                                    let mon = &row.monitor;
                                    let name = if mon.name.is_empty() {
                                        format!("Display {}", idx + 1)
                                    } else {
                                        mon.name.clone()
                                    };
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            egui::RichText::new(format!("{}", idx + 1))
                                                .strong()
                                                .color(theme::colours(ui).text2),
                                        );
                                        ui.label(
                                            egui::RichText::new(&name)
                                                .color(theme::colours(ui).text)
                                                .size(16.0),
                                        );
                                        if mon.is_primary {
                                            ui.label(
                                                egui::RichText::new("Primary")
                                                    .color(theme::colours(ui).text2)
                                                    .size(14.0),
                                            );
                                        }
                                        if idx < 4 {
                                            let bit = 1u8 << idx;
                                            let mut full_color = full_color_display_mask & bit != 0;
                                            if ui
                                                .checkbox(&mut full_color, "Require 4:4:4")
                                                .changed()
                                            {
                                                if full_color {
                                                    full_color_display_mask |= bit;
                                                } else {
                                                    full_color_display_mask &= !bit;
                                                }
                                            }
                                            // Only a Retina/scaled panel has
                                            // backing pixels to stream; offering
                                            // this on a 1x display would promise
                                            // a sharpness it cannot deliver
                                            // while quadrupling nothing.
                                            if mon.scale > 1.0 {
                                                let mut hidpi = hidpi_display_mask & bit != 0;
                                                if ui
                                                    .checkbox(&mut hidpi, "Full resolution")
                                                    .on_hover_text(
                                                        "Streams this display's full backing \
                                                     resolution so the remote desktop applies \
                                                     its own scaling. Sharper and correctly \
                                                     sized, at roughly four times the pixels.",
                                                    )
                                                    .changed()
                                                {
                                                    if hidpi {
                                                        hidpi_display_mask |= bit;
                                                    } else {
                                                        hidpi_display_mask &= !bit;
                                                    }
                                                }
                                            }
                                        }
                                    });
                                    let scale = if (mon.scale - mon.scale.round()).abs() < 0.05 {
                                        format!("{:.0}x", mon.scale.round())
                                    } else {
                                        format!("{:.2}x", mon.scale)
                                    };
                                    ui.indent(("mon", idx), |ui| {
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "{} × {} px, scale {}, {} Hz, at ({}, {})",
                                                mon.width_px,
                                                mon.height_px,
                                                scale,
                                                mon.refresh_hz,
                                                mon.x,
                                                mon.y,
                                            ))
                                            .color(theme::colours(ui).text2),
                                        );
                                        // Fullscreen presentation size: what a
                                        // pinned session actually streams. Shown
                                        // only when it differs, i.e. on a notched
                                        // panel, so the number is never a mystery.
                                        if let Some([pw, ph]) = row
                                            .fullscreen_size(hidpi)
                                            .filter(|size| *size != [mon.width_px, mon.height_px])
                                        {
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "streams {pw} × {ph} px in fullscreen{}",
                                                    words::fullscreen_size_reason(platform)
                                                ))
                                                .color(theme::colours(ui).text2),
                                            );
                                        }
                                    });
                                    ui.add_space(6.0);
                                }
                            }
                        }
                        SettingsTab::Microphone => {
                            page_title(ui, "Microphone");
                            ui.checkbox(
                                &mut microphone_enabled,
                                "Allow microphone forwarding for this launch",
                            );
                            checkbox_help(ui, &choices::microphone_line(platform), Ink::Quiet);
                        }
                        SettingsTab::Input => {
                            page_title(ui, "Input");
                            let [intro, switch_line] =
                                choices::input_intro(platform, keyboard_switch);
                            quiet(ui, &intro);
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    egui::RichText::new(switch_line)
                                        .color(theme::colours(ui).text2),
                                );
                                learn_more(ui, choices::KEYBOARD_GUIDE_SECTION);
                            });

                            group_heading(ui, choices::KEYBOARD_HEADING);
                            let mut on = system_shortcuts_to_host(
                                platform,
                                keyboard_capture,
                                capture_mac_system_shortcuts,
                            );
                            if ui
                                .checkbox(&mut on, choices::SYSTEM_SHORTCUTS_TITLE)
                                .changed()
                            {
                                set_system_shortcuts_to_host(
                                    platform,
                                    on,
                                    &mut keyboard_capture,
                                    &mut capture_mac_system_shortcuts,
                                );
                            }
                            checkbox_help(ui, choices::system_shortcuts_line(platform), Ink::Quiet);
                            // What a connection without its own value uses now.
                            let app = crate::overrides::AppDefaults {
                                typing_mode: crate::overrides::stored_typing_mode(typing_mode),
                                option_keys: option_key_mode,
                                displays_mode: displays,
                                clipboard_enabled,
                                system_shortcuts: on,
                            };
                            if let Some(line) =
                                choices::use_their_own_line(crate::overrides::connections_with_own(
                                    facts.connections,
                                    app,
                                    crate::overrides::OverridableSetting::SystemShortcuts,
                                ))
                            {
                                checkbox_help(ui, &line, Ink::Quiet);
                            }
                            // The permission is one for the whole app: it is
                            // needed as soon as any connection sends them.
                            let any_on = on
                                || facts.connections.iter().any(|connection| {
                                    connection
                                        .settings
                                        .overrides
                                        .effective_system_shortcuts(on)
                                        .value
                                });
                            if let Some(needed) = choices::system_shortcuts_permission(
                                any_on,
                                facts.system_shortcut_permission,
                            ) {
                                checkbox_help(ui, needed, Ink::Note);
                                ui.horizontal(|ui| {
                                    ui.add_space(checkbox_indent(ui));
                                    if button(ui, choices::OPEN_SYSTEM_SETTINGS).clicked() {
                                        actions.push(SettingsAction::AllowSystemShortcutPermission);
                                    }
                                });
                            }

                            group_heading(ui, choices::TYPING_HEADING);
                            quiet(
                                ui,
                                &choices::default_for_all_connections(
                                    crate::overrides::connections_with_own(
                                        facts.connections,
                                        app,
                                        crate::overrides::OverridableSetting::TypingMode,
                                    ),
                                ),
                            );
                            ui.add_space(8.0);
                            let stored = crate::overrides::stored_typing_mode(typing_mode);
                            let mut typing = stored;
                            choice_picker(
                                ui,
                                "typing_mode",
                                None,
                                &mut typing,
                                &choices::typing_choices(),
                            );
                            if typing != stored {
                                typing_mode = typing;
                            }

                            group_heading(ui, choices::CLIPBOARD_HEADING);
                            ui.checkbox(&mut clipboard_enabled, choices::CLIPBOARD_TITLE);
                            if let Some(line) =
                                choices::clipboard_line(crate::overrides::connections_with_own(
                                    facts.connections,
                                    crate::overrides::AppDefaults {
                                        clipboard_enabled,
                                        ..app
                                    },
                                    crate::overrides::OverridableSetting::ClipboardEnabled,
                                ))
                            {
                                checkbox_help(ui, &line, Ink::Quiet);
                            }
                            if let Some(notice) = facts.host_clipboard_notice {
                                checkbox_help(ui, notice, Ink::Note);
                            }
                        }
                        SettingsTab::Tablet => {
                            page_title(ui, "Tablet");
                            let mut rows = choices::tablet_choices().to_vec();
                            if facts.native_tablet.is_none() {
                                for row in &mut rows {
                                    if row.value == TabletModeMsg::UsbTabletBridge {
                                        row.badge = Some(choices::NATIVE_TABLET_NOT_IN_BUILD);
                                    }
                                }
                            }
                            choice_picker(
                                ui,
                                "tablet_mode",
                                None,
                                &mut tablet_mode_requested,
                                &rows,
                            );
                            match tablet_mode_requested {
                                TabletModeMsg::UsbTabletBridge => {
                                    if let Some(control) = facts.native_tablet.as_ref() {
                                        native_tablet_control(ui, control, &mut actions);
                                    }
                                }
                                TabletModeMsg::LocalTermination => {
                                    pen_button_rows(ui, platform, &mut pen_buttons);
                                }
                                TabletModeMsg::DisabledMouseCompat => {}
                            }
                            ui.add_space(8.0);
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    egui::RichText::new(choices::TABLET_FOOTER)
                                        .color(theme::colours(ui).text2),
                                );
                                learn_more(ui, choices::TABLET_GUIDE_SECTION);
                            });
                            ui.add_space(24.0);
                        }
                        SettingsTab::Logs => {
                            page_title(ui, "Logs");
                            quiet(
                                ui,
                                "Set the level of detail captured by the logs. Level 0 (Critical) \
                                 provides the least information and Level 3 (Debug) provides the \
                                 most information.",
                            );
                            ui.add_space(theme::SPACE_SCALE[4]);
                            let levels: Vec<(LogLevel, String)> = LogLevel::ALL
                                .iter()
                                .map(|level| (*level, level.label().to_string()))
                                .collect();
                            let _ = list_picker(
                                ui,
                                "log_level",
                                Some("Log level"),
                                &mut log_level,
                                &levels,
                                None,
                                picker_width(ui),
                            );
                            ui.add_space(theme::GROUP_GAP);
                            // The log's two actions side by side, one width.
                            let labels = ["Show in folder", "Create support bundle\u{2026}"];
                            let width = super::widgets::button_width(ui, &labels);
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = theme::BUTTON_GAP;
                                if super::widgets::secondary_button(ui, labels[0], width).clicked()
                                {
                                    actions.push(SettingsAction::ShowLogFolder);
                                }
                                if let Some(bundle) = &facts.support_bundle {
                                    let open = ui
                                        .add_enabled_ui(!bundle.open, |ui| {
                                            super::widgets::secondary_button(ui, labels[1], width)
                                        })
                                        .inner;
                                    if open.clicked() {
                                        actions.push(SettingsAction::SupportBundle(
                                            super::support_bundle::SupportBundleAction::Open,
                                        ));
                                    }
                                }
                            });
                            if let Some(bundle) = &facts.support_bundle {
                                ui.add_space(theme::SPACE_SCALE[3]);
                                actions.extend(
                                    super::support_bundle::render_support_bundle_panel(
                                        ui, bundle, platform,
                                    )
                                    .into_iter()
                                    .map(SettingsAction::SupportBundle),
                                );
                            }
                        }
                        SettingsTab::Advanced => {
                            ui.label(
                                egui::RichText::new("Advanced")
                                    .color(theme::colours(ui).text)
                                    .size(PAGE_TITLE),
                            );
                            group_heading(ui, choices::CERTIFICATE_HEADING);
                            quiet(ui, choices::CERTIFICATE_INTRO);
                            ui.add_space(12.0);
                            // Low only where this Deck may skip the checks; a saved
                            // Low that cannot is shown as the Medium it checks
                            // as, and only a click saves.
                            let low_allowed = crate::settings::low_security_allowed();
                            let shown = security.in_force_with(low_allowed);
                            let mut chosen = shown;
                            choice_picker(
                                ui,
                                "certificate_checking",
                                None,
                                &mut chosen,
                                &choices::certificate_choices(low_allowed),
                            );
                            if chosen != shown {
                                security = chosen;
                            }
                            if security != security.in_force_with(low_allowed) {
                                help_line(ui, choices::LOW_SECURITY_OFF_NOTE, Ink::Note);
                            }

                            group_heading(ui, choices::ADVANCED_INPUT_HEADING);
                            keyboard_switch_rows(ui, platform, &mut keyboard_switch);
                            help_line(
                                ui,
                                "Help ▸ Keyboard Shortcuts lists every key Arcen Deck keeps and \
                                 the keys Connection ▸ Send Key sends.",
                                Ink::Quiet,
                            );
                            sub_heading(ui, choices::PASTE_HEADING);
                            let rates: Vec<(u32, String)> = arcen_input::text::paste::RATE_CHOICES
                                .iter()
                                .map(|rate| (*rate, choices::paste_speed_choice(*rate)))
                                .collect();
                            let _ = list_picker(
                                ui,
                                "paste_chars_per_second",
                                Some(choices::PASTE_SPEED),
                                &mut paste_settings.chars_per_second,
                                &rates,
                                None,
                                picker_width(ui),
                            );
                            ui.add_space(theme::FIELD_TO_LABEL);
                            let limits: Vec<(usize, String)> =
                                arcen_input::text::paste::CONFIRM_CHOICES
                                    .iter()
                                    .map(|chars| (*chars, choices::paste_ask_choice(*chars)))
                                    .collect();
                            let _ = list_picker(
                                ui,
                                "paste_confirm_above",
                                Some(choices::PASTE_ASK_ABOVE),
                                &mut paste_settings.confirm_above,
                                &limits,
                                None,
                                picker_width(ui),
                            );
                            if cfg!(feature = "dev-tools") {
                                // Developer builds only: the same controls as
                                // every page, the list pickers.
                                group_heading(ui, "Streaming overrides (developer build)");
                                let width = picker_width(ui);
                                let mut ceiling = match performance {
                                    PerformanceMode::Standard => PerformanceMode::Standard,
                                    PerformanceMode::High | PerformanceMode::HighLegacy => {
                                        PerformanceMode::High
                                    }
                                };
                                let ceilings = [
                                    (PerformanceMode::Standard, "30 fps".to_owned()),
                                    (PerformanceMode::High, "60 fps".to_owned()),
                                ];
                                if list_picker(
                                    ui,
                                    "streaming_performance_override",
                                    Some("Frame-rate ceiling"),
                                    &mut ceiling,
                                    &ceilings,
                                    None,
                                    width,
                                )
                                .changed
                                {
                                    performance = ceiling;
                                }
                                ui.add_space(theme::FIELD_TO_LABEL);
                                let presets: Vec<(ColorFidelity, String)> = [
                                    ColorFidelity::Standard,
                                    ColorFidelity::FullColour,
                                    ColorFidelity::GradingReference,
                                    ColorFidelity::Hdr10,
                                ]
                                .iter()
                                .map(|preset| (*preset, preset.title().to_owned()))
                                .collect();
                                let described = color_fidelity.preset.describe();
                                let _ = list_picker(
                                    ui,
                                    "color_fidelity_preset_override",
                                    Some("Base colour contract"),
                                    &mut color_fidelity.preset,
                                    &presets,
                                    Some(described),
                                    width,
                                );
                                ui.add_space(theme::SPACE_SCALE[3]);
                                quiet(
                                    ui,
                                    "Pin individual axes on top of the preset above. Each \
                                     defaults to \"Follow preset\", which is exactly the \
                                     preset's own value, so it stays invisible until you \
                                     explicitly pin it, and a later preset change never \
                                     discards an axis you pinned here.",
                                );
                                let overrides = &mut color_fidelity.overrides;
                                ui.add_space(theme::FIELD_TO_LABEL);
                                let _ = list_picker(
                                    ui,
                                    "color_fidelity_codec_override",
                                    Some("Codec"),
                                    &mut overrides.codec,
                                    &pinned(arcen_media::VideoCodec::ALL, |codec| {
                                        color_fidelity_codec_label(codec).to_string()
                                    }),
                                    None,
                                    width,
                                );
                                ui.add_space(theme::FIELD_TO_LABEL);
                                let _ = list_picker(
                                    ui,
                                    "color_fidelity_chroma_override",
                                    Some("Chroma"),
                                    &mut overrides.chroma,
                                    &pinned(arcen_media::ChromaSubsampling::ALL, |chroma| {
                                        color_fidelity_chroma_label(chroma).to_string()
                                    }),
                                    None,
                                    width,
                                );
                                ui.add_space(theme::FIELD_TO_LABEL);
                                let _ = list_picker(
                                    ui,
                                    "color_fidelity_bit_depth_override",
                                    Some("Bit depth"),
                                    &mut overrides.bit_depth,
                                    &pinned(
                                        arcen_media::BitDepth::ALL,
                                        color_fidelity_bit_depth_label,
                                    ),
                                    None,
                                    width,
                                );
                                ui.add_space(theme::FIELD_TO_LABEL);
                                let _ = list_picker(
                                    ui,
                                    "color_fidelity_range_override",
                                    Some("Colour range"),
                                    &mut overrides.range,
                                    &pinned(arcen_media::ColorRange::ALL, |range| {
                                        color_fidelity_range_label(range).to_string()
                                    }),
                                    None,
                                    width,
                                );
                                ui.add_space(theme::FIELD_TO_LABEL);
                                let _ = list_picker(
                                    ui,
                                    "color_fidelity_matrix_override",
                                    Some("Matrix"),
                                    &mut overrides.matrix,
                                    &pinned(arcen_media::ColorMatrix::ALL, |matrix| {
                                        color_fidelity_matrix_label(matrix).to_string()
                                    }),
                                    None,
                                    width,
                                );
                                sub_heading(ui, "Variant (testing)");
                                quiet(
                                    ui,
                                    "Pin an exact probe-matrix row, bypassing the preset and \
                                     the overrides above entirely. This is how the w0 testing \
                                     machine drives the colour-variation matrix directly. Leave \
                                     at None for ordinary use.",
                                );
                                ui.add_space(theme::FIELD_TO_LABEL);
                                let variants: Vec<_> = std::iter::once((None, "None".to_owned()))
                                    .chain(arcen_media::video::PROBE_MATRIX.iter().map(|variant| {
                                        (
                                            Some(*variant),
                                            format!("{} ({})", variant.describe(), variant.id()),
                                        )
                                    }))
                                    .collect();
                                let _ = list_picker(
                                    ui,
                                    "color_fidelity_variant_override",
                                    None,
                                    &mut color_fidelity.variant_override,
                                    &variants,
                                    None,
                                    width,
                                );
                                ui.add_space(theme::SPACE_SCALE[3]);
                                let color_fidelity_resolved = color_fidelity.resolved_variant();
                                let color_fidelity_effective =
                                    effective_color_fidelity_variant(color_fidelity);
                                ui.label(
                                    egui::RichText::new(format!(
                                        "Resolving to: {}",
                                        color_fidelity_effective.describe()
                                    ))
                                    .color(theme::colours(ui).text)
                                    .strong(),
                                );
                                if !color_fidelity_resolved.is_coherent() {
                                    ui.add_space(6.0);
                                    ui.label(
                                egui::RichText::new(format!(
                                    "This exact combination ({}) is not offered. Falling back \
                                     to the {} preset when you connect.",
                                    color_fidelity_resolved.describe(),
                                    color_fidelity.preset.title(),
                                ))
                                    .color(theme::colours(ui).error),
                                );
                                }
                            }
                        }
                    }
                });
        });
    });

    *values = SettingsValues {
        tab,
        log_level,
        security,
        performance,
        color_fidelity,
        displays,
        hidpi,
        use_notch_area,
        remote_ui_scale_percent,
        full_color_display_mask,
        hidpi_display_mask,
        cursor_preference,
        typing_mode,
        option_key_mode,
        paste_settings,
        clipboard_enabled,
        microphone_enabled,
        tablet_mode_requested,
        pen_buttons,
        exact_still_image,
        canvas,
        keyboard_capture,
        capture_mac_system_shortcuts,
        keyboard_switch,
    };
    actions
}

/// Settings ▸ Advanced ▸ Input ▸ Keyboard switch: the presets and a custom chord,
/// checked in shared code ([`super::keyboard_help::keyboard_switch_verdict`]).
fn keyboard_switch_rows(
    ui: &mut egui::Ui,
    platform: DeckPlatform,
    switch: &mut arcen_input::keyboard::KeyboardSwitchChord,
) {
    use arcen_input::keyboard::KeyboardSwitchPreset;
    let draft_id = egui::Id::new("arcen-keyboard-switch-draft");
    sub_heading(ui, choices::KEYBOARD_SWITCH_HEADING);
    ui.label(
        egui::RichText::new(super::keyboard_help::keyboard_switch_help(
            platform, *switch,
        ))
        .color(theme::colours(ui).text2),
    );
    ui.add_space(6.0);
    let mut preset = switch.preset();
    let mut custom_draft: Option<String> = ui.ctx().data(|data| data.get_temp::<String>(draft_id));
    if custom_draft.is_some() {
        preset = KeyboardSwitchPreset::Custom;
    }
    let presets: Vec<(KeyboardSwitchPreset, String)> = KeyboardSwitchPreset::ALL
        .iter()
        .map(|choice| {
            (
                *choice,
                super::keyboard_help::keyboard_switch_preset_label(platform, *choice),
            )
        })
        .collect();
    let mut chosen = preset;
    let note = preset.note();
    let picked = list_picker(
        ui,
        "keyboard_switch_preset",
        None,
        &mut chosen,
        &presets,
        note,
        picker_width(ui),
    );
    if picked.changed && chosen != preset {
        match chosen.chord() {
            Some(chord) => {
                *switch = chord;
                custom_draft = None;
            }
            None => custom_draft = Some(switch_chord_draft(platform, *switch)),
        }
        preset = chosen;
    }
    if preset == KeyboardSwitchPreset::Custom {
        let mut text = custom_draft.unwrap_or_else(|| switch_chord_draft(platform, *switch));
        ui.add_space(theme::FIELD_TO_LABEL);
        super::widgets::field_label(ui, "Chord");
        let committed =
            super::widgets::text_field(ui, &mut text, false, picker_width(ui), "Ctrl+Alt+Shift+K")
                .lost_focus();
        // A chord is used only once typed out (Enter or leaving the field):
        // the chords a half-typed one passes through are never applied.
        let current = super::keyboard_help::switch_chord_text(platform, *switch);
        match super::keyboard_help::keyboard_switch_verdict(&text) {
            Ok((chord, warning)) => {
                if committed {
                    *switch = chord;
                }
                let note = if chord == *switch {
                    warning
                } else {
                    Some(format!(
                        "Press Enter to use it (now {current}).{}",
                        warning.map_or_else(String::new, |warning| format!(" {warning}"))
                    ))
                };
                if let Some(note) = note {
                    help_line(ui, &note, Ink::Note);
                }
            }
            Err(reason) => {
                help_line(ui, &format!("{reason} Keeping {current}."), Ink::Error);
            }
        }
        ui.ctx().data_mut(|data| data.insert_temp(draft_id, text));
    } else {
        ui.ctx().data_mut(|data| data.remove::<String>(draft_id));
    }
}

fn switch_chord_draft(
    platform: DeckPlatform,
    switch: arcen_input::keyboard::KeyboardSwitchChord,
) -> String {
    match platform {
        DeckPlatform::Mac => switch.mac_text().replace('-', "+"),
        DeckPlatform::Pc => switch.pc_text(),
    }
}

/// The pen-button rows under Tablet support: the buttons this Deck's
/// operating system reports (Windows Ink's one barrel button, or a Mac pen's
/// lower and upper side switch), and what each clicks on the host.
fn pen_button_rows(ui: &mut egui::Ui, platform: DeckPlatform, buttons: &mut PenButtonSettings) {
    let rows: Vec<(&str, &str, &mut PenButtonAction)> = match platform {
        DeckPlatform::Pc => vec![("pen_barrel_button", "Pen button", &mut buttons.barrel)],
        DeckPlatform::Mac => vec![
            ("pen_lower_button", "Lower pen button", &mut buttons.lower),
            ("pen_upper_button", "Upper pen button", &mut buttons.upper),
        ],
    };
    let actions: Vec<(PenButtonAction, String)> = PenButtonAction::ALL
        .iter()
        .map(|choice| (*choice, choice.label().to_string()))
        .collect();
    for (id, label, action) in rows {
        ui.add_space(theme::FIELD_TO_LABEL);
        let _ = list_picker(
            ui,
            id,
            Some(label),
            action,
            &actions,
            None,
            picker_width(ui),
        );
    }
    ui.add_space(theme::FIELD_TO_HELP);
    quiet(ui, pen_button_note(platform));
    ui.add_space(12.0);
}

/// Native tablet support's component: its control (the one prerequisite
/// the user can act on from here), what it still needs and its last error.
fn native_tablet_control(
    ui: &mut egui::Ui,
    control: &NativeTabletControl,
    actions: &mut Vec<SettingsAction>,
) {
    ui.add_space(theme::FIELD_TO_LABEL);
    if button(ui, control.action_label.as_str())
        .on_hover_text(control.guidance.as_str())
        .clicked()
    {
        actions.push(SettingsAction::NativeTabletComponent);
    }
    if !control.ready {
        // Waiting on the user (an approval in System Settings) reads as a
        // note; anything else as the button's explanation.
        let ink = match control.tint {
            ComponentTint::Attention => Ink::Note,
            ComponentTint::Ready | ComponentTint::Action => Ink::Quiet,
        };
        help_line(ui, &control.guidance, ink);
    }
    if let Some(error) = &control.error {
        help_line(ui, error, Ink::Error);
    }
}

/// "Learn more", linking to a section of the user guide. It never breaks
/// across lines: at the end of a wrapped line it moves to the next one
/// whole.
fn learn_more(ui: &mut egui::Ui, section: &str) {
    ui.scope(|ui| {
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
        ui.hyperlink_to(choices::LEARN_MORE, choices::user_guide_url(section));
    });
}

/// The page column's widest.
const PAGE_COLUMN_MAX: f32 = 560.0;
/// A page's title.
const PAGE_TITLE: f32 = 26.0;
/// A group's heading.
const SECTION_TITLE: f32 = 20.0;

/// How a line under a control reads.
#[derive(Debug, Clone, Copy)]
enum Ink {
    /// An explanation.
    Quiet,
    /// Something the user should know, not an error.
    Note,
    Error,
}

/// A list picker's width on a page.
fn picker_width(ui: &egui::Ui) -> f32 {
    ui.available_width().min(theme::COLUMN_MAX)
}

/// A panel's title.
fn page_title(ui: &mut egui::Ui, title: &str) {
    ui.label(
        egui::RichText::new(title)
            .color(theme::colours(ui).text)
            .size(PAGE_TITLE),
    );
    ui.add_space(theme::SPACE_SCALE[3]);
}

/// One heading per group of choices.
fn group_heading(ui: &mut egui::Ui, heading: &str) {
    ui.add_space(theme::GROUP_GAP);
    ui.label(
        egui::RichText::new(heading)
            .color(theme::colours(ui).text)
            .size(SECTION_TITLE),
    );
    ui.add_space(theme::SPACE_SCALE[1]);
}

/// A heading inside a group.
fn sub_heading(ui: &mut egui::Ui, heading: &str) {
    ui.add_space(theme::SPACE_SCALE[4]);
    ui.label(
        egui::RichText::new(heading)
            .color(theme::colours(ui).text)
            .size(16.0),
    );
    ui.add_space(theme::SPACE_SCALE[1]);
}

/// A line of explanation, wrapped to the column.
fn quiet(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).color(theme::colours(ui).text2));
}

/// A control's description or note under it: the small, muted help style
/// every control uses (the list picker's own help line included).
fn help_line(ui: &mut egui::Ui, text: &str, ink: Ink) {
    let colours = theme::colours(ui);
    let colour = match ink {
        Ink::Quiet => colours.text2,
        Ink::Note => colours.warning,
        Ink::Error => colours.error,
    };
    ui.add_space(theme::FIELD_TO_HELP);
    ui.add(
        egui::Label::new(
            egui::RichText::new(text)
                .size(theme::TEXT_LABEL)
                .color(colour),
        )
        .wrap(),
    );
}

/// How far a check box's label starts from its box: its description
/// lines up with the label.
fn checkbox_indent(ui: &egui::Ui) -> f32 {
    ui.spacing().icon_width + ui.spacing().icon_spacing
}

/// A check box's description, aligned with its label text.
fn checkbox_help(ui: &mut egui::Ui, text: &str, ink: Ink) {
    let indent = checkbox_indent(ui);
    ui.horizontal(|ui| {
        ui.add_space(indent);
        ui.vertical(|ui| help_line(ui, text, ink));
    });
}

/// A Settings button: the shared secondary button, as wide as its label
/// needs.
fn button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let width = super::widgets::button_width(ui, &[label]);
    super::widgets::secondary_button(ui, label, width)
}

/// A developer override's choices: "Follow preset", then each value.
fn pinned<T: Copy>(all: &[T], label: impl Fn(T) -> String) -> Vec<(Option<T>, String)> {
    std::iter::once((None, "Follow preset".to_owned()))
        .chain(all.iter().map(|value| (Some(*value), label(*value))))
        .collect()
}

/// A group's choices in one list picker: each choice's title (and badge),
/// the chosen one's line under it.
fn choice_picker<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    id: &str,
    title: Option<&str>,
    current: &mut T,
    rows: &[Choice<T>],
) {
    let labels: Vec<(T, String)> = rows
        .iter()
        .map(|choice| {
            let label = match choice.badge {
                Some(badge) => format!("{} {badge}", choice.title),
                None => choice.title.clone(),
            };
            (choice.value, label)
        })
        .collect();
    let help = rows
        .iter()
        .find(|choice| choice.value == *current)
        .map(|choice| choice.line.clone());
    let _ = list_picker(
        ui,
        id,
        title,
        current,
        &labels,
        help.as_deref(),
        picker_width(ui),
    );
}

/// The note under the pen-button rows.
#[must_use]
pub const fn pen_button_note(platform: DeckPlatform) -> &'static str {
    match platform {
        DeckPlatform::Pc => {
            "Windows Ink reports one pen button: the one your tablet's settings set to Right \
             Click. A pen button your tablet's settings set to a mouse click (Middle Click, \
             Right Click or Left Click) clicks that on the host."
        }
        DeckPlatform::Mac => {
            "A pen button your tablet's settings set to a mouse click (Right Click, Middle \
             Click) clicks that on the host; these choices apply to a button it leaves to the \
             application."
        }
    }
}

/// The few phrases that name the machine, its operating system or its keys.
mod words {
    use super::DeckPlatform;

    pub(super) const fn dense_panel(platform: DeckPlatform) -> &'static str {
        match platform {
            DeckPlatform::Mac => "Retina panel",
            DeckPlatform::Pc => "high-density display",
        }
    }

    /// Why a display streams less than its size in fullscreen: on a Mac, the
    /// notch strip.
    pub(super) const fn fullscreen_size_reason(platform: DeckPlatform) -> &'static str {
        match platform {
            DeckPlatform::Mac => " (the notch strip is reserved by macOS)",
            DeckPlatform::Pc => "",
        }
    }

    /// PARITY Q-8: "Retina" is a Mac word.
    pub(super) const fn hidpi_heading(platform: DeckPlatform) -> &'static str {
        match platform {
            DeckPlatform::Mac => "HiDPI / Retina streaming",
            DeckPlatform::Pc => "HiDPI streaming",
        }
    }

    pub(super) const fn hidpi_checkbox(platform: DeckPlatform) -> &'static str {
        match platform {
            DeckPlatform::Mac => "HiDPI streaming (Retina)",
            DeckPlatform::Pc => "HiDPI streaming (full resolution)",
        }
    }

    pub(super) const fn hidpi_description(platform: DeckPlatform) -> &'static str {
        match platform {
            DeckPlatform::Mac => {
                "Request the stream at your screen's backing resolution \
                 (e.g. 3024×1890 on a 1512×945 Retina panel) for crisp \
                 1:1 pixel rendering. Increases bandwidth roughly 4×. \
                 Takes effect at the next connection."
            }
            DeckPlatform::Pc => {
                "Request the stream at your screen's full resolution \
                 (e.g. 3840×2160 on a display scaled to 200%) for crisp \
                 1:1 pixel rendering. Increases bandwidth roughly 4×. \
                 Takes effect at the next connection."
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use arcen_input::keyboard::DeckShortcut;

    #[test]
    fn the_mac_spells_its_chords_as_its_menus_do() {
        let mac = |shortcut| super::super::wording::chord(DeckPlatform::Mac, shortcut);
        assert_eq!(mac(DeckShortcut::ReleaseKeyboard), "Control-Option-Home");
        assert_eq!(mac(DeckShortcut::Disconnect), "Control-Option-F12");
        // Menu items without a chord.
        assert_eq!(mac(DeckShortcut::KeyboardCharacters), "");
        assert_eq!(mac(DeckShortcut::PasteAsKeystrokes), "");
    }

    #[test]
    fn the_pc_transliterates_and_keeps_its_own_fullscreen_and_quit() {
        let pc = |shortcut| super::super::wording::chord(DeckPlatform::Pc, shortcut);
        assert_eq!(pc(DeckShortcut::KeyboardCharacters), "");
        assert_eq!(pc(DeckShortcut::PasteAsKeystrokes), "");
        assert_eq!(pc(DeckShortcut::ReleaseKeyboard), "Ctrl+Alt+Home");
        assert_eq!(pc(DeckShortcut::Disconnect), "Ctrl+Alt+F12");
        assert_eq!(pc(DeckShortcut::ToggleFullscreen), "Ctrl+Alt+Break");
        assert_eq!(pc(DeckShortcut::Quit), "Alt+F4");
    }

    #[cfg(feature = "runtime")]
    mod native_tablet {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        use arcen_usb_bridge::NativeUsbComponentState as State;

        use super::super::NativeTabletComponent;
        use crate::DeckServices;
        use crate::services::test_support::UnavailableServices;

        /// A component that is not installed until its action runs.
        #[derive(Default)]
        struct Component {
            reads: AtomicUsize,
            installed: Mutex<bool>,
            fail: bool,
        }

        macro_rules! delegate {
            ($($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)*) => {
                $(fn $name(&self, $($arg: $ty),*) -> $ret { UnavailableServices.$name($($arg),*) })*
            };
        }

        impl DeckServices for Component {
            delegate! {
                build_identity() -> arcen_protocol::messages::BuildIdentityMsg;
                platform_summary() -> String;
                native_config_dir() -> Option<std::path::PathBuf>;
                native_log_dir() -> Option<std::path::PathBuf>;
                decode_capabilities() -> crate::decode::DecodeCapabilities;
                displays() -> Vec<arcen_protocol::messages::ClientMonitor>;
                display_color(id: u32) -> Option<arcen_protocol::messages::DisplayColorMsg>;
                captures_typed_pen() -> bool;
                tablet_presence() -> arcen_input::CapabilityAvailability;
                open_clipboard() -> Option<Box<dyn crate::clipboard::PlatformClipboard>>;
                probe_network(host: &str, port: u16) -> crate::network::NetworkProbeResult;
                outgoing_interface(peer: std::net::IpAddr) -> crate::network::OutgoingInterface;
                timezone() -> Option<String>;
            }

            fn start_microphone(
                &self,
                sink: Arc<dyn crate::microphone::MicrophoneFrameSink>,
                cancel: crate::microphone::MicrophoneCancel,
            ) -> Result<
                Box<dyn crate::microphone::MicrophoneStream>,
                crate::microphone::MicrophoneCaptureError,
            > {
                UnavailableServices.start_microphone(sink, cancel)
            }

            fn native_usb_component_state(&self) -> Option<State> {
                self.reads.fetch_add(1, Ordering::SeqCst);
                Some(if *self.installed.lock().unwrap() {
                    State::Ready
                } else {
                    State::NotInstalled
                })
            }

            fn run_native_usb_component_action(
                &self,
                _action: arcen_usb_bridge::NativeComponentAction,
            ) -> Result<(), String> {
                if self.fail {
                    return Err("the administrator declined".to_owned());
                }
                *self.installed.lock().unwrap() = true;
                Ok(())
            }
        }

        fn finish(control: &mut NativeTabletComponent, services: &Component) {
            for _ in 0..500 {
                let _ = control.control(services, crate::screens::wording::DeckPlatform::Pc);
                if !control.busy() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            panic!("the action never finished");
        }

        #[test]
        fn the_state_is_read_once_and_again_after_an_action_run_off_the_ui_thread() {
            let services = Arc::new(Component::default());
            let mut control = NativeTabletComponent::default();
            for _ in 0..3 {
                let shown = control
                    .control(services.as_ref(), crate::screens::wording::DeckPlatform::Pc)
                    .unwrap();
                assert!(!shown.ready);
            }
            assert_eq!(services.reads.load(Ordering::SeqCst), 1, "not per frame");
            control.clicked(Arc::clone(&services) as Arc<dyn DeckServices>, || {});
            finish(&mut control, &services);
            let shown = control
                .control(services.as_ref(), crate::screens::wording::DeckPlatform::Pc)
                .unwrap();
            assert!(shown.ready);
            assert_eq!(shown.error, None);
            assert_eq!(services.reads.load(Ordering::SeqCst), 2);
        }

        #[test]
        fn a_failed_action_is_shown_beside_the_control() {
            let services = Arc::new(Component {
                fail: true,
                ..Component::default()
            });
            let mut control = NativeTabletComponent::default();
            let _ = control.control(
                services.as_ref(),
                crate::screens::wording::DeckPlatform::Mac,
            );
            control.clicked(Arc::clone(&services) as Arc<dyn DeckServices>, || {});
            finish(&mut control, &services);
            let shown = control
                .control(
                    services.as_ref(),
                    crate::screens::wording::DeckPlatform::Mac,
                )
                .unwrap();
            assert_eq!(shown.error.as_deref(), Some("the administrator declined"));
            assert!(!shown.ready);
        }
    }

    fn sample_values() -> SettingsValues {
        SettingsValues {
            tab: SettingsTab::Input,
            log_level: LogLevel::ALL[0],
            security: SecurityMode::High,
            performance: PerformanceMode::High,
            color_fidelity: ColorFidelitySettings::default(),
            displays: DisplaysMode::ALL[0],
            hidpi: false,
            use_notch_area: false,
            remote_ui_scale_percent: 100,
            full_color_display_mask: 0,
            hidpi_display_mask: 0,
            cursor_preference: CursorMode::Local,
            typing_mode: TypingMode::Auto,
            option_key_mode: OptionKeyPreference::Automatic,
            paste_settings: PasteSettings::default(),
            clipboard_enabled: true,
            microphone_enabled: false,
            tablet_mode_requested: TabletModeMsg::LocalTermination,
            pen_buttons: PenButtonSettings::default(),
            exact_still_image: true,
            canvas: true,
            keyboard_capture: arcen_input::keyboard::KeyboardCapturePolicy::Host,
            capture_mac_system_shortcuts: false,
            keyboard_switch: arcen_input::keyboard::KeyboardSwitchChord::DEFAULT,
        }
    }

    #[test]
    fn saved_settings_round_trip_through_the_panels() {
        let saved = crate::settings::ClientSettings::default();
        let mut values = SettingsValues::from_client_settings(&saved, SettingsTab::Input);
        let mut written = saved;
        assert!(
            !values.apply_to(&mut written),
            "an untouched panel saves nothing"
        );

        values.tablet_mode_requested = TabletModeMsg::DisabledMouseCompat;
        values.exact_still_image = !saved.exact_still_image;
        values.canvas = !saved.canvas;
        assert!(values.apply_to(&mut written));
        assert_eq!(
            written.tablet_mode_requested,
            TabletModeMsg::DisabledMouseCompat
        );
        assert!(
            !written.tablet_input_enabled,
            "mouse compatibility takes no typed pen"
        );
        assert_eq!(
            SettingsValues::from_client_settings(&written, SettingsTab::Input),
            values
        );
    }

    #[test]
    fn an_unavailable_preset_is_shown_as_its_fallback_but_never_saved_by_looking() {
        let report = crate::decode_report::DecodeReport::from_capabilities(
            "Example GPU",
            crate::decode::DecodeCapabilities {
                h264: true,
                h265: true,
                ..crate::decode::DecodeCapabilities::default()
            },
        );
        for platform in [DeckPlatform::Mac, DeckPlatform::Pc] {
            let ctx = egui::Context::default();
            super::super::wording::set_deck_platform(&ctx, platform);
            let mut values = SettingsValues {
                tab: SettingsTab::Streaming,
                ..sample_values()
            };
            StreamingPreset::Hdr.apply_to(&mut values.performance, &mut values.color_fidelity);
            let before = values.clone();
            let facts = SettingsFacts {
                hdr_screen: false,
                notch: None,
                monitors: &[],
                host_clipboard_notice: None,
                connections: &[],
                native_tablet: None,
                decode: Some(report.clone()),
                support_bundle: None,
                connection_displays: None,
                canvas_available: false,
                system_shortcut_permission: None,
            };
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                assert!(render_settings(ui, &mut values, &facts).is_empty());
            });
            assert_eq!(values, before, "{platform:?}");
        }
    }

    const ALL_TABS: [SettingsTab; 8] = [
        SettingsTab::Language,
        SettingsTab::Streaming,
        SettingsTab::Displays,
        SettingsTab::Microphone,
        SettingsTab::Input,
        SettingsTab::Tablet,
        SettingsTab::Logs,
        SettingsTab::Advanced,
    ];

    fn sample_facts(
        platform: DeckPlatform,
        connections: &[crate::config::SavedConnectionSummary],
    ) -> SettingsFacts<'_> {
        SettingsFacts {
            hdr_screen: false,
            notch: (platform == DeckPlatform::Mac).then_some(false),
            monitors: &[],
            host_clipboard_notice: None,
            connections,
            native_tablet: None,
            decode: None,
            support_bundle: Some(super::super::support_bundle::SupportBundleView {
                open: true,
                ..super::super::support_bundle::SupportBundleView::default()
            }),
            connection_displays: Some(("Studio".to_owned(), DisplaysMode::MatchLayout)),
            canvas_available: false,
            system_shortcut_permission: None,
        }
    }

    fn collect_text(shape: &egui::Shape, texts: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(text) => texts.push(text.galley.job.text.clone()),
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_text(shape, texts);
                }
            }
            _ => {}
        }
    }

    /// Every piece of text a panel draws, in order.
    fn page_text(
        platform: DeckPlatform,
        values: &SettingsValues,
        facts: &SettingsFacts<'_>,
    ) -> Vec<String> {
        let ctx = egui::Context::default();
        super::super::wording::set_deck_platform(&ctx, platform);
        let mut values = values.clone();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 6000.0),
            )),
            ..egui::RawInput::default()
        };
        let output = ctx.run_ui(input, |ui| {
            let _ = render_settings(ui, &mut values, facts);
        });
        let mut texts = Vec::new();
        for clipped in &output.shapes {
            collect_text(&clipped.shape, &mut texts);
        }
        texts
    }

    /// The panel's own text, without the sidebar (which ends with
    /// Advanced).
    fn panel_text(
        platform: DeckPlatform,
        values: &SettingsValues,
        facts: &SettingsFacts<'_>,
    ) -> Vec<String> {
        let mut texts = page_text(platform, values, facts);
        let sidebar_end = texts
            .iter()
            .position(|text| text == SettingsTab::Advanced.label())
            .expect("the sidebar");
        texts.drain(..=sidebar_end);
        texts
    }

    #[test]
    fn the_sidebar_labels_are_in_sentence_case() {
        let texts = page_text(
            DeckPlatform::Mac,
            &sample_values(),
            &sample_facts(DeckPlatform::Mac, &[]),
        );
        for label in ["General", "Input and devices", "System"] {
            assert!(texts.iter().any(|text| text == label), "{label}: {texts:?}");
        }
        for text in &texts {
            let letters: Vec<char> = text.chars().filter(|c| c.is_alphabetic()).collect();
            assert!(
                letters.len() < 4 || letters.iter().any(|c| c.is_lowercase()),
                "tracked capitals: {text:?}"
            );
        }
    }

    #[test]
    fn every_panel_draws_on_both_decks_without_changing_a_value() {
        for platform in [DeckPlatform::Mac, DeckPlatform::Pc] {
            for tab in ALL_TABS {
                let ctx = egui::Context::default();
                super::super::wording::set_deck_platform(&ctx, platform);
                let mut values = SettingsValues {
                    tab,
                    ..sample_values()
                };
                let before = values.clone();
                let facts = sample_facts(platform, &[]);
                let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                    assert!(render_settings(ui, &mut values, &facts).is_empty());
                });
                assert_eq!(values, before, "{tab:?} on {platform:?}");
            }
        }
    }

    #[test]
    fn every_page_follows_both_palettes_with_readable_text_and_no_light_line() {
        use super::super::theme::{Palette, apply, checks, contrast_ratio};
        fn flatten(shape: egui::Shape, out: &mut Vec<egui::Shape>) {
            match shape {
                egui::Shape::Vec(shapes) => shapes.into_iter().for_each(|s| flatten(s, out)),
                shape => out.push(shape),
            }
        }
        for dark in [false, true] {
            let colours = Palette::of(dark);
            for tab in ALL_TABS {
                let ctx = egui::Context::default();
                apply(&ctx);
                ctx.set_theme(if dark {
                    egui::Theme::Dark
                } else {
                    egui::Theme::Light
                });
                super::super::wording::set_deck_platform(&ctx, DeckPlatform::Mac);
                let mut values = SettingsValues {
                    tab,
                    ..sample_values()
                };
                let facts = sample_facts(DeckPlatform::Mac, &[]);
                let mut shapes = Vec::new();
                for _ in 0..2 {
                    let output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                egui::vec2(1120.0, 724.0),
                            )),
                            ..egui::RawInput::default()
                        },
                        |ui| {
                            let _ = render_settings(ui, &mut values, &facts);
                        },
                    );
                    shapes.clear();
                    for clipped in output.shapes {
                        flatten(clipped.shape, &mut shapes);
                    }
                }
                for colour in checks::painted_colours(&shapes, colours.bg) {
                    assert!(
                        checks::quiet_enough(&colours, colour),
                        "{tab:?}, dark {dark}: {colour:?}"
                    );
                }
                for (text, ink, ground) in checks::texts_on_their_ground(&shapes, colours.bg) {
                    let ratio = contrast_ratio(ink, ground);
                    assert!(
                        ratio >= 4.5,
                        "{tab:?}, dark {dark}: {text:?} {ratio:.2}:1 ({ink:?} on {ground:?})"
                    );
                }
            }
        }
    }

    /// Whether the Deck's fonts draw `c` (Latin-1, or a symbol
    /// `theme::DRAWN_SYMBOLS` lists, which the macOS Deck checks against
    /// the real fonts).
    fn drawn(c: char) -> bool {
        u32::from(c) <= 0xFF || super::super::theme::DRAWN_SYMBOLS.contains(c)
    }

    #[test]
    fn no_panel_shows_a_character_its_fonts_draw_as_an_empty_box() {
        for platform in [DeckPlatform::Mac, DeckPlatform::Pc] {
            for tab in ALL_TABS {
                let values = SettingsValues {
                    tab,
                    ..sample_values()
                };
                let texts = page_text(platform, &values, &sample_facts(platform, &[]));
                assert!(!texts.is_empty(), "{tab:?} on {platform:?}");
                for text in texts {
                    assert!(!text.contains('□'), "{text:?}");
                    assert!(
                        text.chars().all(drawn),
                        "{tab:?} on {platform:?}: {:?} in {text:?}",
                        text.chars().filter(|c| !drawn(*c)).collect::<String>()
                    );
                }
            }
        }
    }

    #[test]
    fn the_input_page_has_one_heading_per_group_and_says_what_each_deck_does() {
        let connections = [
            crate::overrides::ConnectionOverrides {
                typing_mode: crate::overrides::Override::Own(TypingMode::Keys),
                clipboard_enabled: crate::overrides::Override::Own(false),
                ..crate::overrides::ConnectionOverrides::default()
            },
            crate::overrides::ConnectionOverrides {
                typing_mode: crate::overrides::Override::Own(TypingMode::Auto),
                ..crate::overrides::ConnectionOverrides::default()
            },
            crate::overrides::ConnectionOverrides::default(),
        ]
        .map(|overrides| crate::config::SavedConnectionSummary {
            id: "id".to_owned(),
            kind: crate::config::ConnectionKind::DirectMachine,
            name: "name".to_owned(),
            host: "203.0.113.10".to_owned(),
            port: 18_444,
            use_tls: true,
            transport: crate::config::SavedTransport::Quic,
            identity_security_mode: None,
            settings: crate::config::SavedConnectionSettings {
                overrides,
                ..crate::config::SavedConnectionSettings::default()
            },
            unknown: serde_json::Map::new(),
        });
        for platform in [DeckPlatform::Mac, DeckPlatform::Pc] {
            let values = SettingsValues {
                tab: SettingsTab::Input,
                ..sample_values()
            };
            let mut facts = sample_facts(platform, &connections);
            facts.host_clipboard_notice = Some(crate::clipboard::HOST_TURNED_CLIPBOARD_OFF);
            let texts = panel_text(platform, &values, &facts);
            let count = |wanted: &str| texts.iter().filter(|text| *text == wanted).count();
            for heading in ["Input", "Keyboard", "Typing", "Clipboard"] {
                assert_eq!(count(heading), 1, "{heading} on {platform:?}: {texts:?}");
            }
            for line in [
                choices::input_intro(platform, values.keyboard_switch)[1].as_str(),
                choices::SYSTEM_SHORTCUTS_TITLE,
                choices::system_shortcuts_line(platform),
                "Default for all connections. 2 connections use their own.",
                choices::CLIPBOARD_TITLE,
                "1 connection uses its own",
                "Clipboard is turned off by this host.",
                choices::LEARN_MORE,
            ] {
                assert_eq!(count(line), 1, "{line} on {platform:?}: {texts:?}");
            }
            // The Option key and Paste as Keystrokes are not on this page.
            assert!(texts.iter().all(|text| !text.contains("Option key")));
            assert!(texts.iter().all(|text| !text.contains("Paste")));
        }
        // Each Deck names its own shortcuts.
        assert!(choices::system_shortcuts_line(DeckPlatform::Mac).starts_with("⌘Tab"));
        assert!(choices::system_shortcuts_line(DeckPlatform::Pc).starts_with("Alt+Tab"));
    }

    #[test]
    fn the_system_shortcuts_switch_counts_connections_with_their_own_and_their_permission() {
        let connection = |system_shortcuts| crate::config::SavedConnectionSummary {
            id: "id".to_owned(),
            kind: crate::config::ConnectionKind::DirectMachine,
            name: "name".to_owned(),
            host: "203.0.113.10".to_owned(),
            port: 18_444,
            use_tls: true,
            transport: crate::config::SavedTransport::Quic,
            identity_security_mode: None,
            settings: crate::config::SavedConnectionSettings {
                overrides: crate::overrides::ConnectionOverrides {
                    system_shortcuts,
                    ..crate::overrides::ConnectionOverrides::default()
                },
                ..crate::config::SavedConnectionSettings::default()
            },
            unknown: serde_json::Map::new(),
        };
        use crate::overrides::Override;
        let connections = [
            connection(Override::Own(true)),
            connection(Override::Own(false)),
            connection(Override::Inherit),
        ];
        let values = SettingsValues {
            tab: SettingsTab::Input,
            ..sample_values()
        };
        for platform in [DeckPlatform::Mac, DeckPlatform::Pc] {
            let texts = panel_text(platform, &values, &sample_facts(platform, &connections));
            assert!(
                texts
                    .iter()
                    .any(|text| text == "2 connections use their own"),
                "{platform:?}: {texts:?}"
            );
        }
        // Off in Settings, but a connection sends them: the one app-wide
        // permission is still needed.
        assert!(!system_shortcuts_to_host(
            DeckPlatform::Mac,
            values.keyboard_capture,
            values.capture_mac_system_shortcuts
        ));
        let mut facts = sample_facts(DeckPlatform::Mac, &connections);
        facts.system_shortcut_permission = Some(false);
        let texts = panel_text(DeckPlatform::Mac, &values, &facts);
        assert!(
            texts
                .iter()
                .any(|text| text == "Needs Accessibility permission."),
            "{texts:?}"
        );
        let following = [connection(Override::Inherit)];
        let mut facts = sample_facts(DeckPlatform::Mac, &following);
        facts.system_shortcut_permission = Some(false);
        let texts = panel_text(DeckPlatform::Mac, &values, &facts);
        assert!(
            texts
                .iter()
                .all(|text| text != "Needs Accessibility permission."
                    && !text.contains("use their own")
                    && !text.contains("uses its own")),
            "{texts:?}"
        );
    }

    #[test]
    fn the_tablet_page_is_one_picker_and_its_footer() {
        for platform in [DeckPlatform::Mac, DeckPlatform::Pc] {
            let values = SettingsValues {
                tab: SettingsTab::Tablet,
                ..sample_values()
            };
            let texts = panel_text(platform, &values, &sample_facts(platform, &[]));
            // One picker: the choice in force and its line, the others
            // only once it opens.
            let chosen = choices::tablet_choices()
                .into_iter()
                .find(|choice| choice.value == values.tablet_mode_requested)
                .expect("a tablet choice");
            assert_eq!(
                texts
                    .iter()
                    .filter(|text| text.starts_with(chosen.title.as_str()))
                    .count(),
                1,
                "{texts:?}"
            );
            assert_eq!(texts.iter().filter(|text| **text == chosen.line).count(), 1);
            for other in choices::tablet_choices() {
                if other.value != chosen.value {
                    assert!(texts.iter().all(|text| *text != other.line), "{texts:?}");
                }
            }
            assert!(texts.iter().any(|text| text == choices::TABLET_FOOTER));
            // The pen buttons, under Tablet support: one on a PC.
            let pen_buttons = texts
                .iter()
                .filter(|text| text.ends_with("pen button") || *text == "Pen button")
                .count();
            assert_eq!(
                pen_buttons,
                if platform == DeckPlatform::Pc { 1 } else { 2 },
                "{texts:?}"
            );
        }
    }

    #[test]
    fn advanced_input_has_the_paste_tuning_and_the_keyboard_switch_but_no_option_key() {
        for platform in [DeckPlatform::Mac, DeckPlatform::Pc] {
            let values = SettingsValues {
                tab: SettingsTab::Advanced,
                ..sample_values()
            };
            let texts = panel_text(platform, &values, &sample_facts(platform, &[]));
            for wanted in [
                choices::ADVANCED_INPUT_HEADING,
                choices::KEYBOARD_SWITCH_HEADING,
                choices::PASTE_HEADING,
                choices::PASTE_SPEED,
                choices::PASTE_ASK_ABOVE,
            ] {
                assert_eq!(
                    texts.iter().filter(|text| *text == wanted).count(),
                    1,
                    "{wanted} on {platform:?}"
                );
            }
            assert!(texts.iter().all(|text| !text.contains("Option key")
                && !text.contains("AltGr")
                && !text.contains("Option is Alt")));
        }
    }

    #[test]
    fn a_saved_low_this_deck_cannot_use_shows_as_medium_with_a_note() {
        if crate::settings::low_security_allowed() {
            // ARCEN_ACCEPT_INSECURE=1 in this environment: Low is offered.
            return;
        }
        for platform in [DeckPlatform::Mac, DeckPlatform::Pc] {
            let values = SettingsValues {
                tab: SettingsTab::Advanced,
                security: SecurityMode::Low,
                ..sample_values()
            };
            let texts = panel_text(platform, &values, &sample_facts(platform, &[]));
            assert!(texts.iter().all(|text| text != "Low"), "{texts:?}");
            assert!(
                texts.iter().any(|text| text.starts_with("Medium")),
                "{texts:?}"
            );
            assert!(
                texts
                    .iter()
                    .any(|text| text == choices::LOW_SECURITY_OFF_NOTE)
            );
            // Looking saves nothing.
            let ctx = egui::Context::default();
            super::super::wording::set_deck_platform(&ctx, platform);
            let mut shown = values.clone();
            let facts = sample_facts(platform, &[]);
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                assert!(render_settings(ui, &mut shown, &facts).is_empty());
            });
            assert_eq!(shown, values);
        }
        assert_eq!(SecurityMode::Low.in_force_with(false), SecurityMode::Medium);
        assert_eq!(SecurityMode::Low.in_force_with(true), SecurityMode::Low);
        assert!(crate::settings::low_security_allowed_with(Some("1")));
        assert!(!crate::settings::low_security_allowed_with(Some("true")));
        assert!(!crate::settings::low_security_allowed_with(None));
    }

    #[test]
    fn every_choice_is_one_line() {
        let platforms = [DeckPlatform::Mac, DeckPlatform::Pc];
        let mut rows: Vec<(String, String)> = Vec::new();
        rows.extend(
            choices::typing_choices()
                .into_iter()
                .map(|choice| (choice.title, choice.line)),
        );
        rows.extend(
            choices::tablet_choices()
                .into_iter()
                .map(|choice| (choice.title, choice.line)),
        );
        for platform in platforms {
            rows.push((
                choices::SYSTEM_SHORTCUTS_TITLE.to_owned(),
                choices::system_shortcuts_line(platform).to_owned(),
            ));
        }
        for (title, line) in rows {
            assert!(!title.contains('\n') && title.len() <= 48, "{title:?}");
            assert!(
                !line.contains('\n') && line.chars().count() <= 100,
                "{line:?}"
            );
            assert_eq!(line.matches(". ").count(), 0, "one sentence: {line:?}");
            assert!(line.chars().all(drawn) && title.chars().all(drawn));
        }
    }

    #[test]
    fn the_one_system_shortcuts_switch_keeps_each_decks_default() {
        use arcen_input::keyboard::KeyboardCapturePolicy::{Host, ThisComputer};
        let defaults = crate::settings::ClientSettings::default();
        // Off on a Mac (it needs Accessibility), on on a PC, as before.
        assert!(!system_shortcuts_to_host(
            DeckPlatform::Mac,
            defaults.keyboard_capture,
            defaults.capture_mac_system_shortcuts
        ));
        assert!(system_shortcuts_to_host(
            DeckPlatform::Pc,
            defaults.keyboard_capture,
            defaults.capture_mac_system_shortcuts
        ));
        let (mut capture, mut mac) = (ThisComputer, false);
        set_system_shortcuts_to_host(DeckPlatform::Mac, true, &mut capture, &mut mac);
        assert_eq!((capture, mac), (Host, true));
        set_system_shortcuts_to_host(DeckPlatform::Mac, false, &mut capture, &mut mac);
        assert!(!mac);
        assert!(!system_shortcuts_to_host(DeckPlatform::Mac, capture, mac));
        let (mut capture, mut mac) = (Host, false);
        set_system_shortcuts_to_host(DeckPlatform::Pc, false, &mut capture, &mut mac);
        assert_eq!(capture, ThisComputer);
        set_system_shortcuts_to_host(DeckPlatform::Pc, true, &mut capture, &mut mac);
        assert_eq!(capture, Host);
    }

    #[test]
    fn every_short_single_choice_is_the_shared_list_picker() {
        let source = include_str!("settings.rs");
        let code = source.split("#[cfg(test)]").next().expect("the code");
        // No radio rows and no combo boxes, the developer build's
        // streaming overrides included.
        assert!(!code.contains(".radio(") && !code.contains("radio_value("));
        assert!(!code.contains("ComboBox"));
        for id in [
            "\"language\"",
            "\"streaming_preset\"",
            "\"displays_mode\"",
            "\"remote_ui_scale\"",
            "\"typing_mode\"",
            "\"tablet_mode\"",
            "\"log_level\"",
            "\"certificate_checking\"",
            "\"paste_chars_per_second\"",
            "\"paste_confirm_above\"",
            "\"keyboard_switch_preset\"",
            "\"streaming_performance_override\"",
            "\"color_fidelity_variant_override\"",
        ] {
            assert!(code.contains(id), "{id} is a list picker");
        }
    }

    /// Every shape one settled pass of `tab` draws, themed, at 1120 x 760.
    fn page_shapes(tab: SettingsTab, dark: bool) -> Vec<egui::Shape> {
        fn flatten(shape: egui::Shape, out: &mut Vec<egui::Shape>) {
            match shape {
                egui::Shape::Vec(shapes) => shapes.into_iter().for_each(|s| flatten(s, out)),
                shape => out.push(shape),
            }
        }
        let ctx = egui::Context::default();
        super::super::theme::apply(&ctx);
        ctx.set_theme(if dark {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        });
        super::super::wording::set_deck_platform(&ctx, DeckPlatform::Mac);
        let mut values = SettingsValues {
            tab,
            ..sample_values()
        };
        let facts = SettingsFacts {
            native_tablet: Some(NativeTabletControl {
                action_label: "Install\u{2026}".to_owned(),
                tint: ComponentTint::Action,
                ready: false,
                guidance: "Needs its component.".to_owned(),
                error: None,
            }),
            system_shortcut_permission: Some(false),
            canvas_available: true,
            ..sample_facts(DeckPlatform::Mac, &[])
        };
        if tab == SettingsTab::Tablet {
            values.tablet_mode_requested = TabletModeMsg::UsbTabletBridge;
        }
        if tab == SettingsTab::Input {
            set_system_shortcuts_to_host(
                DeckPlatform::Mac,
                true,
                &mut values.keyboard_capture,
                &mut values.capture_mac_system_shortcuts,
            );
        }
        let mut shapes = Vec::new();
        for _ in 0..2 {
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1120.0, 760.0),
                    )),
                    ..egui::RawInput::default()
                },
                |ui| {
                    let _ = render_settings(ui, &mut values, &facts);
                },
            );
            shapes.clear();
            for clipped in output.shapes {
                flatten(clipped.shape, &mut shapes);
            }
        }
        shapes
    }

    #[test]
    fn every_settings_button_is_the_shared_control_and_the_bar_shows() {
        use super::super::launcher::{LAUNCHER_BAR_FILL, LAUNCHER_BAR_HEIGHT};
        use super::super::theme::{CONTROL_HEIGHT, Palette};
        for dark in [false, true] {
            let colours = Palette::of(dark);
            for tab in ALL_TABS {
                let shapes = page_shapes(tab, dark);
                // The orange bar across the top, with Done and no gear.
                assert!(
                    shapes
                        .iter()
                        .any(|shape| matches!(shape, egui::Shape::Rect(rect)
                        if rect.fill == LAUNCHER_BAR_FILL
                            && rect.rect.top() < 1.0
                            && (rect.rect.height() - LAUNCHER_BAR_HEIGHT).abs() < 0.5)),
                    "{tab:?}: the bar"
                );
                assert!(
                    shapes
                        .iter()
                        .any(|shape| matches!(shape, egui::Shape::Text(text)
                        if text.galley.job.text == "Done" && text.pos.y < LAUNCHER_BAR_HEIGHT)),
                    "{tab:?}: Done on the bar"
                );
                // Every outlined control (a button, a field, a picker) is
                // the shared one: as tall as every control.
                for shape in &shapes {
                    if let egui::Shape::Rect(rect) = shape
                        && rect.stroke.width > 0.0
                        && rect.stroke.color == colours.field_border
                        && rect.rect.width() > 24.0
                    {
                        assert!(
                            (rect.rect.height() - CONTROL_HEIGHT).abs() < 0.5,
                            "{tab:?}, dark {dark}: a control {:?} tall",
                            rect.rect.height()
                        );
                    }
                }
            }
        }
        // And in the code: no stock egui button on a page.
        for source in [
            include_str!("settings.rs"),
            include_str!("support_bundle.rs"),
            include_str!("profile.rs"),
        ] {
            let code = source.split("#[cfg(test)]").next().expect("the code");
            assert!(!code.contains(".button(") && !code.contains("Button::new("));
        }
    }

    #[test]
    fn every_controls_description_is_the_small_muted_help() {
        let shapes = page_shapes(SettingsTab::Streaming, true);
        let texts: Vec<_> = shapes
            .iter()
            .filter_map(|shape| match shape {
                egui::Shape::Text(text) => Some(text),
                _ => None,
            })
            .collect();
        let size = |wanted: &str| {
            texts
                .iter()
                .find(|text| text.galley.job.text == wanted)
                .and_then(|text| text.galley.job.sections.first())
                .map_or_else(
                    || panic!("no {wanted}"),
                    |section| section.format.font_id.size,
                )
        };
        let x = |wanted: &str| {
            texts
                .iter()
                .find(|text| text.galley.job.text == wanted)
                .map_or_else(|| panic!("no {wanted}"), |text| text.pos.x)
        };
        for description in [EXACT_STILL_IMAGE_DESCRIPTION, CANVAS_DESCRIPTION] {
            assert!((size(description) - theme::TEXT_LABEL).abs() < 0.01);
        }
        // Under its check box's label, not its box.
        assert!((x(EXACT_STILL_IMAGE_DESCRIPTION) - x("Exact still image")).abs() < 1.0);
    }
}
