// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Settings: the eight panels (Language, Streaming, Displays, Microphone,
//! Clipboard, Input, Logs, Advanced) with the same wording on both Decks.
//!
//! The shell hands in a copy of the current values ([`SettingsValues`]) and
//! what the platform knows ([`SettingsFacts`]); [`render_settings`] edits the
//! copy and returns what the user asked for ([`SettingsAction`]). The shell
//! compares, persists and applies, exactly once per pass.

use arcen_input::keyboard::DeckShortcut;
use arcen_input::text::paste::PasteSettings;
use arcen_input::text::{OptionKeyPreference, TypingMode};
use arcen_protocol::messages::{ClientMonitor, ClipboardPolicyMsg, CursorMode, TabletModeMsg};

use super::theme;
use super::widgets::{
    settings_nav_item, settings_nav_section_label, settings_radio, settings_radio_with_trailing,
};
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
    Clipboard,
    Input,
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
            Self::Clipboard => "Clipboard",
            Self::Input => "Input",
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
    pub exact_still_image: bool,
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
            exact_still_image: settings.exact_still_image,
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
        settings.exact_still_image = self.exact_still_image;
        changed
    }
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

/// How the Native tablet component's button is tinted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentTint {
    /// Installed and running.
    Ready,
    /// Waiting for the user (approval in system settings).
    Attention,
    /// Something to do (install, start, recheck).
    Action,
}

impl ComponentTint {
    #[must_use]
    pub const fn colour(self) -> egui::Color32 {
        match self {
            Self::Ready => theme::SUCCESS,
            Self::Attention => theme::WARNING,
            Self::Action => theme::ACCENT,
        }
    }
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
    /// The clipboard policy the host granted in the current session.
    pub clipboard_policy: Option<ClipboardPolicyMsg>,
    /// Which connection the tablet mode is saved for.
    pub tablet_scope_note: String,
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
}

/// The note under Settings › Input › Tablet: which saved connection the
/// tablet mode is remembered for, if any.
#[must_use]
pub fn tablet_scope_note(connection_name: Option<&str>) -> String {
    match connection_name {
        Some(name) => format!(
            "Remembered for \u{201c}{name}\u{201d} only. Each saved connection keeps its \
             own tablet mode; choose it again for your other hosts."
        ),
        None => {
            "Applies to this session only: select a saved connection to remember it.".to_string()
        }
    }
}

#[must_use]
pub const fn tablet_mode_label(mode: TabletModeMsg) -> &'static str {
    match mode {
        TabletModeMsg::LocalTermination => "Tablet support",
        TabletModeMsg::UsbTabletBridge => "Native tablet (USB bridged)",
        TabletModeMsg::DisabledMouseCompat => "Mouse compatibility only",
    }
}

/// What Native tablet does on each kind of host, one sentence each, where a
/// host genuinely differs (Q-16: any tablet by default; the exceptions are
/// stated, never hidden).
pub const NATIVE_TABLET_TARGET_NOTES: [&str; 3] = [
    "On a Linux host the host's own driver claims the tablet; an X11 desktop needs \
     xf86-input-wacom, and a headless Wayland desktop refuses Native tablet because it has no \
     seat to confine a USB device to.",
    "On a macOS host Arcen reads the pen itself, from any tablet that reports a standard pen \
     (pressure, tilt, buttons and eraser), with more for models that have a dedicated reader; \
     touch and the tablet's own keys are not used there.",
    "On a Windows host Arcen also reads the pen itself, from any tablet that reports a standard \
     pen, and injects it as a Windows Ink pen (pressure, tilt, buttons and eraser), with no \
     driver installed; touch and the tablet's own keys are not used there.",
];

#[must_use]
pub fn tablet_mode_description(mode: TabletModeMsg, platform: DeckPlatform) -> String {
    let targets = NATIVE_TABLET_TARGET_NOTES.join(" ");
    let this = platform.this_machine();
    let this_capital = platform.this_machine_capitalised();
    let apps = words::applications(platform);
    let os = words::os_name(platform);
    match mode {
        // Both descriptions lead with the network, because that is the choice
        // the operator is actually making. Everything else about these two
        // modes follows from where the pen is interpreted, and where the pen is
        // interpreted is decided by how far away the host is.
        TabletModeMsg::LocalTermination => format!(
            "For Wi-Fi, 5G, and working away from the office — good enough for most remote \
             work. {this_capital}'s tablet driver reads the pen and Arcen sends finished pen events \
             to the host, so no tablet driver is needed there and no pen sample has to wait for \
             a reply. Distance costs you responsiveness, not correctness.\n\n\
             Pressure, tilt, rotation, eraser, proximity, and barrel buttons all work; finger \
             touch and the tablet's own buttons stay on {this}. The tablet also stays usable \
             in {apps} while you are connected. Any pen tablet works, and handedness and \
             mapping follow {this}'s own pen and tablet settings."
        ),
        TabletModeMsg::UsbTabletBridge => format!(
            "For a LAN — the office nearby, not the other side of the country. As close to \
             plugging the tablet straight into the host as a network allows: a privileged \
             Arcen helper takes the device away from {os} and forwards its raw USB traffic, \
             and the host's own driver for that tablet claims it exactly as if it were plugged \
             in there. Any pen tablet works; models with a dedicated reader get extra features. \
             The whole device works — pressure, tilt, rotation, eraser, the tablet's own \
             buttons, and finger touch — wherever the host's own driver claims it.\n\n\
             {targets}\n\n\
             In exchange: every pen sample makes a full round trip to the host, so latency is \
             felt directly and this is not a WAN mode. {apps} cannot use the tablet \
             until you disconnect, and the host needs its own driver for the tablet and USB \
             virtualization. The host's driver moves the host's pointer; with Local cursor \
             rendering Arcen Deck draws the cursor where the host reports it, which is the \
             recommended pairing (host cursor rendering also works, one video round trip \
             later). The helper is what runs privileged; Arcen Deck itself never does."
        ),
        TabletModeMsg::DisabledMouseCompat => "No tablet redirection. The pen behaves as a mouse; \
             no pressure, tilt, eraser, proximity, or tablet buttons are available. No \
             tablet-specific Arcen dependencies."
            .to_owned(),
    }
}

