// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Home: the saved-connection list, the selected connection's detail panel
//! and the add/edit connection form. The shell owns the state and maps each
//! [`HomeAction`].
// ── Sidebar geometry ─────────────────────────────────────────────────────────
const SIDEBAR_W: f32 = 210.0;
const SIDEBAR_ROW_H: f32 = 56.0;
const SIDEBAR_BG: egui::Color32 = egui::Color32::from_rgb(0xFA, 0xF7, 0xF1);
const SIDEBAR_BORDER: egui::Color32 = egui::Color32::from_rgb(0xCB, 0xC5, 0xB9);
const SIDEBAR_SEL: egui::Color32 = egui::Color32::from_rgb(0xC9, 0x78, 0x50);
const SIDEBAR_SEL_TEXT: egui::Color32 = egui::Color32::WHITE;
const SIDEBAR_TEXT: egui::Color32 = egui::Color32::from_rgb(0x20, 0x22, 0x20);
const SECTION_COLOR: egui::Color32 = egui::Color32::from_rgb(0x8F, 0x8D, 0x86);

// ── Validate a username string (mirrors the app-level rule) ──────────────────
fn validate_username(username: &str) -> Option<String> {
    let u = username.trim();
    (!u.is_empty() && u.len() <= 255 && !u.chars().any(char::is_control)).then(|| u.to_string())
}

pub use crate::config::{
    ConnectionKind, SavedConnectionSettings, SavedConnectionSummary, SavedTransport,
};
pub use crate::connection_draft::ConnectionDraft;

/// All the transient state the Home screen reads and writes. The caller
/// (app.rs `ArcenApp`) owns all the underlying fields; this struct is a
/// short-lived borrow assembled each frame.
#[derive(Debug)]
pub struct HomeViewState<'a> {
    /// Working draft for the selected connection (holds username/password).
    pub direct: &'a mut ConnectionDraft,
    pub saved_connections: &'a [SavedConnectionSummary],
    pub search: &'a mut String,
    pub operator_notice: Option<&'a str>,
    /// `true` → show the "Add connection" form in the detail panel.
    pub show_add_form: &'a mut bool,
    /// `Some(i)` → show the "Edit connection" form for saved index `i`.
    pub editing_index: &'a mut Option<usize>,
    // Add / edit form field buffers
    pub form_name: &'a mut String,
    pub form_host: &'a mut String,
    pub form_port: &'a mut String,
    pub form_username: &'a mut String,
    /// Working value for the form's Displays selector. `None` renders as
    /// "Use app default".
    pub form_displays_mode: &'a mut Option<crate::settings::DisplaysMode>,
    /// Working value for the form's Keyboard settings.
    pub form_keyboard: &'a mut arcen_input::keyboard::KeyboardSettings,
    /// Settings › Displays, which a connection without its own Displays
    /// choice connects with (the panel says which one wins).
    pub app_displays_mode: crate::settings::DisplaysMode,
    /// Connection error to show under the Connect button.
    pub connection_error: Option<&'a str>,
    pub remember_username: &'a mut bool,
    /// The connection test shown in the editor or detail panel, if any.
    pub connection_test: Option<&'a super::connection_test::ConnectionTest>,
}

#[derive(Debug, Clone)]
pub enum HomeAction {
    /// Open the "Add connection" form in the detail panel.
    BeginAdd,
    /// Connect using the supplied draft (may carry a password).
    Connect(ConnectionDraft),
    /// Select a saved connection without connecting yet (populate detail panel).
    SelectConnection(usize),
    /// Open the edit form for a saved connection.
    Edit(usize),
    /// Delete a saved connection.
    Delete(usize),
    /// Confirm a new-connection form.
    SaveNew {
        name: String,
        host: String,
        port: String,
        username: String,
        /// This connection's own Displays choice, or `None` to follow the
        /// application default.
        displays_mode: Option<crate::settings::DisplaysMode>,
        /// This connection's keyboard settings.
        keyboard: arcen_input::keyboard::KeyboardSettings,
    },
    /// Confirm an edit-connection form.
    SaveEdit {
        index: usize,
        name: String,
        host: String,
        port: String,
        username: String,
        /// This connection's own Displays choice, or `None` to follow the
        /// application default.
        displays_mode: Option<crate::settings::DisplaysMode>,
        /// This connection's keyboard settings.
        keyboard: arcen_input::keyboard::KeyboardSettings,
    },
    /// Cancel the add/edit form.
    CancelForm,
    /// Hand a saved connection's Displays choice back to Settings ›
    /// Displays ("Use Settings" on its panel).
    FollowAppDisplays(usize),
    /// Run the connection test against this address. Sends no credentials.
    TestConnection { host: String, port: String },
    /// A button in the connection test's report.
    ConnectionTest(super::connection_test::ConnectionTestAction),
}

/// What the shell must do after [`apply_home_action`].
#[derive(Debug)]
pub enum HomeEffect {
    None,
    /// Start the connection flow with this draft (it may carry a password).
    Connect(ConnectionDraft),
    /// Run the connection test against this address.
    TestConnection {
        host: String,
        port: String,
    },
    /// A button in the connection test's report.
    ConnectionTest(super::connection_test::ConnectionTestAction),
}

/// The result of [`apply_home_action`]: the effect, the saved connections
/// when they changed (the shell reconciles them) and the status line.
#[derive(Debug)]
pub struct HomeOutcome {
    pub effect: HomeEffect,
    pub store: Option<crate::settings::SavedConnectionsStore>,
    pub status: Option<String>,
}

impl HomeOutcome {
    const fn effect(effect: HomeEffect) -> Self {
        Self {
            effect,
            store: None,
            status: None,
        }
    }
}