/// Draws the Settings screen. Edits `values` in place and returns what the
/// user asked for, in the order asked.
pub fn render_settings(
    ui: &mut egui::Ui,
    values: &mut SettingsValues,
    facts: &SettingsFacts<'_>,
) -> Vec<SettingsAction> {
    let platform = deck_platform(ui.ctx());
    let accent = theme::ACCENT;
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
        mut cursor_preference,
        mut typing_mode,
        mut option_key_mode,
        mut paste_settings,
        mut clipboard_enabled,
        mut microphone_enabled,
        mut tablet_mode_requested,
        mut exact_still_image,
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
    let effective_clipboard = facts.clipboard_policy;

    // No accent header bar — the reference design relies on the native
    // window frame. We keep only a subtle top-right "Done" ghost button to
    // return Home, since this is a single-window app rather than a separate
    // settings window. Settings persist live on change, so there is no Save.
    let width = ui.available_width();
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.set_width(width);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(28.0);
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new("Done")
                            .color(accent)
                            .size(15.0)
                            .strong(),
                    )
                    // Warm neutrals taken from the Settings nav pills below
                    // (hover and active), so the ghost button belongs to the
                    // same palette instead of introducing a second hue.
                    .fill(egui::Color32::from_rgb(0xF0, 0xEC, 0xE4))
                    .stroke(egui::Stroke::new(
                        1.0,
                        egui::Color32::from_rgb(0xE5, 0xDE, 0xD2),
                    ))
                    .corner_radius(8.0)
                    .min_size(egui::vec2(72.0, 32.0)),
                )
                .clicked()
            {
                actions.push(SettingsAction::Close);
            }
        });
    });
    ui.add_space(2.0);

    let heading_col = egui::Color32::from_rgb(0x1A, 0x1A, 0x1A);
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
                        settings_nav_section_label(ui, "GENERAL");
                        for item in [
                            SettingsTab::Language,
                            SettingsTab::Streaming,
                            SettingsTab::Displays,
                        ] {
                            if settings_nav_item(ui, item.label(), item == tab, accent) {
                                tab = item;
                            }
                            ui.add_space(2.0);
                        }

                        ui.add_space(8.0);
                        settings_nav_section_label(ui, "INPUT & DEVICES");
                        for item in [
                            SettingsTab::Microphone,
                            SettingsTab::Clipboard,
                            SettingsTab::Input,
                        ] {
                            if settings_nav_item(ui, item.label(), item == tab, accent) {
                                tab = item;
                            }
                            ui.add_space(2.0);
                        }

                        ui.add_space(8.0);
                        settings_nav_section_label(ui, "SYSTEM");
                        for item in [SettingsTab::Logs, SettingsTab::Advanced] {
                            if settings_nav_item(ui, item.label(), item == tab, accent) {
                                tab = item;
                            }
                            ui.add_space(2.0);
                        }
                    });
            });
            ui.add_space(12.0);
            ui.separator();
            ui.add_space(44.0);
            ui.vertical(|ui| {
                ui.set_width((ui.available_width() - 44.0).clamp(360.0, 760.0));
                egui::ScrollArea::vertical()
                    .id_salt("settings_content_scroll")
                    .max_height(body_height)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                    // Bump the body text size for the whole content column so the
                    // descriptions read at reference proportions without touching
                    // every label individually.
                    ui.style_mut().text_styles.insert(
                        egui::TextStyle::Body,
                        egui::FontId::proportional(16.0),
                    );
                    ui.add_space(16.0);
                    match tab {
                    SettingsTab::Language => {
                        ui.label(
                            egui::RichText::new("Language")
                                .color(heading_col)
                                .size(30.0),
                        );
                        ui.add_space(20.0);
                        ui.label(
                            egui::RichText::new(
                                "Select the preferred language for your Arcen client.",
                            )
                            .color(egui::Color32::DARK_GRAY),
                        );
                        ui.add_space(24.0);
                        egui::ComboBox::from_id_salt("language")
                            .selected_text("English (US)")
                            .show_ui(ui, |ui| {
                                let _ = ui.selectable_label(true, "English (US)");
                            });
                    }
                    SettingsTab::Streaming => {
                        ui.label(
                            egui::RichText::new("Streaming")
                                .color(heading_col)
                                .size(30.0),
                        );
                        ui.add_space(18.0);
                        ui.label(
                            egui::RichText::new(
                                "Choose the result you need. Each preset fixes frame rate, colour \
                                 fidelity, and encoder intent together so the request cannot \
                                 become an accidental combination of unrelated switches.",
                            )
                            .color(egui::Color32::DARK_GRAY),
                        );
                        ui.add_space(16.0);
                        for preset in StreamingPreset::PRIMARY {
                            let unavailable = match preset {
                                StreamingPreset::Grading => fidelity.grading.as_deref(),
                                StreamingPreset::Hdr => fidelity.hdr.as_deref(),
                                _ => None,
                            };
                            ui.add_enabled_ui(unavailable.is_none(), |ui| {
                                settings_radio(
                                    ui,
                                    &mut streaming_preset,
                                    preset,
                                    (preset == StreamingPreset::Auto).then_some("(Default)"),
                                    preset.title(),
                                    unavailable.unwrap_or_else(|| preset.description()),
                                );
                            });
                        }
                        if let Some(codecs) = fidelity.codecs.as_deref() {
                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new(codecs).color(egui::Color32::DARK_GRAY),
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
                                .color(theme::DANGER),
                            );
                        }
                        ui.add_space(18.0);
                        ui.checkbox(&mut exact_still_image, "Exact still image");
                        ui.label(
                            egui::RichText::new(EXACT_STILL_IMAGE_DESCRIPTION)
                                .color(egui::Color32::DARK_GRAY),
                        );
                        ui.add_space(12.0);
                        ui.horizontal_wrapped(|ui| {
                            if ui.button("Colour check…").clicked() {
                                actions.push(SettingsAction::OpenColourCheck);
                            }
                            ui.label(
                                egui::RichText::new(format!(
                                    "What the host serves, what {} shows, and whether the \
                                     code values arrive exactly.",
                                    platform.this_machine()
                                ))
                                .color(egui::Color32::DARK_GRAY),
                            );
                        });
                    }
                    SettingsTab::Displays => {
                        ui.label(
                            egui::RichText::new("Displays")
                                .color(heading_col)
                                .size(30.0),
                        );
                        ui.add_space(18.0);
                        ui.label(
                            egui::RichText::new(
                                "Choose how the remote stream is sized. Windowed follows the app \
                                 window. The other modes size the stream to what the Deck can \
                                 actually show in fullscreen and enter fullscreen on connect.",
                            )
                            .color(egui::Color32::DARK_GRAY),
                        );
                        ui.add_space(16.0);
                        for mode in DisplaysMode::ALL {
                            settings_radio(
                                ui,
                                &mut displays,
                                mode,
                                None,
                                mode.label(),
                                mode.description(),
                            );
                        }
                        if let Some((connection, mode)) = &facts.connection_displays {
                            ui.add_space(10.0);
                            ui.label(
                                egui::RichText::new(
                                    crate::connect_plan::connection_displays_note(
                                        connection, *mode,
                                    ),
                                )
                                .color(egui::Color32::from_rgb(0x8A, 0x6D, 0x3B)),
                            );
                            ui.add_space(4.0);
                            if ui
                                .button(crate::connect_plan::connection_displays_follow_label(
                                    connection,
                                ))
                                .clicked()
                            {
                                actions.push(SettingsAction::FollowDisplaysSetting);
                            }
                        }

                        // A screen with no notch at all (the Windows Deck)
                        // has no notch section (PARITY Q-7).
                        if let Some(has_notch) = facts.notch {
                        ui.add_space(22.0);
                        ui.label(
                            egui::RichText::new("Notch area")
                                .color(egui::Color32::BLACK)
                                .size(19.0),
                        );
                        ui.add_space(8.0);
                        ui.add_enabled_ui(has_notch, |ui| {
                            ui.checkbox(
                                &mut use_notch_area,
                                "Extend fullscreen under the notch",
                            );
                        });
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(if has_notch {
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
                            })
                            .color(egui::Color32::DARK_GRAY),
                        );
                        }

                        ui.add_space(22.0);
                        ui.label(
                            egui::RichText::new("Remote UI scale")
                                .color(egui::Color32::BLACK)
                                .size(19.0),
                        );
                        ui.add_space(8.0);
                        let mut remote_ui_scale =
                            RemoteUiScale::from_percent(remote_ui_scale_percent);
                        egui::ComboBox::from_id_salt("remote_ui_scale")
                            .selected_text(remote_ui_scale.label())
                            .show_ui(ui, |ui| {
                                for scale in RemoteUiScale::ALL {
                                    ui.selectable_value(
                                        &mut remote_ui_scale,
                                        scale,
                                        scale.label(),
                                    );
                                }
                            });
                        remote_ui_scale_percent = remote_ui_scale.as_percent();
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(format!(
                                "How large the remote desktop's own interface is, on hosts \
                                 that take their scaling from the display Arcen advertises \
                                 — that is Windows. Automatic reports this panel's real \
                                 size, so Windows picks the scaling matching its physical \
                                 density, which on a {} is large; a lower \
                                 percentage trades apparent size for desktop space. This \
                                 changes the remote desktop's layout, never the stream's \
                                 1:1 pixel mapping. Takes effect at the next connection.",
                                words::dense_panel(platform)
                            ))
                            .color(egui::Color32::DARK_GRAY),
                        );
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(
                                "Linux hosts ignore this. A GNOME session on Xorg offers \
                                 only 100% and 200%, and chooses for itself. To change \
                                 apparent size on a Linux host, use HiDPI streaming below: \
                                 with it off the remote interface is roughly twice the \
                                 size it is with it on.",
                            )
                            .color(egui::Color32::from_rgb(0x8A, 0x6D, 0x3B)),
                        );

                        ui.add_space(22.0);
                        ui.label(
                            egui::RichText::new(words::hidpi_heading(platform))
                                .color(egui::Color32::BLACK)
                                .size(19.0),
                        );
                        ui.add_space(8.0);
                        ui.checkbox(&mut hidpi, words::hidpi_checkbox(platform));
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(words::hidpi_description(platform))
                            .color(egui::Color32::DARK_GRAY),
                        );

                        ui.add_space(22.0);
                        ui.label(
                            egui::RichText::new("Per-display color priority")
                                .color(egui::Color32::BLACK)
                                .size(19.0),
                        );
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(
                                "Advanced: require full-color 4:4:4 on selected display numbers. \
                                 When at least one is selected, unselected displays may use \
                                 host-approved 4:2:0 hardware or software fallback. Pier still \
                                 decides which configured GPU/encoder resources may be used and \
                                 rejects the whole layout if it cannot satisfy every requirement.",
                            )
                            .color(egui::Color32::DARK_GRAY),
                        );
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("Detected displays")
                                    .color(egui::Color32::BLACK)
                                    .size(19.0),
                            );
                            ui.add_space(10.0);
                            if ui.button("↻  Refresh").clicked() {
                                actions.push(SettingsAction::RefreshDisplays);
                            }
                        });
                        ui.add_space(8.0);
                        if facts.monitors.is_empty() {
                            ui.label(
                                egui::RichText::new(
                                    "No displays detected on this platform. The host will fall \
                                     back to its configured default size.",
                                )
                                .color(egui::Color32::DARK_GRAY),
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
                                            .color(egui::Color32::WHITE)
                                            .background_color(egui::Color32::from_rgb(
                                                0x00, 0x89, 0x89,
                                            )),
                                    );
                                    ui.label(
                                        egui::RichText::new(&name)
                                            .color(egui::Color32::BLACK)
                                            .size(16.0),
                                    );
                                    if mon.is_primary {
                                        ui.label(
                                            egui::RichText::new("• Primary")
                                                .color(egui::Color32::from_rgb(
                                                    0x00, 0x89, 0x89,
                                                ))
                                                .size(14.0),
                                        );
                                    }
                                    if idx < 4 {
                                        let bit = 1u8 << idx;
                                        let mut full_color =
                                            full_color_display_mask & bit != 0;
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
                                            "{} × {} px   ·   scale {}   ·   {} Hz   ·   at ({}, {})",
                                            mon.width_px,
                                            mon.height_px,
                                            scale,
                                            mon.refresh_hz,
                                            mon.x,
                                            mon.y,
                                        ))
                                        .color(egui::Color32::DARK_GRAY),
                                    );
                                    // Fullscreen presentation size: what a
                                    // pinned session actually streams. Shown
                                    // only when it differs, i.e. on a notched
                                    // panel, so the number is never a mystery.
                                    if let Some([pw, ph]) = row
                                        .fullscreen_size(hidpi)
                                        .filter(|size| {
                                            *size != [mon.width_px, mon.height_px]
                                        })
                                    {
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "streams {pw} × {ph} px in fullscreen{}",
                                                words::fullscreen_size_reason(platform)
                                            ))
                                            .color(egui::Color32::DARK_GRAY),
                                        );
                                    }
                                });
                                ui.add_space(6.0);
                            }
                        }
                    }
                    SettingsTab::Microphone => {
                        ui.label(
                            egui::RichText::new("Microphone")
                                .color(heading_col)
                                .size(30.0),
                        );
                        ui.add_space(18.0);
                        ui.checkbox(
                            &mut microphone_enabled,
                            "Allow microphone forwarding for this launch",
                        );
                        ui.add_space(10.0);
                        ui.label(
                            egui::RichText::new(format!(
                                "Off by default. When enabled, Deck still waits for host policy, \
                                 negotiated microphone-v1 capabilities, and {} microphone \
                                 permission before capture starts. Disabling this setting stops \
                                 capture and requires reconnecting.",
                                words::os_name(platform)
                            ))
                            .color(egui::Color32::DARK_GRAY),
                        );
                    }
                    SettingsTab::Clipboard => {
                        ui.label(
                            egui::RichText::new("Clipboard")
                                .color(heading_col)
                                .size(30.0),
                        );
                        ui.add_space(18.0);
                        ui.checkbox(&mut clipboard_enabled, "Enable clipboard redirection");
                        ui.add_space(10.0);
                        ui.label(
                            egui::RichText::new(
                                "Clipboard redirection is enabled by default. The host is \
                                 authoritative and may narrow or disable direction, content, and \
                                 transfer size. Turning this off advertises no clipboard \
                                 capability and starts no pasteboard polling.",
                            )
                            .color(egui::Color32::DARK_GRAY),
                        );
                        ui.add_space(20.0);
                        ui.label(
                            egui::RichText::new("Effective host policy")
                                .color(egui::Color32::BLACK)
                                .size(19.0),
                        );
                        let policy_text = if !clipboard_enabled {
                            "Disabled locally".to_string()
                        } else if let Some(policy) = effective_clipboard {
                            format!(
                                "{:?} · {:?} · {} MiB",
                                policy.direction,
                                policy.content,
                                policy.max_bytes / (1024 * 1024)
                            )
                        } else {
                            "Not negotiated or disabled by host".to_string()
                        };
                        ui.label(
                            egui::RichText::new(policy_text).color(egui::Color32::DARK_GRAY),
                        );
                    }
                    SettingsTab::Input => {
                        ui.label(
                            egui::RichText::new("Input")
                                .color(egui::Color32::BLACK)
                                .size(30.0),
                        );
                        ui.add_space(18.0);
                        ui.label(
                            egui::RichText::new(
                                "Mouse, keyboard, and tablet input are sent to the remote \
                                 computer. Cursor rendering only chooses where the pointer is \
                                 drawn; it never turns remote mouse control off.",
                            )
                            .color(egui::Color32::DARK_GRAY),
                        );
                        ui.add_space(24.0);
                        ui.label(
                            egui::RichText::new("Pointer and cursor")
                                .color(egui::Color32::BLACK)
                                .size(19.0),
                        );
                        ui.add_space(10.0);
                        settings_radio(
                            ui,
                            &mut cursor_preference,
                            CursorMode::Local,
                            Some("(Recommended)"),
                            "Client-rendered cursor",
                            "Your mouse controls the remote computer while Arcen Deck draws the pointer \
                             locally for immediate response. Works with every host.",
                        );
                        settings_radio(
                            ui,
                            &mut cursor_preference,
                            CursorMode::Host,
                            None,
                            "Cursor in remote video",
                            "Your mouse still controls the remote computer, but the host draws the pointer \
                             into the captured video. This can feel less responsive and depends \
                             on the capture backend including the cursor.",
                        );
                        ui.add_space(24.0);
                        ui.label(
                            egui::RichText::new("Typing")
                                .color(egui::Color32::BLACK)
                                .size(19.0),
                        );
                        ui.add_space(10.0);
                        {
                            use arcen_input::text::{OptionKeyMode, OptionKeyPreference, TypingMode};
                            settings_radio(
                                ui,
                                &mut typing_mode,
                                TypingMode::Auto,
                                Some("(Recommended)"),
                                &format!("Characters as on {}", platform.this_machine()),
                                "Keys go to the remote computer as keys, except where its keyboard \
                                 layout would type a different character: those, and everything your \
                                 input method composes (accents, dead keys, Japanese, Chinese, emoji), \
                                 arrive as the characters you typed. Needs a host that types text.",
                            );
                            settings_radio(
                                ui,
                                &mut typing_mode,
                                TypingMode::Keys,
                                None,
                                "Keys only",
                                "Every key goes as a key; the remote keyboard layout decides each \
                                 character. Best for games. The input method does not compose.",
                            );
                            settings_radio(
                                ui,
                                &mut typing_mode,
                                TypingMode::Characters,
                                None,
                                "Always characters",
                                "Every typed character arrives as typed, whatever the remote layout. \
                                 Shortcuts still go as keys; held letter keys do not.",
                            );
                            ui.add_space(10.0);
                            if platform == DeckPlatform::Mac {
                                settings_radio(
                                    ui,
                                    &mut option_key_mode,
                                    OptionKeyPreference::Automatic,
                                    Some("(Recommended)"),
                                    "Automatic",
                                    "On a Mac host both Option keys stay Option, as on this Mac. On \
                                     other hosts left Option is Alt and right Option types this \
                                     Mac's special characters.",
                                );
                                settings_radio(
                                    ui,
                                    &mut option_key_mode,
                                    OptionKeyPreference::Chosen(OptionKeyMode::LeftAltRightCharacters),
                                    None,
                                    "Left Option is Alt, right Option types characters",
                                    "Left Option sends Alt shortcuts to the remote computer; right Option \
                                     types the special characters it types on this Mac.",
                                );
                                settings_radio(
                                    ui,
                                    &mut option_key_mode,
                                    OptionKeyPreference::Chosen(OptionKeyMode::BothAlt),
                                    None,
                                    "Both Option keys are Alt",
                                    "Both Option keys send Alt shortcuts.",
                                );
                                settings_radio(
                                    ui,
                                    &mut option_key_mode,
                                    OptionKeyPreference::Chosen(OptionKeyMode::BothCharacters),
                                    None,
                                    "Both Option keys type characters",
                                    "Both Option keys type this Mac's special characters.",
                                );
                            } else {
                                // A PC keyboard has Alt and AltGr, not two
                                // Option keys (PARITY Q-4).
                                settings_radio(
                                    ui,
                                    &mut option_key_mode,
                                    OptionKeyPreference::Automatic,
                                    Some("(Recommended)"),
                                    "AltGr types characters",
                                    "Alt sends Alt shortcuts to the remote computer; AltGr types \
                                     the special characters it types on this PC.",
                                );
                                settings_radio(
                                    ui,
                                    &mut option_key_mode,
                                    OptionKeyPreference::Chosen(OptionKeyMode::BothAlt),
                                    None,
                                    "AltGr is Alt",
                                    "Both Alt keys send Alt shortcuts.",
                                );
                            }
                            ui.add_space(10.0);
                            ui.label(
                                egui::RichText::new(format!(
                                    "The Connection menu's Keyboard: Characters ({}) switches a session \
                                     to characters and Physical Keys ({}) back; a saved \
                                     connection can start in either.",
                                    super::wording::chord(platform, DeckShortcut::KeyboardCharacters),
                                    super::wording::chord(platform, DeckShortcut::KeyboardPhysical),
                                ))
                                .color(egui::Color32::DARK_GRAY)
                                .size(12.0),
                            );
                        }
                        ui.add_space(24.0);
                        ui.label(
                            egui::RichText::new("Paste as Keystrokes")
                                .color(egui::Color32::BLACK)
                                .size(19.0),
                        );
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(format!(
                                "The Connection menu's Paste as Keystrokes ({}) types {} \
                                 clipboard text into the remote computer, for sign-in screens \
                                 and consoles that clipboard sharing cannot reach. Esc stops it.",
                                super::wording::chord(platform, DeckShortcut::PasteAsKeystrokes),
                                platform.this_machines(),
                            ))
                            .color(egui::Color32::DARK_GRAY)
                            .size(12.0),
                        );
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.label("Typing speed");
                            egui::ComboBox::from_id_salt("paste_chars_per_second")
                                .selected_text(format!(
                                    "{} characters a second",
                                    paste_settings.chars_per_second
                                ))
                                .show_ui(ui, |ui| {
                                    for rate in arcen_input::text::paste::RATE_CHOICES {
                                        let label = if rate
                                            == arcen_input::text::paste::DEFAULT_CHARS_PER_SECOND
                                        {
                                            format!("{rate} characters a second (default)")
                                        } else {
                                            format!("{rate} characters a second")
                                        };
                                        ui.selectable_value(
                                            &mut paste_settings.chars_per_second,
                                            rate,
                                            label,
                                        );
                                    }
                                });
                        });
                        ui.horizontal(|ui| {
                            ui.label("Ask first above");
                            egui::ComboBox::from_id_salt("paste_confirm_above")
                                .selected_text(format!(
                                    "{} characters",
                                    paste_settings.confirm_above
                                ))
                                .show_ui(ui, |ui| {
                                    for chars in arcen_input::text::paste::CONFIRM_CHOICES {
                                        ui.selectable_value(
                                            &mut paste_settings.confirm_above,
                                            chars,
                                            format!("{chars} characters"),
                                        );
                                    }
                                });
                        });
                        ui.label(
                            egui::RichText::new(
                                "Slow the speed down for virtual machine consoles and older \
                                 applications that drop keys.",
                            )
                            .color(egui::Color32::GRAY)
                            .size(11.0),
                        );
                        ui.add_space(24.0);
                        // One surface owns every input decision. Tablet mode
                        // and non-HID USB forwarding used to live in two
                        // different tabs, and the tab named for USB was the
                        // one that did *not* control the tablet bridge.
                        ui.label(
                            egui::RichText::new("Tablet")
                                .color(egui::Color32::BLACK)
                                .size(19.0),
                        );
                        ui.label(
                            egui::RichText::new(facts.tablet_scope_note.as_str())
                                .color(egui::Color32::GRAY)
                                .size(11.0),
                        );
                        ui.add_space(10.0);
                        settings_radio(
                            ui,
                            &mut tablet_mode_requested,
                            TabletModeMsg::LocalTermination,
                            Some("(Recommended for WAN)"),
                            tablet_mode_label(TabletModeMsg::LocalTermination),
                            &tablet_mode_description(TabletModeMsg::LocalTermination, platform),
                        );
                        // The helper control sits on the option's own row and
                        // states where the helper stands, because that is the
                        // one prerequisite the user can act on from here. It
                        // used to be a parenthetical badge plus a block of
                        // buttons that appeared only once the helper was
                        // *already* missing, which put the status and the fix
                        // in two different places and left an installed helper
                        // with nothing to click. Read once per settings visit,
                        // not per frame: each read is an XPC round trip to
                        // `smd`.
                        if let Some(control) = &facts.native_tablet {
                            settings_radio_with_trailing(
                                ui,
                                &mut tablet_mode_requested,
                                TabletModeMsg::UsbTabletBridge,
                                None,
                                tablet_mode_label(TabletModeMsg::UsbTabletBridge),
                                &tablet_mode_description(TabletModeMsg::UsbTabletBridge, platform),
                                |ui| {
                                    let label = egui::RichText::new(control.action_label.as_str())
                                        .color(control.tint.colour())
                                        .size(14.0);
                                    if ui
                                        .button(label)
                                        .on_hover_text(control.guidance.as_str())
                                        .clicked()
                                    {
                                        actions.push(SettingsAction::NativeTabletComponent);
                                    }
                                },
                            );
                            if !control.ready {
                                ui.horizontal(|ui| {
                                    ui.add_space(28.0);
                                    ui.label(
                                        egui::RichText::new(control.guidance.as_str())
                                            .color(egui::Color32::DARK_GRAY),
                                    );
                                });
                                ui.add_space(8.0);
                            }
                            if let Some(error) = &control.error {
                                ui.horizontal(|ui| {
                                    ui.add_space(28.0);
                                    ui.label(egui::RichText::new(error.as_str()).color(theme::DANGER));
                                });
                                ui.add_space(8.0);
                            }
                        } else {
                            settings_radio(
                                ui,
                                &mut tablet_mode_requested,
                                TabletModeMsg::UsbTabletBridge,
                                Some("(not available in this build)"),
                                tablet_mode_label(TabletModeMsg::UsbTabletBridge),
                                &tablet_mode_description(TabletModeMsg::UsbTabletBridge, platform),
                            );
                        }
                        // A bridged tablet moves the *host's* pointer, and a
                        // captured tablet no longer moves this Mac's. With
                        // Local cursor rendering the Deck draws the cursor
                        // where the host reports its pointer
                        // (`cursor_position_v1`), the recommended pairing; a
                        // host that cannot report it leaves the pen without a
                        // visible pointer, so host cursor rendering stays
                        // offered next to the choice.
                        if tablet_mode_requested == TabletModeMsg::UsbTabletBridge
                            && cursor_preference == CursorMode::Local
                        {
                            ui.horizontal(|ui| {
                                ui.add_space(28.0);
                                ui.label(
                                    egui::RichText::new(
                                        "With Local cursor rendering the pen's pointer is drawn \
                                         where the host reports it. If the host cannot report \
                                         its pointer, the pen has no visible pointer: use the \
                                         host cursor instead.",
                                    )
                                    .color(theme::TEXT_SECONDARY),
                                );
                            });
                            ui.horizontal(|ui| {
                                ui.add_space(28.0);
                                if ui.button("Use host cursor").clicked() {
                                    cursor_preference = CursorMode::Host;
                                }
                            });
                            ui.add_space(8.0);
                        }
                        settings_radio(
                            ui,
                            &mut tablet_mode_requested,
                            TabletModeMsg::DisabledMouseCompat,
                            None,
                            tablet_mode_label(TabletModeMsg::DisabledMouseCompat),
                            &tablet_mode_description(TabletModeMsg::DisabledMouseCompat, platform),
                        );
                        ui.add_space(10.0);
                        ui.label(
                            egui::RichText::new(
                                "Mode changes take effect on the next connection.\n\n\
                                 Native tablet hands the tablet itself to the host: a Linux \
                                 host's own driver claims it on a virtual USB controller, a \
                                 macOS or Windows host reads the pen itself, with no driver \
                                 installed. Tablet support needs nothing on the host and works \
                                 over any network.",
                            )
                            .color(egui::Color32::DARK_GRAY),
                        );
                        ui.add_space(24.0);
                    }
                    SettingsTab::Logs => {
                        ui.label(
                            egui::RichText::new("Logs").color(heading_col).size(30.0),
                        );
                        ui.add_space(18.0);
                        ui.label(
                            egui::RichText::new(
                                "Set the level of detail captured by the logs. Level 0 (Critical) \
                                 provides the least information and Level 3 (Debug) provides the \
                                 most information.",
                            )
                            .color(egui::Color32::DARK_GRAY),
                        );
                        ui.add_space(20.0);
                        ui.label(
                            egui::RichText::new("Log level")
                                .color(egui::Color32::DARK_GRAY)
                                .size(13.0),
                        );
                        egui::ComboBox::from_id_salt("log_level")
                            .selected_text(log_level.label())
                            .show_ui(ui, |ui| {
                                for level in LogLevel::ALL {
                                    ui.selectable_value(&mut log_level, level, level.label());
                                }
                            });
                        ui.add_space(20.0);
                        if ui.button("📂  Show in folder").clicked() {
                            actions.push(SettingsAction::ShowLogFolder);
                        }
                        if let Some(bundle) = &facts.support_bundle {
                            ui.add_space(12.0);
                            if ui
                                .add_enabled(
                                    !bundle.open,
                                    egui::Button::new("Create support bundle…"),
                                )
                                .clicked()
                            {
                                actions.push(SettingsAction::SupportBundle(
                                    super::support_bundle::SupportBundleAction::Open,
                                ));
                            }
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
                                .color(heading_col)
                                .size(30.0),
                        );
                        ui.add_space(18.0);
                        ui.label(
                            egui::RichText::new("Security modes")
                                .color(egui::Color32::BLACK)
                                .size(19.0),
                        );
                        ui.add_space(10.0);
                        ui.label(
                            egui::RichText::new(
                                "The Arcen client uses certificates to verify that the server \
                                 connection is secure. Select your desired security mode:",
                            )
                            .color(egui::Color32::DARK_GRAY),
                        );
                        ui.add_space(12.0);
                        settings_radio(
                            ui,
                            &mut security,
                            SecurityMode::High,
                            None,
                            "Security mode - High",
                            "Fully verified. You cannot connect unless the certificate is verified.",
                        );
                        settings_radio(
                            ui,
                            &mut security,
                            SecurityMode::Medium,
                            None,
                            "Security mode - Medium",
                            "If the certificate cannot be verified, you will get a warning, but can still connect.",
                        );
                        settings_radio(
                            ui,
                            &mut security,
                            SecurityMode::Low,
                            None,
                            "Security mode - Low",
                            "No certificate verification required to connect.",
                        );
                        if cfg!(feature = "dev-tools") {
                            ui.add_space(16.0);
                            egui::CollapsingHeader::new(
                                egui::RichText::new("Advanced: streaming overrides")
                                    .color(egui::Color32::BLACK)
                                    .size(16.0),
                            )
                            .id_salt("color_fidelity_advanced")
                            .default_open(false)
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new("Frame-rate ceiling")
                                        .color(egui::Color32::BLACK),
                                );
                                egui::ComboBox::from_id_salt("streaming_performance_override")
                                    .selected_text(match performance {
                                        PerformanceMode::Standard => "30 fps",
                                        PerformanceMode::High | PerformanceMode::HighLegacy => {
                                            "60 fps"
                                        }
                                    })
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(
                                            &mut performance,
                                            PerformanceMode::Standard,
                                            "30 fps",
                                        );
                                        ui.selectable_value(
                                            &mut performance,
                                            PerformanceMode::High,
                                            "60 fps",
                                        );
                                    });
                                ui.add_space(10.0);
                                ui.label(
                                    egui::RichText::new("Base colour contract")
                                        .color(egui::Color32::BLACK),
                                );
                                egui::ComboBox::from_id_salt("color_fidelity_preset_override")
                                    .selected_text(color_fidelity.preset.title())
                                    .show_ui(ui, |ui| {
                                        for preset in [
                                            ColorFidelity::Standard,
                                            ColorFidelity::FullColour,
                                            ColorFidelity::GradingReference,
                                            ColorFidelity::Hdr10,
                                        ] {
                                            ui.selectable_value(
                                                &mut color_fidelity.preset,
                                                preset,
                                                preset.title(),
                                            );
                                        }
                                    });
                                ui.label(
                                    egui::RichText::new(color_fidelity.preset.describe())
                                        .color(egui::Color32::DARK_GRAY),
                                );
                                ui.add_space(12.0);
                            ui.label(
                                egui::RichText::new(
                                    "Pin individual axes on top of the preset above. Each \
                                     defaults to \"Follow preset\", which is exactly the \
                                     preset's own value, so it stays invisible until you \
                                     explicitly pin it -- and a later preset change never \
                                     discards an axis you pinned here.",
                                )
                                .color(egui::Color32::DARK_GRAY),
                            );
                            ui.add_space(10.0);
                            ui.label(egui::RichText::new("Codec").color(egui::Color32::BLACK));
                            egui::ComboBox::from_id_salt("color_fidelity_codec_override")
                                .selected_text(color_fidelity.overrides.codec.map_or(
                                    "Follow preset".to_string(),
                                    |codec| color_fidelity_codec_label(codec).to_string(),
                                ))
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut color_fidelity.overrides.codec,
                                        None,
                                        "Follow preset",
                                    );
                                    for codec in arcen_media::VideoCodec::ALL.iter().copied() {
                                        ui.selectable_value(
                                            &mut color_fidelity.overrides.codec,
                                            Some(codec),
                                            color_fidelity_codec_label(codec),
                                        );
                                    }
                                });
                            ui.add_space(8.0);
                            ui.label(egui::RichText::new("Chroma").color(egui::Color32::BLACK));
                            egui::ComboBox::from_id_salt("color_fidelity_chroma_override")
                                .selected_text(color_fidelity.overrides.chroma.map_or(
                                    "Follow preset".to_string(),
                                    |chroma| color_fidelity_chroma_label(chroma).to_string(),
                                ))
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut color_fidelity.overrides.chroma,
                                        None,
                                        "Follow preset",
                                    );
                                    for chroma in
                                        arcen_media::ChromaSubsampling::ALL.iter().copied()
                                    {
                                        ui.selectable_value(
                                            &mut color_fidelity.overrides.chroma,
                                            Some(chroma),
                                            color_fidelity_chroma_label(chroma),
                                        );
                                    }
                                });
                            ui.add_space(8.0);
                            ui.label(
                                egui::RichText::new("Bit depth").color(egui::Color32::BLACK),
                            );
                            egui::ComboBox::from_id_salt("color_fidelity_bit_depth_override")
                                .selected_text(
                                    color_fidelity.overrides.bit_depth.map_or(
                                        "Follow preset".to_string(),
                                        color_fidelity_bit_depth_label,
                                    ),
                                )
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut color_fidelity.overrides.bit_depth,
                                        None,
                                        "Follow preset",
                                    );
                                    for bit_depth in arcen_media::BitDepth::ALL.iter().copied() {
                                        ui.selectable_value(
                                            &mut color_fidelity.overrides.bit_depth,
                                            Some(bit_depth),
                                            color_fidelity_bit_depth_label(bit_depth),
                                        );
                                    }
                                });
                            ui.add_space(8.0);
                            ui.label(
                                egui::RichText::new("Colour range").color(egui::Color32::BLACK),
                            );
                            egui::ComboBox::from_id_salt("color_fidelity_range_override")
                                .selected_text(color_fidelity.overrides.range.map_or(
                                    "Follow preset".to_string(),
                                    |range| color_fidelity_range_label(range).to_string(),
                                ))
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut color_fidelity.overrides.range,
                                        None,
                                        "Follow preset",
                                    );
                                    for range in arcen_media::ColorRange::ALL.iter().copied() {
                                        ui.selectable_value(
                                            &mut color_fidelity.overrides.range,
                                            Some(range),
                                            color_fidelity_range_label(range),
                                        );
                                    }
                                });
                            ui.add_space(8.0);
                            ui.label(egui::RichText::new("Matrix").color(egui::Color32::BLACK));
                            egui::ComboBox::from_id_salt("color_fidelity_matrix_override")
                                .selected_text(color_fidelity.overrides.matrix.map_or(
                                    "Follow preset".to_string(),
                                    |matrix| color_fidelity_matrix_label(matrix).to_string(),
                                ))
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut color_fidelity.overrides.matrix,
                                        None,
                                        "Follow preset",
                                    );
                                    for matrix in arcen_media::ColorMatrix::ALL.iter().copied() {
                                        ui.selectable_value(
                                            &mut color_fidelity.overrides.matrix,
                                            Some(matrix),
                                            color_fidelity_matrix_label(matrix),
                                        );
                                    }
                                });
                            });
                            ui.add_space(16.0);
                            ui.label(
                            egui::RichText::new("Variant (testing)")
                                .color(egui::Color32::BLACK)
                                .size(16.0),
                        );
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(
                                "Pin an exact probe-matrix row, bypassing the preset and \
                                 Advanced overrides above entirely. This is how the w0 testing \
                                 machine drives the colour-variation matrix directly. Leave at \
                                 None for ordinary use.",
                            )
                            .color(egui::Color32::DARK_GRAY),
                        );
                        ui.add_space(8.0);
                        egui::ComboBox::from_id_salt("color_fidelity_variant_override")
                            .selected_text(
                                color_fidelity
                                    .variant_override
                                    .map_or("None".to_string(), arcen_media::video::VideoVariant::describe),
                            )
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut color_fidelity.variant_override,
                                    None,
                                    "None",
                                );
                                for variant in arcen_media::video::PROBE_MATRIX.iter().copied() {
                                    ui.selectable_value(
                                        &mut color_fidelity.variant_override,
                                        Some(variant),
                                        format!("{} ({})", variant.describe(), variant.id()),
                                    );
                                }
                            });
                        ui.add_space(16.0);
                        let color_fidelity_resolved = color_fidelity.resolved_variant();
                        let color_fidelity_effective =
                            effective_color_fidelity_variant(color_fidelity);
                        ui.label(
                            egui::RichText::new(format!(
                                "Resolving to: {}",
                                color_fidelity_effective.describe()
                            ))
                            .color(egui::Color32::BLACK)
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
                                    .color(theme::DANGER),
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
        exact_still_image,
    };
    actions
}