/// Applies a Home action to the form state and the saved connections. Both
/// Decks run this; only the effects (connect, test) are the shell's.
pub fn apply_home_action(
    state: &mut HomeViewState<'_>,
    action: HomeAction,
    repository: &crate::settings::ClientConfigRepository,
    security_mode: crate::settings::SecurityMode,
) -> HomeOutcome {
    match action {
        HomeAction::TestConnection { host, port } => {
            HomeOutcome::effect(HomeEffect::TestConnection { host, port })
        }
        HomeAction::ConnectionTest(panel) => HomeOutcome::effect(HomeEffect::ConnectionTest(panel)),
        HomeAction::BeginAdd => {
            *state.editing_index = None;
            state.form_host.clear();
            state.form_name.clear();
            state.form_username.clear();
            // A brand-new connection starts on the application default
            // rather than inheriting whichever connection was edited last.
            *state.form_displays_mode = None;
            *state.form_keyboard = arcen_input::keyboard::KeyboardSettings::default();
            *state.form_port = ConnectionKind::DirectMachine.default_port().to_string();
            *state.show_add_form = true;
            HomeOutcome::effect(HomeEffect::None)
        }
        HomeAction::SelectConnection(index) => {
            let mut outcome = HomeOutcome::effect(HomeEffect::None);
            if let Some(saved) = state.saved_connections.get(index) {
                // The error is the last attempt's; it does not follow the
                // selection to another connection.
                if state.connection_error.is_some()
                    && state.direct.saved_connection_id.as_deref() != Some(saved.id.as_str())
                {
                    outcome.status = Some(String::new());
                }
                state.direct.saved_connection_id = Some(saved.id.clone());
                state.direct.kind = saved.kind;
                state.direct.host.clone_from(&saved.host);
                state.direct.port = if saved.port != 0 {
                    saved.port
                } else {
                    saved.kind.default_port()
                };
                state.direct.use_tls = saved.use_tls;
                state.direct.username = saved
                    .settings
                    .remembered_username
                    .clone()
                    .unwrap_or_default();
                state.direct.password.clear();
                *state.show_add_form = false;
                *state.editing_index = None;
            }
            outcome
        }
        HomeAction::Connect(draft) => HomeOutcome::effect(HomeEffect::Connect(draft)),
        HomeAction::Edit(index) => {
            if let Some(saved) = state.saved_connections.get(index) {
                state.form_host.clone_from(&saved.host);
                state.form_name.clone_from(&saved.name);
                let port = if saved.port != 0 {
                    saved.port
                } else {
                    saved.kind.default_port()
                };
                *state.form_port = port.to_string();
                *state.form_username = saved
                    .settings
                    .remembered_username
                    .clone()
                    .unwrap_or_default();
                *state.form_displays_mode = saved.settings.displays_mode;
                *state.form_keyboard = saved.settings.keyboard;
                *state.editing_index = Some(index);
                *state.show_add_form = false;
            }
            HomeOutcome::effect(HomeEffect::None)
        }
        HomeAction::Delete(index) => {
            let mut outcome = HomeOutcome::effect(HomeEffect::None);
            if let Some(removed) = state.saved_connections.get(index).cloned() {
                match repository.delete_connection(&removed.id, &removed) {
                    Ok(store) => {
                        outcome.store = Some(store);
                        outcome.status = Some(format!("Removed {}", removed.name));
                        // Clear selection if we deleted the selected one
                        if state.direct.saved_connection_id.as_deref() == Some(removed.id.as_str())
                        {
                            state.direct.saved_connection_id = None;
                        }
                    }
                    Err(error) => {
                        outcome.status = Some(format!("Connection was not removed: {error}"));
                    }
                }
            }
            outcome
        }
        HomeAction::SaveNew {
            name,
            host,
            port,
            username,
            displays_mode,
            keyboard,
        } => {
            let mut outcome = HomeOutcome::effect(HomeEffect::None);
            let port_num = port
                .trim()
                .parse::<u16>()
                .ok()
                .filter(|v| *v != 0)
                .unwrap_or_else(|| ConnectionKind::DirectMachine.default_port());
            let id = crate::config::new_saved_connection_id(
                ConnectionKind::DirectMachine,
                &host,
                port_num,
                true,
                security_mode,
                &name,
                state.saved_connections.len(),
            );
            let connection = SavedConnectionSummary {
                id: id.clone(),
                kind: ConnectionKind::DirectMachine,
                name: name.clone(),
                host: host.clone(),
                port: port_num,
                use_tls: true,
                transport: SavedTransport::Quic,
                identity_security_mode: Some(security_mode.key().to_string()),
                settings: SavedConnectionSettings {
                    remembered_username: crate::config::validated_remembered_username(&username),
                    displays_mode,
                    keyboard,
                    ..SavedConnectionSettings::default()
                },
                unknown: serde_json::Map::new(),
            };
            match repository.add_connection(connection) {
                Ok(store) => {
                    outcome.store = Some(store);
                    // Auto-select the newly added connection
                    state.direct.saved_connection_id = Some(id);
                    state.direct.host = host;
                    state.direct.port = port_num;
                    state.direct.username =
                        crate::config::validated_remembered_username(&username).unwrap_or_default();
                    state.direct.password.clear();
                    outcome.status = Some(format!("Added {name}"));
                    *state.show_add_form = false;
                    *state.editing_index = None;
                }
                Err(error) => {
                    outcome.status = Some(format!("Connection was not saved: {error}"));
                }
            }
            outcome
        }
        HomeAction::SaveEdit {
            index,
            name,
            host,
            port,
            username,
            displays_mode,
            keyboard,
        } => {
            let mut outcome = HomeOutcome::effect(HomeEffect::None);
            if let Some(original) = state.saved_connections.get(index).cloned() {
                let port_num = port
                    .trim()
                    .parse::<u16>()
                    .ok()
                    .filter(|v| *v != 0)
                    .unwrap_or(original.port);
                let mut updated = original.clone();
                updated.name.clone_from(&name);
                updated.host = host;
                updated.port = port_num;
                updated.settings.remembered_username =
                    crate::config::validated_remembered_username(&username);
                updated.settings.unknown.remove("remembered_username");
                updated.settings.displays_mode = displays_mode;
                updated.settings.keyboard = keyboard;
                match repository.update_connection(&original.id, &original, updated) {
                    Ok(store) => {
                        outcome.store = Some(store);
                        outcome.status = Some(format!("Updated {name}"));
                        *state.editing_index = None;
                        *state.show_add_form = false;
                    }
                    Err(error) => {
                        outcome.status = Some(format!("Connection was not saved: {error}"));
                    }
                }
            }
            outcome
        }
        HomeAction::CancelForm => {
            *state.show_add_form = false;
            *state.editing_index = None;
            state.form_host.clear();
            state.form_name.clear();
            state.form_username.clear();
            state.form_port.clear();
            HomeOutcome::effect(HomeEffect::None)
        }
        HomeAction::FollowAppDisplays(index) => {
            let mut outcome = HomeOutcome::effect(HomeEffect::None);
            if let Some(original) = state.saved_connections.get(index).cloned() {
                let mut updated = original.clone();
                updated.settings.displays_mode = None;
                match repository.update_connection(&original.id, &original, updated) {
                    Ok(store) => {
                        outcome.store = Some(store);
                        outcome.status = Some(format!(
                            "\u{201c}{}\u{201d} now connects with Settings \u{203a} Displays: {}",
                            original.name,
                            state.app_displays_mode.label()
                        ));
                    }
                    Err(error) => {
                        outcome.status = Some(format!("Connection was not saved: {error}"));
                    }
                }
            }
            outcome
        }
    }
}

// ── Main entry point ─────────────────────────────────────────────────────────

pub fn render_home(ui: &mut egui::Ui, state: &mut HomeViewState<'_>) -> Option<HomeAction> {
    let mut action = None;

    render_operator_notice(ui, state.operator_notice);

    let selected_index = state
        .direct
        .saved_connection_id
        .as_ref()
        .and_then(|sel_id| state.saved_connections.iter().position(|c| &c.id == sel_id));
    let error_under =
        connection_error_owner(selected_index, *state.show_add_form, *state.editing_index);
    if error_under.is_none() {
        render_connection_error_banner(ui, state.connection_error);
    }

    // Capture available size *after* any notice, then allocate a fixed rect
    // so the sidebar + detail panel fill the entire remaining area.
    let available = ui.available_size();

    ui.allocate_ui_with_layout(
        available,
        egui::Layout::left_to_right(egui::Align::TOP),
        |ui| {
            let search_needle = state.search.trim().to_lowercase();
            // No stand-in connection while an error belongs to none: its
            // panel would connect with the failed attempt's draft, and
            // selecting it would move the error under it (`--connect`).
            let fallback_index = if selected_index.is_none()
                && !*state.show_add_form
                && state.editing_index.is_none()
                && state.connection_error.is_none()
            {
                first_visible_connection_index(state.saved_connections, &search_needle)
            } else {
                None
            };

            // ── Left sidebar ──────────────────────────────────────────────
            if let Some(a) = sidebar(ui, state, available.y) {
                action = Some(a);
            }

            // ── Right detail panel ────────────────────────────────────────
            let detail_action = if *state.show_add_form {
                connection_form(ui, state, None)
            } else if let Some(edit_idx) = *state.editing_index {
                connection_form(ui, state, Some(edit_idx))
            } else if let Some(i) = selected_index.or(fallback_index) {
                detail_connection(ui, state, i, error_under == Some(i))
            } else {
                detail_empty(ui);
                None
            };
            if let Some(a) = detail_action {
                action = Some(a);
            }
            if action.is_none()
                && let Some(i) = fallback_index
            {
                action = Some(HomeAction::SelectConnection(i));
            }
        },
    );

    action
}

// ── Sidebar ──────────────────────────────────────────────────────────────────

fn sidebar(ui: &mut egui::Ui, state: &mut HomeViewState<'_>, panel_h: f32) -> Option<HomeAction> {
    let mut action = None;

    egui::Frame::new()
        .fill(SIDEBAR_BG)
        .stroke(egui::Stroke::new(1.0, SIDEBAR_BORDER))
        .inner_margin(egui::Margin::ZERO)
        .show(ui, |ui| {
            ui.set_width(SIDEBAR_W);
            ui.set_min_height(panel_h);

            ui.vertical(|ui| {
                ui.set_width(SIDEBAR_W);
                let sidebar_top_y = ui.cursor().min.y;

                // Header + search
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    ui.label(
                        egui::RichText::new("Connections")
                            .strong()
                            .color(SIDEBAR_TEXT)
                            .size(13.0),
                    );
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    ui.scope(|ui| {
                        ui.style_mut().spacing.interact_size.y = 28.0;
                        ui.add_sized(
                            [SIDEBAR_W - 16.0, 28.0],
                            egui::TextEdit::singleline(state.search)
                                .vertical_align(egui::Align::Center)
                                .margin(egui::Margin::symmetric(6, 4))
                                .hint_text("Search…")
                                .font(egui::TextStyle::Small),
                        );
                    });
                });
                ui.add_space(10.0);

                let needle = state.search.trim().to_lowercase();

                let has_direct = state.saved_connections.iter().any(|c| {
                    c.kind == ConnectionKind::DirectMachine && connection_matches(c, &needle)
                });

                if has_direct {
                    sidebar_section_label(ui, "DIRECT");
                    for (idx, saved) in state.saved_connections.iter().enumerate() {
                        if saved.kind != ConnectionKind::DirectMachine {
                            continue;
                        }
                        if !connection_matches(saved, &needle) {
                            continue;
                        }
                        let sel =
                            state.direct.saved_connection_id.as_deref() == Some(saved.id.as_str());
                        if let Some(a) = sidebar_row(ui, idx, saved, sel) {
                            action = Some(a);
                        }
                    }
                }

                if state.saved_connections.is_empty() {
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        ui.add_space(12.0);
                        ui.label(
                            egui::RichText::new("No saved connections yet.")
                                .color(SECTION_COLOR)
                                .size(12.0),
                        );
                    });
                } else if !has_direct {
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        ui.add_space(12.0);
                        ui.label(
                            egui::RichText::new("No matches.")
                                .color(SECTION_COLOR)
                                .size(12.0),
                        );
                    });
                }

                // Spacer + pinned Add Connection footer
                let used = ui.cursor().min.y - sidebar_top_y;
                let footer_h = 62.0;
                let remaining = panel_h - used - footer_h;
                if remaining > 0.0 {
                    ui.add_space(remaining);
                }
                ui.separator();
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    if ui
                        .add_sized(
                            [SIDEBAR_W - 16.0, 34.0],
                            egui::Button::new(
                                egui::RichText::new("+  Add Connection")
                                    .size(13.5)
                                    .color(egui::Color32::WHITE),
                            )
                            .fill(SIDEBAR_SEL)
                            .stroke(egui::Stroke::NONE)
                            .corner_radius(8.0),
                        )
                        .clicked()
                    {
                        action = Some(HomeAction::BeginAdd);
                    }
                });
                ui.add_space(10.0);
            }); // end vertical
        });

    action
}

fn sidebar_section_label(ui: &mut egui::Ui, label: &str) {
    ui.horizontal(|ui| {
        ui.add_space(12.0);
        ui.label(
            egui::RichText::new(label)
                .color(SECTION_COLOR)
                .size(10.0)
                .strong(),
        );
    });
    ui.add_space(2.0);
}

fn sidebar_row(
    ui: &mut egui::Ui,
    index: usize,
    saved: &SavedConnectionSummary,
    selected: bool,
) -> Option<HomeAction> {
    let mut action = None;

    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(SIDEBAR_W, SIDEBAR_ROW_H), egui::Sense::click());

    if ui.is_rect_visible(rect) {
        let bg = if selected {
            SIDEBAR_SEL
        } else if resp.hovered() {
            egui::Color32::from_black_alpha(12)
        } else {
            egui::Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect, 0.0, bg);

        let name_color = if selected {
            SIDEBAR_SEL_TEXT
        } else {
            SIDEBAR_TEXT
        };
        let sub_color = if selected {
            egui::Color32::from_white_alpha(180)
        } else {
            SECTION_COLOR
        };

        // Kind icon
        paint_connection_kind_icon(
            ui.painter(),
            saved.kind,
            rect.left_center() + egui::vec2(18.0, 0.0),
            name_color,
        );

        // Connection name (upper line)
        let name = if saved.name.len() > 22 {
            format!("{}…", &saved.name[..20])
        } else {
            saved.name.clone()
        };
        ui.painter().text(
            rect.left_center() + egui::vec2(36.0, -9.0),
            egui::Align2::LEFT_CENTER,
            name,
            egui::FontId::proportional(14.0),
            name_color,
        );

        // Host (lower line, smaller)
        let host = if saved.host.len() > 24 {
            format!("{}…", &saved.host[..21])
        } else {
            saved.host.clone()
        };
        ui.painter().text(
            rect.left_center() + egui::vec2(36.0, 9.0),
            egui::Align2::LEFT_CENTER,
            host,
            egui::FontId::proportional(11.5),
            sub_color,
        );

        // Kebab (⋮) — three painted dots, right side
        let dot_x = rect.right() - 14.0;
        let dot_color = if selected {
            egui::Color32::from_white_alpha(180)
        } else {
            SECTION_COLOR
        };
        let dot_rect = egui::Rect::from_center_size(
            egui::pos2(dot_x, rect.center().y),
            egui::vec2(20.0, 36.0),
        );
        let dot_resp = ui.allocate_rect(dot_rect, egui::Sense::click());
        let active_dot = if dot_resp.hovered() {
            if selected {
                egui::Color32::WHITE
            } else {
                SIDEBAR_SEL
            }
        } else {
            dot_color
        };
        for dy in [-5.0_f32, 0.0, 5.0] {
            ui.painter()
                .circle_filled(egui::pos2(dot_x, rect.center().y + dy), 1.8, active_dot);
        }

        egui::Popup::menu(&dot_resp).gap(2.0).show(|ui| {
            ui.set_min_width(120.0);
            if ui.button(egui::RichText::new("Edit").size(14.0)).clicked() {
                action = Some(HomeAction::Edit(index));
                ui.close();
            }
            ui.separator();
            if ui
                .button(
                    egui::RichText::new("Delete")
                        .color(egui::Color32::from_rgb(0xC0, 0x20, 0x20))
                        .size(14.0),
                )
                .clicked()
            {
                action = Some(HomeAction::Delete(index));
                ui.close();
            }
        });
    }

    if resp.clicked() && action.is_none() {
        action = Some(HomeAction::SelectConnection(index));
    }

    action
}

fn connection_matches(saved: &SavedConnectionSummary, needle: &str) -> bool {
    needle.is_empty()
        || saved.name.to_lowercase().contains(needle)
        || saved.host.to_lowercase().contains(needle)
}

fn first_visible_connection_index(
    saved_connections: &[SavedConnectionSummary],
    needle: &str,
) -> Option<usize> {
    saved_connections
        .iter()
        .enumerate()
        .find_map(|(idx, saved)| connection_matches(saved, needle).then_some(idx))
}

/// The detail panel's address line: "ip (host)" once the name has resolved,
/// the name alone until then or when it does not resolve.
///
/// Resolution never runs on the UI thread (R08): a lookup starts on its own
/// thread and wakes the UI when it lands. Answers expire, failures sooner,
/// and [`forget_resolved_addresses`] drops them all when the network changes.
fn display_address(ctx: &egui::Context, host: &str, port: u16) -> String {
    let host = host.trim();
    if host.parse::<std::net::IpAddr>().is_ok() {
        return host.to_string();
    }
    match resolver().lookup(host, port, std::time::Instant::now(), || {
        let ctx = ctx.clone();
        move || ctx.request_repaint()
    }) {
        Some(ip) => format!("{ip} ({host})"),
        None => host.to_string(),
    }
}

/// Drops every resolved address, so the next paint resolves again (a shell
/// calls it when the network or location changes).
pub fn forget_resolved_addresses() {
    resolver().clear();
}

fn resolver() -> &'static AddressResolver {
    static RESOLVER: std::sync::OnceLock<AddressResolver> = std::sync::OnceLock::new();
    RESOLVER.get_or_init(AddressResolver::default)
}

/// How long a resolved address is shown before it is looked up again.
const RESOLVED_TTL: std::time::Duration = std::time::Duration::from_secs(300);
/// How soon a name that did not resolve is tried again.
const UNRESOLVED_TTL: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, Clone)]
enum Resolution {
    /// A lookup is running; the previous answer stays up meanwhile.
    Pending(Option<String>),
    /// The answer, and when it arrived.
    Done(Option<String>, std::time::Instant),
}

#[derive(Debug, Default)]
struct ResolverState {
    entries: std::collections::HashMap<(String, u16), Resolution>,
    /// Bumped by [`AddressResolver::clear`]: a lookup that started before
    /// a network change does not write its answer back.
    generation: u64,
}

#[derive(Debug, Default)]
struct AddressResolver {
    state: std::sync::Arc<std::sync::Mutex<ResolverState>>,
}