/// The few phrases that name the machine, its operating system or its keys.
mod words {
    use super::DeckPlatform;

    pub(super) const fn os_name(platform: DeckPlatform) -> &'static str {
        match platform {
            DeckPlatform::Mac => "macOS",
            DeckPlatform::Pc => "Windows",
        }
    }

    pub(super) const fn applications(platform: DeckPlatform) -> &'static str {
        match platform {
            DeckPlatform::Mac => "Mac applications",
            DeckPlatform::Pc => "Windows applications",
        }
    }

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

    #[test]
    fn the_mac_spells_its_chords_as_its_menus_do() {
        let mac = |shortcut| super::super::wording::chord(DeckPlatform::Mac, shortcut);
        assert_eq!(mac(DeckShortcut::KeyboardCharacters), "Control-Command-U");
        assert_eq!(mac(DeckShortcut::KeyboardPhysical), "Control-Command-K");
        assert_eq!(
            mac(DeckShortcut::PasteAsKeystrokes),
            "Control-Option-Command-V"
        );
    }

    #[test]
    fn the_pc_transliterates_and_keeps_its_own_fullscreen_and_quit() {
        let pc = |shortcut| super::super::wording::chord(DeckPlatform::Pc, shortcut);
        assert_eq!(pc(DeckShortcut::KeyboardCharacters), "Ctrl+Win+U");
        assert_eq!(pc(DeckShortcut::PasteAsKeystrokes), "Ctrl+Alt+Win+V");
        assert_eq!(pc(DeckShortcut::Disconnect), "Ctrl+Alt+F12");
        assert_eq!(pc(DeckShortcut::ToggleFullscreen), "Ctrl+Alt+Break");
        assert_eq!(pc(DeckShortcut::Quit), "Alt+F4");
    }

    #[test]
    fn the_mac_tablet_wording_is_unchanged_and_the_pc_names_itself() {
        let mac = tablet_mode_description(TabletModeMsg::LocalTermination, DeckPlatform::Mac);
        assert!(mac.contains("This Mac's tablet driver reads the pen"));
        assert!(mac.contains("stay on this Mac. The tablet also stays usable in Mac applications"));
        let pc = tablet_mode_description(TabletModeMsg::UsbTabletBridge, DeckPlatform::Pc);
        assert!(pc.contains("takes the device away from Windows"));
        assert!(!pc.contains("Mac"));
    }

    /// Q-16 and Q-17: any tablet works in both modes, Tablet support follows
    /// the Deck's own pen settings, and Native tablet states per host what
    /// differs, on both Decks alike.
    #[test]
    fn any_tablet_works_and_each_host_says_what_differs() {
        for platform in [DeckPlatform::Mac, DeckPlatform::Pc] {
            let native = tablet_mode_description(TabletModeMsg::UsbTabletBridge, platform);
            assert!(native.contains(
                "Any pen tablet works; models with a dedicated reader get extra features."
            ));
            assert!(!native.contains("never interprets"));
            for note in NATIVE_TABLET_TARGET_NOTES {
                assert!(native.contains(note), "{note}");
            }
            let local = tablet_mode_description(TabletModeMsg::LocalTermination, platform);
            assert!(local.contains(&format!(
                "handedness and mapping follow {}'s own pen and tablet settings",
                platform.this_machine()
            )));
        }
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
            exact_still_image: true,
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
                clipboard_policy: None,
                tablet_scope_note: String::new(),
                native_tablet: None,
                decode: Some(report.clone()),
                support_bundle: None,
                connection_displays: None,
            };
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                assert!(render_settings(ui, &mut values, &facts).is_empty());
            });
            assert_eq!(values, before, "{platform:?}");
        }
    }

    #[test]
    fn every_panel_draws_on_both_decks_without_changing_a_value() {
        for platform in [DeckPlatform::Mac, DeckPlatform::Pc] {
            for tab in [
                SettingsTab::Language,
                SettingsTab::Streaming,
                SettingsTab::Displays,
                SettingsTab::Microphone,
                SettingsTab::Clipboard,
                SettingsTab::Input,
                SettingsTab::Logs,
                SettingsTab::Advanced,
            ] {
                let ctx = egui::Context::default();
                super::super::wording::set_deck_platform(&ctx, platform);
                let mut values = SettingsValues {
                    tab,
                    ..sample_values()
                };
                let before = values.clone();
                let facts = SettingsFacts {
                    hdr_screen: false,
                    notch: (platform == DeckPlatform::Mac).then_some(false),
                    monitors: &[],
                    clipboard_policy: None,
                    tablet_scope_note: String::new(),
                    native_tablet: None,
                    decode: None,
                    support_bundle: Some(super::super::support_bundle::SupportBundleView {
                        open: true,
                        ..super::super::support_bundle::SupportBundleView::default()
                    }),
                    connection_displays: Some(("Studio".to_owned(), DisplaysMode::MatchLayout)),
                };
                let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                    assert!(render_settings(ui, &mut values, &facts).is_empty());
                });
                assert_eq!(values, before, "{tab:?} on {platform:?}");
            }
        }
    }
}