impl AddressResolver {
    /// The cached answer for `host`, starting a background lookup when there
    /// is none or it has expired. At most one lookup per name runs at a time.
    fn lookup<W, F>(
        &self,
        host: &str,
        port: u16,
        now: std::time::Instant,
        waker: W,
    ) -> Option<String>
    where
        W: FnOnce() -> F,
        F: FnOnce() + Send + 'static,
    {
        let key = (host.to_string(), port);
        let mut state = self.state.lock().ok()?;
        let previous = match state.entries.get(&key) {
            Some(Resolution::Pending(previous)) => return previous.clone(),
            Some(Resolution::Done(answer, at)) => {
                let ttl = if answer.is_some() {
                    RESOLVED_TTL
                } else {
                    UNRESOLVED_TTL
                };
                if now.saturating_duration_since(*at) < ttl {
                    return answer.clone();
                }
                answer.clone()
            }
            None => None,
        };
        state
            .entries
            .insert(key.clone(), Resolution::Pending(previous.clone()));
        let generation = state.generation;
        drop(state);
        let shared = std::sync::Arc::clone(&self.state);
        let wake = waker();
        let spawned = std::thread::Builder::new()
            .name("arcen-home-resolve".to_owned())
            .spawn(move || {
                let answer = resolve_host_ip(&key.0, key.1);
                if let Ok(mut state) = shared.lock()
                    && state.generation == generation
                {
                    state
                        .entries
                        .insert(key, Resolution::Done(answer, std::time::Instant::now()));
                }
                wake();
            });
        if spawned.is_err()
            && let Ok(mut state) = self.state.lock()
        {
            state.entries.insert(
                (host.to_string(), port),
                Resolution::Done(None, std::time::Instant::now()),
            );
        }
        previous
    }

    fn clear(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.entries.clear();
            state.generation = state.generation.wrapping_add(1);
        }
    }
}

fn resolve_host_ip(host: &str, port: u16) -> Option<String> {
    std::net::ToSocketAddrs::to_socket_addrs(&(host, port))
        .ok()
        .and_then(|iter| {
            let ips: Vec<std::net::IpAddr> = iter.map(|socket| socket.ip()).collect();
            ips.iter()
                .copied()
                .find(std::net::IpAddr::is_ipv4)
                .or_else(|| ips.first().copied())
        })
        .map(|ip| ip.to_string())
}

// ── Detail panel ─────────────────────────────────────────────────────────────

fn detail_empty(ui: &mut egui::Ui) {
    ui.with_layout(
        egui::Layout::centered_and_justified(egui::Direction::TopDown),
        |ui| {
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(0xFB, 0xFB, 0xFD))
                .stroke(egui::Stroke::new(
                    1.0,
                    egui::Color32::from_rgb(0xE6, 0xE6, 0xEE),
                ))
                .corner_radius(12.0)
                .inner_margin(egui::Margin::symmetric(28, 22))
                .show(ui, |ui| {
                    ui.set_width(360.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new("Select a connection")
                                .color(egui::Color32::from_rgb(0x78, 0x78, 0x90))
                                .size(22.0),
                        );
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new("Choose a saved connection or add a new one.")
                                .color(egui::Color32::from_rgb(0x9A, 0x9A, 0xAF))
                                .size(14.0),
                        );
                    });
                });
        },
    );
}

fn detail_connection(
    ui: &mut egui::Ui,
    state: &mut HomeViewState<'_>,
    idx: usize,
    shows_connection_error: bool,
) -> Option<HomeAction> {
    let mut action = None;
    let saved = state.saved_connections[idx].clone();

    egui::ScrollArea::vertical()
        .id_salt("detail_scroll")
        .show(ui, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space(20.0);
                ui.vertical(|ui| {
                    egui::Frame::new()
                        .fill(egui::Color32::from_rgb(0xFC, 0xFC, 0xFE))
                        .stroke(egui::Stroke::new(
                            1.0,
                            egui::Color32::from_rgb(0xE6, 0xE6, 0xEE),
                        ))
                        .corner_radius(12.0)
                        .inner_margin(egui::Margin::symmetric(24, 16))
                        .show(ui, |ui| {
                            ui.set_max_width(500.0);

                            // ── Header ────────────────────────────────────────────
                            ui.with_layout(
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    let (icon_rect, _) = ui.allocate_exact_size(
                                        egui::vec2(22.0, 22.0),
                                        egui::Sense::hover(),
                                    );
                                    paint_connection_kind_icon(
                                        ui.painter(),
                                        saved.kind,
                                        icon_rect.center() + egui::vec2(0.0, 1.0),
                                        SIDEBAR_SEL,
                                    );
                                    ui.add_space(6.0);
                                    ui.heading(
                                        egui::RichText::new(&saved.name)
                                            .color(egui::Color32::BLACK)
                                            .size(30.0),
                                    );
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui
                                                .small_button(
                                                    egui::RichText::new("Edit").size(12.0),
                                                )
                                                .clicked()
                                            {
                                                action = Some(HomeAction::Edit(idx));
                                            }
                                        },
                                    );
                                },
                            );
                            ui.add_space(10.0);

                            // ── Connection info ────────────────────────────────────
                            let address = display_address(ui.ctx(), &saved.host, saved.port);
                            detail_kv(ui, "ADDRESS", &address);
                            if saved.port != saved.kind.default_port() {
                                detail_kv(ui, "PORT", &saved.port.to_string());
                            }
                            detail_kv(
                                ui,
                                "ENCRYPTION",
                                if saved.use_tls {
                                    "TLS (encrypted)"
                                } else {
                                    "Off (not encrypted)"
                                },
                            );
                            let (displays, overrides) =
                                crate::connect_plan::connection_displays_line(
                                    saved.settings.displays_mode,
                                    state.app_displays_mode,
                                );
                            ui.horizontal_wrapped(|ui| {
                                ui.add_sized(
                                    [72.0, 18.0],
                                    egui::Label::new(
                                        egui::RichText::new("DISPLAYS")
                                            .color(SECTION_COLOR)
                                            .size(10.0)
                                            .strong(),
                                    ),
                                );
                                ui.label(egui::RichText::new(displays).size(13.0).color(
                                    if overrides {
                                        egui::Color32::from_rgb(0xB0, 0x60, 0x00)
                                    } else {
                                        SIDEBAR_TEXT
                                    },
                                ));
                                if overrides
                                    && ui
                                        .small_button(
                                            egui::RichText::new(
                                                crate::connect_plan::CONNECTION_DISPLAYS_USE_SETTINGS,
                                            )
                                            .size(12.0),
                                        )
                                        .clicked()
                                {
                                    action = Some(HomeAction::FollowAppDisplays(idx));
                                }
                            });
                            ui.add_space(18.0);
                            ui.separator();
                            ui.add_space(14.0);

                            // ── Credentials ───────────────────────────────────────
                            ui.label(
                                egui::RichText::new("CREDENTIALS")
                                    .color(SECTION_COLOR)
                                    .size(10.0)
                                    .strong(),
                            );
                            ui.add_space(8.0);
                            detail_input(ui, "Username", &mut state.direct.username, false, 320.0);
                            ui.add_space(8.0);
                            detail_input(ui, "Password", &mut state.direct.password, true, 320.0);
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                ui.add_space(2.0);
                                ui.checkbox(
                                    state.remember_username,
                                    egui::RichText::new("Remember username").size(12.0),
                                );
                            });
                            ui.add_space(16.0);
                            ui.separator();
                            ui.add_space(12.0);

                            // ── Connection options ─────────────────────────────────
                            ui.checkbox(
                                &mut state.direct.auto_reconnect,
                                egui::RichText::new("Auto-reconnect").size(13.0),
                            );
                            ui.add_space(24.0);

                            // ── Error + Connect button ─────────────────────────────
                            if let Some(err) =
                                state.connection_error.filter(|_| shows_connection_error)
                            {
                                ui.label(
                                    egui::RichText::new(err)
                                        .color(egui::Color32::from_rgb(0xC0, 0x20, 0x20))
                                        .size(13.0),
                                );
                                ui.add_space(8.0);
                            }

                            let can_connect = !state.direct.username.is_empty()
                                && !state.direct.password.is_empty();
                            let btn_fill = if can_connect {
                                SIDEBAR_SEL
                            } else {
                                egui::Color32::from_rgb(0xE0, 0xE0, 0xE0)
                            };
                            let btn_text = if can_connect {
                                egui::Color32::WHITE
                            } else {
                                egui::Color32::from_rgb(0xA0, 0xA0, 0xA0)
                            };

                            if ui
                                .add_sized(
                                    [220.0, 46.0],
                                    egui::Button::new(
                                        egui::RichText::new("Connect").color(btn_text).size(15.0),
                                    )
                                    .fill(btn_fill)
                                    .stroke(egui::Stroke::NONE)
                                    .corner_radius(8.0),
                                )
                                .clicked()
                            {
                                let mut draft = state.direct.clone();
                                draft.saved_connection_id = Some(saved.id.clone());
                                draft.kind = saved.kind;
                                if draft.host.is_empty() {
                                    draft.host.clone_from(&saved.host);
                                }
                                if draft.port == 0 {
                                    draft.port = saved.port;
                                }
                                draft.use_tls = saved.use_tls;
                                action = Some(HomeAction::Connect(draft));
                            }

                            ui.add_space(10.0);
                            if let Some(test_action) =
                                connection_test_controls(ui, state.connection_test, true)
                            {
                                action = Some(match test_action {
                                    None => HomeAction::TestConnection {
                                        host: saved.host.clone(),
                                        port: saved.port.to_string(),
                                    },
                                    Some(panel) => HomeAction::ConnectionTest(panel),
                                });
                            }

                            ui.add_space(24.0);
                        });
                });
            });
        });

    action
}

fn paint_connection_kind_icon(
    painter: &egui::Painter,
    kind: ConnectionKind,
    center: egui::Pos2,
    color: egui::Color32,
) {
    match kind {
        ConnectionKind::DirectMachine => {
            // Desktop/host icon: monitor outline + stand.
            let w = 14.0;
            let h = 9.0;
            let left = center.x - w * 0.5;
            let right = center.x + w * 0.5;
            let top = center.y - h * 0.5 - 2.0;
            let bottom = center.y + h * 0.5 - 2.0;
            let stroke = egui::Stroke::new(1.4, color);
            painter.line_segment([egui::pos2(left, top), egui::pos2(right, top)], stroke);
            painter.line_segment([egui::pos2(right, top), egui::pos2(right, bottom)], stroke);
            painter.line_segment(
                [egui::pos2(right, bottom), egui::pos2(left, bottom)],
                stroke,
            );
            painter.line_segment([egui::pos2(left, bottom), egui::pos2(left, top)], stroke);
            painter.line_segment(
                [
                    egui::pos2(center.x, bottom),
                    egui::pos2(center.x, bottom + 3.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x - 4.0, bottom + 3.5),
                    egui::pos2(center.x + 4.0, bottom + 3.5),
                ],
                stroke,
            );
        }
    }
}

// ── Add / Edit form ───────────────────────────────────────────────────────────

fn connection_form(
    ui: &mut egui::Ui,
    state: &mut HomeViewState<'_>,
    editing_index: Option<usize>,
) -> Option<HomeAction> {
    let mut action = None;
    let is_edit = editing_index.is_some();

    egui::ScrollArea::vertical()
        .id_salt("form_scroll")
        .show(ui, |ui| {
            ui.add_space(24.0);
            ui.horizontal(|ui| {
                ui.add_space(30.0);
                ui.vertical(|ui| {
                    egui::Frame::new()
                        .fill(egui::Color32::from_rgb(0xFB, 0xFB, 0xFD))
                        .stroke(egui::Stroke::new(
                            1.0,
                            egui::Color32::from_rgb(0xE6, 0xE6, 0xEE),
                        ))
                        .corner_radius(12.0)
                        .inner_margin(egui::Margin::symmetric(24, 20))
                        .show(ui, |ui| {
                            ui.set_max_width(390.0);

                            ui.heading(
                                egui::RichText::new(if is_edit {
                                    "Edit Connection"
                                } else {
                                    "New Connection"
                                })
                                .color(egui::Color32::BLACK)
                                .size(24.0),
                            );
                            ui.add_space(22.0);

                            form_field(ui, "Name", state.form_name, false, 340.0);
                            ui.add_space(14.0);
                            form_field(ui, "Host Address", state.form_host, false, 340.0);
                            ui.add_space(14.0);
                            form_field(ui, "Port", state.form_port, false, 340.0);
                            ui.add_space(14.0);
                            form_field(
                                ui,
                                "Remembered Username (optional)",
                                state.form_username,
                                false,
                                340.0,
                            );

                            let username_ok = state.form_username.trim().is_empty()
                                || validate_username(state.form_username).is_some();
                            if !username_ok {
                                ui.add_space(4.0);
                                ui.label(
                                    egui::RichText::new(
                                        "Username must be ≤ 255 bytes with no control characters.",
                                    )
                                    .color(egui::Color32::from_rgb(0xC0, 0x20, 0x20))
                                    .size(12.0),
                                );
                            }

                            ui.add_space(14.0);
                            displays_field(ui, state.form_displays_mode);
                            ui.add_space(14.0);
                            keyboard_field(ui, state.form_keyboard);

                            ui.add_space(24.0);

                            let can_save = !state.form_name.trim().is_empty()
                                && !state.form_host.trim().is_empty()
                                && username_ok;

                            ui.horizontal(|ui| {
                                let save_fill = if can_save {
                                    // Brand accent, matching the sidebar
                                    // selection and the Add Connection footer.
                                    SIDEBAR_SEL
                                } else {
                                    egui::Color32::from_rgb(0xE0, 0xE0, 0xE0)
                                };
                                let save_text = if can_save {
                                    egui::Color32::WHITE
                                } else {
                                    egui::Color32::from_rgb(0xA0, 0xA0, 0xA0)
                                };

                                let btn_label = if is_edit {
                                    "Save Changes"
                                } else {
                                    "Add Connection"
                                };
                                let save = ui.add_sized(
                                    [170.0, 42.0],
                                    egui::Button::new(
                                        egui::RichText::new(btn_label).color(save_text).size(14.0),
                                    )
                                    .fill(save_fill)
                                    .stroke(egui::Stroke::NONE)
                                    .corner_radius(8.0),
                                );
                                if can_save && save.clicked() {
                                    let name = state.form_name.trim().to_string();
                                    let host = state.form_host.trim().to_string();
                                    let port = state.form_port.trim().to_string();
                                    let username = state.form_username.trim().to_string();
                                    action = Some(if let Some(idx) = editing_index {
                                        HomeAction::SaveEdit {
                                            index: idx,
                                            name,
                                            host,
                                            port,
                                            username,
                                            displays_mode: *state.form_displays_mode,
                                            keyboard: *state.form_keyboard,
                                        }
                                    } else {
                                        HomeAction::SaveNew {
                                            name,
                                            host,
                                            port,
                                            username,
                                            displays_mode: *state.form_displays_mode,
                                            keyboard: *state.form_keyboard,
                                        }
                                    });
                                }

                                ui.add_space(10.0);
                                // Same height as Save so the pair reads as one
                                // row; narrower and outlined so it still reads
                                // as the secondary action.
                                if ui
                                    .add_sized(
                                        [110.0, 42.0],
                                        egui::Button::new(
                                            egui::RichText::new("Cancel")
                                                .size(14.0)
                                                .color(egui::Color32::from_rgb(0x44, 0x44, 0x4A)),
                                        )
                                        .fill(egui::Color32::TRANSPARENT)
                                        .stroke(egui::Stroke::new(
                                            1.0,
                                            egui::Color32::from_rgb(0xC4, 0xC4, 0xCC),
                                        ))
                                        .corner_radius(8.0),
                                    )
                                    .clicked()
                                {
                                    action = Some(HomeAction::CancelForm);
                                }
                            });

                            ui.add_space(12.0);
                            let can_test = !state.form_host.trim().is_empty();
                            if let Some(test_action) =
                                connection_test_controls(ui, state.connection_test, can_test)
                            {
                                action = Some(match test_action {
                                    None => HomeAction::TestConnection {
                                        host: state.form_host.trim().to_string(),
                                        port: state.form_port.trim().to_string(),
                                    },
                                    Some(panel) => HomeAction::ConnectionTest(panel),
                                });
                            }

                            ui.add_space(24.0);
                        });
                });
            });
        });

    action
}

// ── Small helpers ─────────────────────────────────────────────────────────────

/// The **Test connection** button and, below it, the report. `Some(None)`
/// starts a test; `Some(Some(_))` is a button inside the report.
#[allow(
    clippy::option_option,
    reason = "the two levels are the two buttons the doc names"
)]
fn connection_test_controls(
    ui: &mut egui::Ui,
    test: Option<&super::connection_test::ConnectionTest>,
    enabled: bool,
) -> Option<Option<super::connection_test::ConnectionTestAction>> {
    let mut action = None;
    let running = test.is_some_and(super::connection_test::ConnectionTest::running);
    let button = ui
        .add_enabled(
            enabled && !running,
            egui::Button::new(egui::RichText::new("Test connection").size(13.0))
                .fill(egui::Color32::TRANSPARENT)
                .stroke(egui::Stroke::new(
                    1.0,
                    egui::Color32::from_rgb(0xC4, 0xC4, 0xCC),
                ))
                .corner_radius(8.0),
        )
        .on_hover_text(
            "Checks the name, UDP 18444, the certificate, the Pier's greeting, \
             path MTU and round trip. Sends no user name or password.",
        );
    if button.clicked() {
        action = Some(None);
    }
    if let Some(test) = test {
        ui.add_space(8.0);
        if let Some(panel) = super::connection_test::render(ui, test) {
            action = Some(Some(panel));
        }
    }
    action
}

/// Read-only label + value row (e.g. "HOST   workstation.local").
fn detail_kv(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [72.0, 18.0],
            egui::Label::new(
                egui::RichText::new(label)
                    .color(SECTION_COLOR)
                    .size(10.0)
                    .strong(),
            ),
        );
        ui.label(egui::RichText::new(value).color(SIDEBAR_TEXT).size(13.0));
    });
    ui.add_space(4.0);
}

/// Labelled text input for the detail connection panel.
fn detail_input(ui: &mut egui::Ui, label: &str, value: &mut String, password: bool, width: f32) {
    let row_h = 30.0;
    let label_w = 92.0;
    ui.horizontal(|ui| {
        ui.set_min_height(row_h);
        let field_w = (width - label_w - ui.spacing().item_spacing.x).max(120.0);
        // Vertically centred label.
        ui.allocate_ui_with_layout(
            egui::vec2(label_w, row_h),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.label(
                    egui::RichText::new(label)
                        .color(egui::Color32::from_rgb(0x88, 0x88, 0x88))
                        .size(12.0),
                );
            },
        );
        // Keep interact height tied to row height so text sits centered.
        ui.scope(|ui| {
            ui.style_mut().spacing.interact_size.y = row_h;
            ui.add_sized(
                [field_w, row_h],
                egui::TextEdit::singleline(value)
                    .vertical_align(egui::Align::Center)
                    .margin(egui::Margin::symmetric(6, 4))
                    .password(password)
                    .font(egui::TextStyle::Body),
            );
        });
    });
}

/// Labelled text input for the add/edit form.
/// The connection form's Displays selector.
///
/// "Use app default" is deliberately the first entry and the default value:
/// most connections should follow the application-wide preference, and only
/// a host that genuinely cannot serve it -- one advertising
/// `max_monitors: 1`, which rejects Match My Layout outright -- needs its own
/// pinned choice. Naming the neutral option after the app default (rather
/// than calling it an override) keeps the common case the obvious one.
fn displays_field(ui: &mut egui::Ui, value: &mut Option<crate::settings::DisplaysMode>) {
    use crate::settings::DisplaysMode;

    const FIELD_W: f32 = 340.0;

    ui.label(
        egui::RichText::new("Displays")
            .color(egui::Color32::DARK_GRAY)
            .size(12.0),
    );
    ui.add_space(3.0);
    let selected = value.map_or("Use app default", DisplaysMode::label);
    egui::ComboBox::from_id_salt("connection_displays_mode")
        .selected_text(selected)
        .width(FIELD_W)
        .show_ui(ui, |ui| {
            ui.selectable_value(value, None, "Use app default");
            for mode in DisplaysMode::ALL {
                ui.selectable_value(value, Some(mode), mode.label());
            }
        });
    ui.add_space(3.0);
    ui.label(
        egui::RichText::new(match value {
            None => "Follows the Displays setting in Settings.",
            Some(mode) => mode.description(),
        })
        .color(egui::Color32::GRAY)
        .size(11.0),
    );
}

/// The connection form's Keyboard settings: what Command and Control are on
/// the host, and whether left Option types like `AltGr` on a Windows host.
/// A PC keyboard has its own keys, so a PC Deck offers the PC choices and no
/// Option setting (PARITY KB-11, Q-3, Q-4).
fn keyboard_field(ui: &mut egui::Ui, value: &mut arcen_input::keyboard::KeyboardSettings) {
    use super::wording::DeckPlatform;
    use arcen_input::keyboard::ShortcutPreference;

    const FIELD_W: f32 = 340.0;
    let platform = super::wording::deck_platform(ui.ctx());
    let choices: &[ShortcutPreference] = match platform {
        DeckPlatform::Mac => &ShortcutPreference::ALL,
        DeckPlatform::Pc => &PC_SHORTCUT_CHOICES,
    };

    ui.label(
        egui::RichText::new(match platform {
            DeckPlatform::Mac => "Keyboard shortcuts",
            DeckPlatform::Pc => "Command key on a Mac host",
        })
        .color(egui::Color32::DARK_GRAY)
        .size(12.0),
    );
    ui.add_space(3.0);
    egui::ComboBox::from_id_salt("connection_keyboard_shortcuts")
        .selected_text(shortcut_label(value.shortcuts, platform))
        .width(FIELD_W)
        .show_ui(ui, |ui| {
            for preference in choices.iter().copied() {
                ui.selectable_value(
                    &mut value.shortcuts,
                    preference,
                    shortcut_label(preference, platform),
                );
            }
        });
    ui.add_space(3.0);
    ui.label(
        egui::RichText::new(shortcut_description(value.shortcuts, platform))
            .color(egui::Color32::GRAY)
            .size(11.0),
    );
    if platform == DeckPlatform::Mac {
        ui.add_space(6.0);
        ui.checkbox(
            &mut value.left_option_altgr,
            "Left Option is also AltGr (Windows hosts)",
        );
        ui.label(
            egui::RichText::new(
                // The Deck's font has no Option glyph (U+2325); it would draw a box.
                "Right Option is always AltGr on a Windows host, so Option+2 types @ on a \
                 Norwegian layout. Tick this to type those characters with left Option too.",
            )
            .color(egui::Color32::GRAY)
            .size(11.0),
        );
    }
    ui.add_space(8.0);
    ui.label(
        egui::RichText::new("Keyboard mode")
            .color(egui::Color32::DARK_GRAY)
            .size(12.0),
    );
    ui.add_space(3.0);
    egui::ComboBox::from_id_salt("connection_keyboard_mode")
        .selected_text(keyboard_mode_label(value.typing))
        .width(FIELD_W)
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut value.typing, None, keyboard_mode_label(None));
            for mode in arcen_input::text::KeyboardMode::ALL {
                ui.selectable_value(
                    &mut value.typing,
                    Some(mode),
                    keyboard_mode_label(Some(mode)),
                );
            }
        });
    ui.add_space(3.0);
    let this_machine = super::wording::deck_platform(ui.ctx()).this_machine();
    ui.label(
        egui::RichText::new(match value.typing {
            None => "Follows the Typing setting in Settings.".to_owned(),
            Some(arcen_input::text::KeyboardMode::Physical) => {
                "Keys go as keys and the remote keyboard layout decides each character. Best \
                 for games and applications that read keys."
                    .to_owned()
            }
            Some(arcen_input::text::KeyboardMode::Characters) => format!(
                "Typed characters arrive as {this_machine} types them, whatever the remote \
                 layout; shortcuts, arrows and function keys stay keys. Needs a host that types \
                 text."
            ),
        })
        .color(egui::Color32::GRAY)
        .size(11.0),
    );
}

const fn keyboard_mode_label(mode: Option<arcen_input::text::KeyboardMode>) -> &'static str {
    match mode {
        None => "Use the Typing setting",
        Some(arcen_input::text::KeyboardMode::Physical) => "Physical keys",
        Some(arcen_input::text::KeyboardMode::Characters) => "Characters",
    }
}

/// The choices a PC Deck offers (PARITY Q-3), stored in the same
/// per-connection key as the Mac's: "Ctrl (as on Windows)" is Automatic (a
/// Mac Deck reading it keeps its own default) and "Windows key (keys as
/// labelled)" is the Mac's Windows App mapping. Windows and Linux hosts get
/// every PC key as itself whatever is chosen; there is no Deck-wide swap.
const PC_SHORTCUT_CHOICES: [arcen_input::keyboard::ShortcutPreference; 2] = [
    arcen_input::keyboard::ShortcutPreference::Automatic,
    arcen_input::keyboard::ShortcutPreference::WindowsApp,
];

const fn shortcut_label(
    preference: arcen_input::keyboard::ShortcutPreference,
    platform: super::wording::DeckPlatform,
) -> &'static str {
    use arcen_input::keyboard::ShortcutPreference;
    if let super::wording::DeckPlatform::Pc = platform {
        // A Mac's swap or "⌘ is Control" means Ctrl on a PC.
        return match preference {
            ShortcutPreference::WindowsApp => "Windows key (keys as labelled)",
            ShortcutPreference::Automatic
            | ShortcutPreference::SwapCommandControl
            | ShortcutPreference::CommandIsControl => "Ctrl (as on Windows)",
        };
    }
    match preference {
        ShortcutPreference::Automatic => "Automatic (by host)",
        ShortcutPreference::WindowsApp => "⌘ is the Windows key, Mac edit shortcuts",
        ShortcutPreference::SwapCommandControl => "⌘ and Control swapped",
        ShortcutPreference::CommandIsControl => "⌘ is Control",
    }
}

const fn shortcut_description(
    preference: arcen_input::keyboard::ShortcutPreference,
    platform: super::wording::DeckPlatform,
) -> &'static str {
    use arcen_input::keyboard::ShortcutPreference;
    if let super::wording::DeckPlatform::Pc = platform {
        return match preference {
            ShortcutPreference::WindowsApp => "The key next to Alt is Command.",
            ShortcutPreference::Automatic
            | ShortcutPreference::SwapCommandControl
            | ShortcutPreference::CommandIsControl => "Ctrl+C copies on the Mac.",
        };
    }
    match preference {
        ShortcutPreference::Automatic => {
            "Windows host: ⌘ is the Windows key, ⌘C/X/V/A/Z/F/W are Ctrl. \
             Linux host: ⌘ is Control. Mac host: ⌘ is ⌘."
        }
        ShortcutPreference::WindowsApp => {
            "As Microsoft's Windows App: ⌘ alone opens Start, ⌘E opens Explorer, \
             ⌘C/X/V/A/Z/F/W are Ctrl+C/X/V/A/Z/F/W. A Mac host keeps ⌘."
        }
        ShortcutPreference::SwapCommandControl => {
            "⌘ is Ctrl for every key and Control is the Windows key. A Mac host keeps ⌘."
        }
        ShortcutPreference::CommandIsControl => {
            "⌘ and Control are both Ctrl, as Arcen has always done for Linux. A Mac host keeps ⌘."
        }
    }
}

fn form_field(ui: &mut egui::Ui, label: &str, value: &mut String, password: bool, width: f32) {
    const FIELD_H: f32 = 36.0;
    ui.label(
        egui::RichText::new(label)
            .color(egui::Color32::DARK_GRAY)
            .size(12.0),
    );
    ui.add_space(3.0);
    // Tie the interact height to the drawn height and centre the text, or egui
    // lays the glyphs out at the top of the taller box (matches `detail_input`).
    ui.scope(|ui| {
        ui.style_mut().spacing.interact_size.y = FIELD_H;
        ui.add_sized(
            [width, FIELD_H],
            egui::TextEdit::singleline(value)
                .vertical_align(egui::Align::Center)
                .margin(egui::Margin::symmetric(8, 6))
                .password(password)
                .font(egui::TextStyle::Body),
        );
    });
}

/// The saved connection whose Connect button the connection error belongs
/// under: the selected one, while its detail panel is what Home shows.
/// `None` puts the error in a banner above Home instead, so a refusal or a
/// lost session is never invisible — a Deck started with `--connect` has no
/// selected connection, and the error of one connection must not appear
/// under another that Home merely shows first.
const fn connection_error_owner(
    selected_index: Option<usize>,
    show_add_form: bool,
    editing_index: Option<usize>,
) -> Option<usize> {
    if show_add_form || editing_index.is_some() {
        None
    } else {
        selected_index
    }
}

/// The connection error as a banner above Home, both Decks alike.
fn render_connection_error_banner(ui: &mut egui::Ui, error: Option<&str>) -> bool {
    let Some(error) = error else {
        return false;
    };
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(0xFD, 0xEC, 0xEC))
        .inner_margin(egui::Margin::symmetric(24, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                egui::RichText::new(error)
                    .color(egui::Color32::from_rgb(0xC0, 0x20, 0x20))
                    .size(13.0),
            );
        });
    true
}

fn render_operator_notice(ui: &mut egui::Ui, notice: Option<&str>) -> bool {
    let Some(notice) = notice else {
        return false;
    };
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(0xFF, 0xF3, 0xCD))
        .inner_margin(egui::Margin::symmetric(24, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                egui::RichText::new(notice)
                    .color(egui::Color32::from_rgb(0x66, 0x44, 0x00))
                    .strong(),
            );
        });
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pc_offers_the_command_key_on_a_mac_host_and_nothing_else() {
        use super::super::wording::DeckPlatform::Pc;
        use arcen_input::keyboard::{PcKeyboardSettings, ShortcutPreference};
        let labels: Vec<_> = PC_SHORTCUT_CHOICES
            .iter()
            .map(|choice| shortcut_label(*choice, Pc))
            .collect();
        assert_eq!(
            labels,
            ["Ctrl (as on Windows)", "Windows key (keys as labelled)"]
        );
        // Each choice is the PC keyboard policy's Mac-host Command key.
        for (choice, key) in PC_SHORTCUT_CHOICES
            .iter()
            .zip(arcen_input::keyboard::MacHostCommandKey::ALL)
        {
            let keyboard = arcen_input::keyboard::KeyboardSettings {
                shortcuts: *choice,
                ..Default::default()
            };
            let settings = PcKeyboardSettings::from_settings(
                &keyboard,
                arcen_input::text::OptionKeyPreference::Automatic,
            );
            assert_eq!(settings.mac_host_command, key);
        }
        // A Mac's own choices read as the PC default.
        for choice in [
            ShortcutPreference::SwapCommandControl,
            ShortcutPreference::CommandIsControl,
        ] {
            assert_eq!(shortcut_label(choice, Pc), "Ctrl (as on Windows)");
        }
    }

    fn settle(resolver: &AddressResolver, key: &(String, u16)) {
        for _ in 0..500 {
            if matches!(
                resolver.state.lock().unwrap().entries.get(key),
                Some(Resolution::Done(..))
            ) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("lookup of {key:?} never finished");
    }

    fn woken_once(woken: &std::sync::atomic::AtomicUsize) {
        for _ in 0..500 {
            if woken.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(woken.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn a_name_resolves_off_the_ui_thread_and_wakes_the_ui_once() {
        let resolver = AddressResolver::default();
        let woken = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let waker = || {
            let woken = std::sync::Arc::clone(&woken);
            move || {
                woken.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        };
        let now = std::time::Instant::now();
        // The first paint never waits: it shows the name and starts a lookup.
        assert_eq!(resolver.lookup("localhost", 18444, now, waker), None);
        // A second paint while it runs starts no second lookup.
        assert_eq!(resolver.lookup("localhost", 18444, now, waker), None);
        woken_once(&woken);
        assert!(resolver.lookup("localhost", 18444, now, waker).is_some());
    }

    #[test]
    fn a_failed_lookup_is_retried_and_a_network_change_forgets_answers() {
        let resolver = AddressResolver::default();
        let key = ("localhost".to_owned(), 18444);
        let failed_at = std::time::Instant::now();
        resolver
            .state
            .lock()
            .unwrap()
            .entries
            .insert(key.clone(), Resolution::Done(None, failed_at));
        let noop = || || {};
        // Within the retry window the failure stands and nothing is started.
        assert_eq!(resolver.lookup(&key.0, key.1, failed_at, noop), None);
        assert!(matches!(
            resolver.state.lock().unwrap().entries.get(&key),
            Some(Resolution::Done(None, _))
        ));
        // After it, the name is looked up again.
        let later = failed_at + UNRESOLVED_TTL;
        assert_eq!(resolver.lookup(&key.0, key.1, later, noop), None);
        settle(&resolver, &key);

        resolver.state.lock().unwrap().entries.insert(
            key.clone(),
            Resolution::Done(Some("203.0.113.82".to_owned()), failed_at),
        );
        resolver.clear();
        assert!(!resolver.state.lock().unwrap().entries.contains_key(&key));
    }

    #[test]
    fn a_stale_answer_stays_up_while_it_refreshes_and_a_cleared_lookup_writes_nothing() {
        let resolver = AddressResolver::default();
        let key = ("localhost".to_owned(), 18444);
        let resolved_at = std::time::Instant::now();
        let old = Some("203.0.113.82".to_owned());
        resolver
            .state
            .lock()
            .unwrap()
            .entries
            .insert(key.clone(), Resolution::Done(old.clone(), resolved_at));
        let later = resolved_at + RESOLVED_TTL;
        let noop = || || {};
        assert_eq!(resolver.lookup(&key.0, key.1, later, noop), old);
        // Every paint during the refresh keeps showing it.
        assert_eq!(resolver.lookup(&key.0, key.1, later, noop), old);
        settle(&resolver, &key);

        let woken = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let waker = || {
            let woken = std::sync::Arc::clone(&woken);
            move || {
                woken.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        };
        resolver.clear();
        assert_eq!(resolver.lookup(&key.0, key.1, later, waker), None);
        // The network changes while the lookup runs.
        resolver.clear();
        woken_once(&woken);
        assert!(!resolver.state.lock().unwrap().entries.contains_key(&key));
    }

    #[test]
    fn a_direct_machine_uses_the_product_udp_port() {
        assert_eq!(ConnectionKind::DirectMachine.default_port(), 18444);
        assert_eq!(ConnectionKind::DirectMachine.title(), "Direct Machine");
    }

    #[test]
    fn operator_notice_is_rendered_only_when_present() {
        egui::__run_test_ui(|ui| {
            assert!(!render_operator_notice(ui, None));
            assert!(render_operator_notice(
                ui,
                Some("Insecure mode: this Deck is not verifying Pier certificates.")
            ));
        });
    }

    #[test]
    fn a_connection_error_is_shown_under_its_own_connection_or_above_home() {
        // Connected from the selected saved connection: under its button.
        assert_eq!(connection_error_owner(Some(2), false, None), Some(2));
        // Started with `--connect` (nothing selected): a banner, even when
        // Home shows another saved connection first.
        assert_eq!(connection_error_owner(None, false, None), None);
        // A form covers the detail panel: a banner.
        assert_eq!(connection_error_owner(Some(2), true, None), None);
        assert_eq!(connection_error_owner(Some(2), false, Some(2)), None);
        egui::__run_test_ui(|ui| {
            assert!(!render_connection_error_banner(ui, None));
            assert!(render_connection_error_banner(
                ui,
                Some("the host refused the requested video: selects yuv444")
            ));
        });
    }

    use crate::settings::SecurityMode;

    #[test]
    fn a_connect_error_stays_in_the_banner_until_a_connection_is_chosen() {
        let saved = vec![SavedConnectionSummary {
            id: "studio".to_owned(),
            kind: ConnectionKind::DirectMachine,
            name: "Studio".to_owned(),
            host: "203.0.113.10".to_owned(),
            port: 18444,
            use_tls: true,
            transport: SavedTransport::Quic,
            identity_security_mode: None,
            settings: SavedConnectionSettings::default(),
            unknown: serde_json::Map::new(),
        }];
        // A Deck started with `--connect`: nothing saved is selected.
        let mut direct = ConnectionDraft::new(ConnectionKind::DirectMachine);
        let (mut search, mut add, mut editing) = (String::new(), false, None);
        let (mut name, mut host, mut port, mut user) =
            (String::new(), String::new(), String::new(), String::new());
        let mut displays = None;
        let mut keyboard = arcen_input::keyboard::KeyboardSettings::default();
        let mut remember = false;
        let repository = crate::settings::ClientConfigRepository::new(None);
        let mut frame = |error: Option<&str>, direct: &mut ConnectionDraft| {
            let mut state = HomeViewState {
                direct,
                saved_connections: &saved,
                search: &mut search,
                operator_notice: None,
                show_add_form: &mut add,
                editing_index: &mut editing,
                form_name: &mut name,
                form_host: &mut host,
                form_port: &mut port,
                form_username: &mut user,
                form_displays_mode: &mut displays,
                form_keyboard: &mut keyboard,
                app_displays_mode: crate::settings::DisplaysMode::SinglePrimary,
                connection_error: error,
                remember_username: &mut remember,
                connection_test: None,
            };
            let mut action = None;
            egui::__run_test_ui(|ui| action = render_home(ui, &mut state));
            let outcome = action.clone().map(|action| {
                apply_home_action(&mut state, action, &repository, SecurityMode::High)
            });
            (action, outcome.and_then(|outcome| outcome.status))
        };
        let refused = Some("the host refused the requested video: selects yuv444");
        // The error is not moved under the first saved connection.
        let (action, _) = frame(refused, &mut direct);
        assert!(action.is_none(), "{action:?}");
        assert!(direct.saved_connection_id.is_none());
        // Without one, Home selects the first connection as before.
        let (action, status) = frame(None, &mut direct);
        assert!(matches!(action, Some(HomeAction::SelectConnection(0))));
        assert!(status.is_none());
    }

    #[test]
    fn choosing_another_connection_clears_the_last_attempts_error() {
        let saved = vec![SavedConnectionSummary {
            id: "studio".to_owned(),
            kind: ConnectionKind::DirectMachine,
            name: "Studio".to_owned(),
            host: "203.0.113.10".to_owned(),
            port: 18444,
            use_tls: true,
            transport: SavedTransport::Quic,
            identity_security_mode: None,
            settings: SavedConnectionSettings::default(),
            unknown: serde_json::Map::new(),
        }];
        let mut direct = ConnectionDraft::new(ConnectionKind::DirectMachine);
        let (mut search, mut add, mut editing) = (String::new(), false, None);
        let (mut name, mut host, mut port, mut user) =
            (String::new(), String::new(), String::new(), String::new());
        let mut displays = None;
        let mut keyboard = arcen_input::keyboard::KeyboardSettings::default();
        let mut remember = false;
        let repository = crate::settings::ClientConfigRepository::new(None);
        let mut state = HomeViewState {
            direct: &mut direct,
            saved_connections: &saved,
            search: &mut search,
            operator_notice: None,
            show_add_form: &mut add,
            editing_index: &mut editing,
            form_name: &mut name,
            form_host: &mut host,
            form_port: &mut port,
            form_username: &mut user,
            form_displays_mode: &mut displays,
            form_keyboard: &mut keyboard,
            app_displays_mode: crate::settings::DisplaysMode::SinglePrimary,
            connection_error: Some("the host refused the requested video: selects yuv444"),
            remember_username: &mut remember,
            connection_test: None,
        };
        let outcome = apply_home_action(
            &mut state,
            HomeAction::SelectConnection(0),
            &repository,
            SecurityMode::High,
        );
        assert_eq!(outcome.status.as_deref(), Some(""));
        // Re-selecting the connection the error is shown under keeps it.
        let outcome = apply_home_action(
            &mut state,
            HomeAction::SelectConnection(0),
            &repository,
            SecurityMode::High,
        );
        assert!(outcome.status.is_none());
    }

    #[test]
    fn validate_username_rejects_control_chars_and_long_values() {
        assert!(validate_username("alice").is_some());
        assert!(validate_username("").is_none());
        assert!(validate_username("  ").is_none());
        assert!(validate_username("a\x00b").is_none());
        assert!(validate_username(&"x".repeat(255)).is_some());
        assert!(validate_username(&"x".repeat(256)).is_none());
    }

    #[test]
    fn every_saved_kind_key_loads_as_a_direct_machine() {
        assert_eq!(
            ConnectionKind::from_key(ConnectionKind::DirectMachine.as_key()),
            ConnectionKind::DirectMachine
        );
        assert_eq!(
            ConnectionKind::from_key("retired-kind"),
            ConnectionKind::DirectMachine
        );
    }
}
