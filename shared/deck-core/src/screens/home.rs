// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Home, the calm list: the saved machines on the left, each with its
//! status (or its address while no status is known) under its name; the
//! selected machine on the right in one centred column, with one large
//! Connect (or Resume); and the two-column new/edit connection form. The
//! shell owns the state and maps each [`HomeAction`].
// ── Sidebar geometry ─────────────────────────────────────────────────────────
const SIDEBAR_W: f32 = 210.0;
/// A row: its two lines (the name, then the address) and
/// [`super::theme::ROW_PADDING_Y`] above and below them.
const SIDEBAR_ROW_H: f32 = 2.0 * super::theme::ROW_PADDING_Y + ROW_NAME_LINE + ROW_ADDRESS_LINE;
/// The name line's height in a row (body text).
const ROW_NAME_LINE: f32 = 18.0;
/// The address line's height in a row (label text).
const ROW_ADDRESS_LINE: f32 = 16.0;
/// The sidebar's edge, the one rule on Home.
const SIDEBAR_BORDER_W: f32 = 1.0;
/// The accent bar on a selected row's left edge.
const SELECTED_ROW_BAR_W: f32 = 3.0;
/// The least space either side of the scene's column.
const DETAIL_SIDE_MARGIN: f32 = 24.0;
/// The selected machine's name, the title of its column.
const MACHINE_TITLE: f32 = 28.0;
/// The new/edit form's title and its two section titles.
const FORM_TITLE: f32 = 26.0;
const SECTION_TITLE: f32 = 20.0;
/// The form's columns, the gutter between them, and the pane width under
/// which they stack (Connection first).
const FORM_COLUMN_W: f32 = 340.0;
const FORM_GUTTER: f32 = 40.0;
const FORM_STACK_BELOW: f32 = 760.0;

// ── Validate a username string (mirrors the app-level rule) ──────────────────
fn validate_username(username: &str) -> Option<String> {
    let u = username.trim();
    (!u.is_empty() && u.len() <= 255 && !u.chars().any(char::is_control)).then(|| u.to_string())
}

/// What the connection form says under Address when it is empty.
pub const ADDRESS_MISSING: &str = "Enter the host\u{2019}s name or IP address.";
/// What it says when the host part is not a name or an IP address.
pub const ADDRESS_INVALID: &str =
    "Enter a host name or IP address; write IPv6 with a port as [address]:port.";
/// What it says when the port is not one.
pub const PORT_INVALID: &str = "Enter a port from 1 to 65535.";

/// A connection form's address: `host`, `host:port`, `[v6]:port` or a bare
/// IPv6 address (the shared reader the connection test uses). The port
/// defaults to the product port, 18444.
///
/// # Errors
///
/// The sentence the form shows under the field.
pub fn parse_address(text: &str) -> Result<(String, u16), &'static str> {
    use arcen_session::connection_diagnostics::{DiagnosticTarget, TargetError};
    DiagnosticTarget::parse_with_port(text, Some(ConnectionKind::DirectMachine.default_port()))
        .map(|target| (target.host, target.port))
        .map_err(|error| match error {
            TargetError::Empty => ADDRESS_MISSING,
            TargetError::InvalidHost => ADDRESS_INVALID,
            TargetError::InvalidPort => PORT_INVALID,
        })
}

/// How a saved address reads in its field and on Home: the host alone on
/// the product port, `host:port` otherwise (`[v6]:port` for IPv6).
#[must_use]
pub fn address_text(host: &str, port: u16) -> String {
    if port == 0 || port == ConnectionKind::DirectMachine.default_port() {
        host.to_owned()
    } else if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// The connection form's buttons beside each other, on the right of its
/// action row. The import summary (`super::profile`) orders its Cancel and
/// Import the same way, Import as Save.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormButton {
    Cancel,
    Save,
}

/// The form's buttons, left to right, in each platform's order: a Mac puts
/// the default button last (`[Cancel][Save]`), Windows first
/// (`[Save][Cancel]`).
#[must_use]
pub const fn form_button_order(platform: super::wording::DeckPlatform) -> [FormButton; 2] {
    match platform {
        super::wording::DeckPlatform::Mac => [FormButton::Cancel, FormButton::Save],
        super::wording::DeckPlatform::Pc => [FormButton::Save, FormButton::Cancel],
    }
}

pub use crate::config::{
    ConnectionKind, SavedConnectionSettings, SavedConnectionSummary, SavedTransport,
};
pub use crate::connection_draft::ConnectionDraft;
use crate::presence::{HostPresence, PresenceSource};
use crate::sign_in::{SignInFeedback, SignInField};

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
    /// The address as typed: `host`, `host:port` or `[v6]:port`.
    pub form_host: &'a mut String,
    pub form_username: &'a mut String,
    /// Working values for the form's own-value settings (Displays,
    /// Command, Option, Typing); each follows Settings unless set.
    pub form_overrides: &'a mut crate::overrides::ConnectionOverrides,
    /// The application-wide values a connection without its own value
    /// connects with: Edit Connection's On this machine shows "App default
    /// (…)". (A connection with its own Displays choice says so in its
    /// column, from its saved settings.)
    pub app_defaults: crate::overrides::AppDefaults,
    /// The last sign-in outcome: under its field on the connection's card,
    /// or in an acknowledgement box (`crate::sign_in`).
    pub sign_in: &'a mut crate::sign_in::SignInState,
    /// A Deck window is over Home (Help ▸ About or Keyboard Shortcuts, the
    /// Colour check): Esc is that window's, never the form's.
    pub overlay_open: bool,
    pub remember_username: &'a mut bool,
    /// The connection test shown in the editor or detail panel, if any.
    pub connection_test: Option<&'a super::connection_test::ConnectionTest>,
    /// What Export or Import last did, under the connection list until
    /// dismissed ([`super::profile::ProfileOutcome::home_notice`]).
    pub notice: &'a mut Option<HomeNotice>,
}

/// A quiet line under Home's connection list saying what Export or Import
/// did (a sign-in outcome never shows here: it is under its field).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeNotice {
    /// What happened, in plain words.
    pub text: String,
    /// Whether something did not happen or was only partly done.
    pub problem: bool,
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
    /// Save a connection as a profile file (`.arcen`).
    ExportProfile(usize),
    /// Read a profile file and show its import summary.
    ImportProfile,
    /// Confirm a new-connection form.
    SaveNew {
        name: String,
        host: String,
        port: u16,
        username: String,
        /// This connection's own values; the rest follows Settings.
        overrides: crate::overrides::ConnectionOverrides,
    },
    /// Confirm an edit-connection form.
    SaveEdit {
        index: usize,
        name: String,
        host: String,
        port: u16,
        username: String,
        /// This connection's own values; the rest follows Settings.
        overrides: crate::overrides::ConnectionOverrides,
    },
    /// Cancel the add/edit form.
    CancelForm,
    /// Hand a saved connection's Displays choice back to Settings ›
    /// Displays ("Use Settings" on its panel).
    FollowAppDisplays(usize),
    /// Run the connection test against this address. Sends no credentials.
    TestConnection { host: String, port: String },
    /// Select a saved connection and run the connection test against it
    /// (its ⋮ menu), so the report shows on its own card.
    TestSaved(usize),
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
    /// Export this saved connection ([`super::profile::ProfileSharing::export`]).
    ExportProfile(usize),
    /// Import a profile ([`super::profile::ProfileSharing::import`]).
    ImportProfile,
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
        HomeAction::ExportProfile(index) => HomeOutcome::effect(HomeEffect::ExportProfile(index)),
        HomeAction::ImportProfile => HomeOutcome::effect(HomeEffect::ImportProfile),
        HomeAction::BeginAdd => {
            // A form covers the card the outcome belongs to: it goes.
            state.sign_in.dismiss();
            *state.editing_index = None;
            state.form_host.clear();
            state.form_name.clear();
            state.form_username.clear();
            // A brand-new connection starts on the application default
            // rather than inheriting whichever connection was edited last.
            *state.form_overrides = crate::overrides::ConnectionOverrides::default();
            *state.show_add_form = true;
            HomeOutcome::effect(HomeEffect::None)
        }
        HomeAction::TestSaved(index) => match state.saved_connections.get(index).cloned() {
            Some(saved) => {
                select_connection(state, index);
                HomeOutcome::effect(HomeEffect::TestConnection {
                    host: saved.host,
                    port: saved.port.to_string(),
                })
            }
            None => HomeOutcome::effect(HomeEffect::None),
        },
        HomeAction::SelectConnection(index) => {
            select_connection(state, index);
            HomeOutcome::effect(HomeEffect::None)
        }
        // Nothing is dialled without what signing in needs: the field to
        // fill is named and focused instead.
        // Once Connect goes ahead the password leaves Home with the draft:
        // the field is wiped, so it never outlives the attempt on screen
        // (a later Disconnect finds it empty, and retyping never appends).
        HomeAction::Connect(draft) => match state.sign_in.connect(&draft) {
            Ok(()) => {
                zeroize::Zeroize::zeroize(&mut state.direct.password);
                HomeOutcome::effect(HomeEffect::Connect(draft))
            }
            Err(_) => HomeOutcome::effect(HomeEffect::None),
        },
        HomeAction::Edit(index) => {
            if let Some(saved) = state.saved_connections.get(index) {
                state.sign_in.dismiss();
                // A port other than the product's shows in the address.
                *state.form_host = address_text(&saved.host, saved.port);
                state.form_name.clone_from(&saved.name);
                *state.form_username = saved
                    .settings
                    .remembered_username
                    .clone()
                    .unwrap_or_default();
                *state.form_overrides = saved.settings.overrides;
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
                            state.sign_in.clear();
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
            overrides,
        } => {
            let mut outcome = HomeOutcome::effect(HomeEffect::None);
            let port_num = if port == 0 {
                ConnectionKind::DirectMachine.default_port()
            } else {
                port
            };
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
                    overrides,
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
                    zeroize::Zeroize::zeroize(&mut state.direct.password);
                    // The new connection is selected: an outcome of another
                    // one does not follow it.
                    state.sign_in.clear();
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
            overrides,
        } => {
            let mut outcome = HomeOutcome::effect(HomeEffect::None);
            if let Some(original) = state.saved_connections.get(index).cloned() {
                let port_num = if port == 0 { original.port } else { port };
                let mut updated = original.clone();
                updated.name.clone_from(&name);
                updated.host = host;
                updated.port = port_num;
                updated.settings.remembered_username =
                    crate::config::validated_remembered_username(&username);
                updated.settings.unknown.remove("remembered_username");
                updated
                    .settings
                    .overrides
                    .replace_in(overrides, &mut updated.settings.unknown);
                let selected =
                    state.direct.saved_connection_id.as_deref() == Some(original.id.as_str());
                match repository.update_connection(&original.id, &original, updated.clone()) {
                    Ok(store) => {
                        outcome.store = Some(store);
                        // The selected connection now names another machine:
                        // its typed password and its certificate trust were
                        // for the old one, and its outcome too.
                        if selected && !state.direct.is_for(&updated) {
                            state.direct.select_saved(&updated);
                            state.sign_in.clear();
                        }
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
            HomeOutcome::effect(HomeEffect::None)
        }
        HomeAction::FollowAppDisplays(index) => {
            let mut outcome = HomeOutcome::effect(HomeEffect::None);
            if let Some(original) = state.saved_connections.get(index).cloned() {
                let mut updated = original.clone();
                let mut edited = updated.settings.overrides;
                edited.displays_mode = crate::overrides::Override::Inherit;
                updated
                    .settings
                    .overrides
                    .replace_in(edited, &mut updated.settings.unknown);
                match repository.update_connection(&original.id, &original, updated) {
                    Ok(store) => {
                        outcome.store = Some(store);
                        outcome.status = Some(format!(
                            "\u{201c}{}\u{201d} now connects with Settings \u{203a} Displays: {}",
                            original.name,
                            state.app_defaults.displays_mode.label()
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

/// Selects saved connection `index`: its card, with its remembered username.
fn select_connection(state: &mut HomeViewState<'_>, index: usize) {
    if let Some(saved) = state.saved_connections.get(index) {
        // The outcome is the last attempt's; it does not follow the
        // selection to another connection.
        if state.direct.saved_connection_id.as_deref() != Some(saved.id.as_str()) {
            state.sign_in.clear();
        }
        // Another connection's (or another address's) certificate trust
        // does not follow the selection: that starts a fresh draft.
        state.direct.select_saved(saved);
        *state.show_add_form = false;
        *state.editing_index = None;
    }
}

// ── Main entry point ─────────────────────────────────────────────────────────

/// ⌘N on a Mac, Ctrl+N on a PC: New connection, on Home only (no Deck
/// menu or chord uses it, and a session never draws Home).
pub const NEW_CONNECTION_SHORTCUT: egui::KeyboardShortcut =
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::N);

pub fn render_home(ui: &mut egui::Ui, state: &mut HomeViewState<'_>) -> Option<HomeAction> {
    render_home_with(ui, state, crate::presence::current())
}

/// [`render_home`] with machine status from `presence`.
pub fn render_home_with(
    ui: &mut egui::Ui,
    state: &mut HomeViewState<'_>,
    presence: &dyn PresenceSource,
) -> Option<HomeAction> {
    let mut action = None;
    if !*state.show_add_form && state.editing_index.is_none() {
        // The next form starts afresh: nothing tried, its disclosure as
        // its connection's settings say.
        ui.data_mut(|data| data.remove::<FormMemory>(form_memory_id()));
    }
    if !*state.show_add_form
        && state.editing_index.is_none()
        && !state.overlay_open
        && state.sign_in.acknowledgement().is_none()
        && ui.input_mut(|input| input.consume_shortcut(&NEW_CONNECTION_SHORTCUT))
    {
        action = Some(HomeAction::BeginAdd);
    }

    render_operator_notice(ui, state.operator_notice);

    let selected_index = state
        .direct
        .saved_connection_id
        .as_ref()
        .and_then(|sel_id| state.saved_connections.iter().position(|c| &c.id == sel_id));
    // A connection that is not saved (`--connect`) keeps a card of its own
    // after a failed sign-in, so its password can be typed again.
    let unsaved_card = selected_index.is_none()
        && !*state.show_add_form
        && state.editing_index.is_none()
        && state.direct.saved_connection_id.is_none()
        && !state.direct.host.is_empty()
        && state.sign_in.keeps_unsaved_connection();
    // A sign-in outcome shows only under its field on its own card (or in
    // the acknowledgement box): never as a banner above Home, which stayed
    // on other connections and forms and moved the layout.
    if render_acknowledgement(ui.ctx(), state.sign_in.acknowledgement()) {
        state.sign_in.acknowledge();
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
                && !unsaved_card
                && !*state.show_add_form
                && state.editing_index.is_none()
                && state.sign_in.feedback().is_none()
            {
                first_visible_connection_index(state.saved_connections, &search_needle)
            } else {
                None
            };

            // ── Left sidebar ──────────────────────────────────────────────
            if let Some(a) = sidebar(ui, state, presence, available.y) {
                action = Some(a);
            }

            // ── Right detail panel ────────────────────────────────────────
            let detail_action = if *state.show_add_form {
                connection_form(ui, state, None)
            } else if let Some(edit_idx) = *state.editing_index {
                connection_form(ui, state, Some(edit_idx))
            } else if let Some(i) = selected_index.or(fallback_index) {
                let saved = &state.saved_connections[i];
                let status = presence.presence(&saved.id, &saved.name);
                detail_connection(ui, state, i, &status)
            } else if unsaved_card {
                detail_unsaved(ui, state)
            } else {
                detail_empty(ui, state.saved_connections.is_empty())
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

fn sidebar(
    ui: &mut egui::Ui,
    state: &mut HomeViewState<'_>,
    presence: &dyn PresenceSource,
    panel_h: f32,
) -> Option<HomeAction> {
    let mut action = None;
    let colours = super::theme::colours(ui);

    egui::Frame::new()
        .fill(colours.panel)
        .stroke(egui::Stroke::new(SIDEBAR_BORDER_W, colours.border))
        .inner_margin(egui::Margin::ZERO)
        .show(ui, |ui| {
            ui.set_width(SIDEBAR_W);
            ui.set_min_height(panel_h);

            ui.vertical(|ui| {
                ui.set_width(SIDEBAR_W);
                let sidebar_top_y = ui.cursor().min.y;

                // Header + search, the header on the scenes' top line (the
                // frame's 1-point border is above it already).
                ui.add_space(super::theme::SCENE_TOP_INSET - SIDEBAR_BORDER_W);
                ui.horizontal_top(|ui| {
                    ui.add_space(super::theme::LIST_INSET);
                    ui.label(
                        egui::RichText::new("Connections")
                            .strong()
                            .color(colours.text)
                            .size(super::theme::TEXT_BODY),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                        ui.add_space(super::theme::LIST_INSET - 4.0);
                        if let (_, Some(item)) = add_menu_button(ui) {
                            action = Some(item.action());
                        }
                    });
                });
                ui.add_space(super::theme::HEADER_TO_SEARCH);
                ui.horizontal(|ui| {
                    ui.add_space(super::theme::LIST_INSET);
                    let _ = super::widgets::text_field(
                        ui,
                        state.search,
                        false,
                        SIDEBAR_W - 2.0 * super::theme::LIST_INSET,
                        "Search\u{2026}",
                    );
                });
                ui.add_space(super::theme::SPACE_SCALE[2]);

                let needle = state.search.trim().to_lowercase();
                let mut any = false;
                for (idx, saved) in state.saved_connections.iter().enumerate() {
                    if !connection_matches(saved, &needle) {
                        continue;
                    }
                    any = true;
                    let sel =
                        state.direct.saved_connection_id.as_deref() == Some(saved.id.as_str());
                    let status = presence.presence(&saved.id, &saved.name);
                    if let Some(a) = sidebar_row(ui, idx, saved, &status, sel) {
                        action = Some(a);
                    }
                }

                let empty = if state.saved_connections.is_empty() {
                    Some("No saved connections yet.")
                } else if any {
                    None
                } else {
                    Some("No matches.")
                };
                if let Some(empty) = empty {
                    ui.add_space(super::theme::SPACE_SCALE[2]);
                    ui.horizontal(|ui| {
                        ui.add_space(super::theme::LIST_INSET);
                        ui.label(
                            egui::RichText::new(empty)
                                .color(colours.text2)
                                .size(super::theme::TEXT_LABEL),
                        );
                    });
                }
                if home_notice(ui, state.notice.as_ref()) {
                    *state.notice = None;
                }

                // The list keeps the whole height: adding a connection is
                // the ＋ beside "Connections" (and the empty card's buttons).
                let used = ui.cursor().min.y - sidebar_top_y;
                if panel_h > used {
                    ui.add_space(panel_h - used);
                }
            });
        });

    action
}

/// Export's or Import's notice under the list, with Dismiss. Returns
/// whether it was dismissed.
fn home_notice(ui: &mut egui::Ui, notice: Option<&HomeNotice>) -> bool {
    let Some(notice) = notice else {
        return false;
    };
    let colours = super::theme::colours(ui);
    let mut dismissed = false;
    ui.add_space(super::theme::SPACE_SCALE[2]);
    ui.horizontal(|ui| {
        ui.add_space(super::theme::LIST_INSET);
        ui.vertical(|ui| {
            ui.set_max_width(SIDEBAR_W - 2.0 * super::theme::LIST_INSET);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(&notice.text)
                        .color(if notice.problem {
                            colours.error
                        } else {
                            colours.text2
                        })
                        .size(super::theme::TEXT_LABEL),
                )
                .wrap(),
            );
            dismissed = super::widgets::text_button(ui, "Dismiss", false).clicked();
        });
    });
    dismissed
}

/// The Connections header's ＋ menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddMenuItem {
    NewConnection,
    ImportFromFile,
}

impl AddMenuItem {
    /// The menu, top to bottom.
    pub const ALL: [Self; 2] = [Self::NewConnection, Self::ImportFromFile];

    /// Its title.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::NewConnection => "New connection\u{2026}",
            Self::ImportFromFile => "Import from file\u{2026}",
        }
    }

    /// The Home action it takes.
    #[must_use]
    pub const fn action(self) -> HomeAction {
        match self {
            Self::NewConnection => HomeAction::BeginAdd,
            Self::ImportFromFile => HomeAction::ImportProfile,
        }
    }
}

/// The ＋ beside "Connections": a drawn plus that opens [`AddMenuItem::ALL`].
fn add_menu_button(ui: &mut egui::Ui) -> (egui::Response, Option<AddMenuItem>) {
    let colours = super::theme::colours(ui);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(24.0, 24.0), egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Add connection")
    });
    if ui.is_rect_visible(rect) {
        if response.hovered() || response.has_focus() {
            ui.painter()
                .rect_filled(rect, super::theme::CONTROL_RADIUS, colours.hover);
        }
        super::icons::paint(
            ui.painter(),
            super::icons::Icon::Plus,
            rect.center(),
            super::icons::ICON_GRID,
            colours.text,
        );
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let response = response.on_hover_text("New connection or import from a file");
    let mut chosen = None;
    egui::Popup::menu(&response).gap(2.0).show(|ui| {
        for item in AddMenuItem::ALL {
            if ui.button(item.title()).clicked() {
                chosen = Some(item);
            }
        }
    });
    (response, chosen)
}

/// The colour a known status reads in.
fn status_colour(colours: &super::theme::Palette, presence: &HostPresence) -> egui::Color32 {
    match presence {
        HostPresence::Ready => colours.success,
        HostPresence::SignedIn { by_you: true, .. } => colours.info,
        HostPresence::SignedIn { .. } | HostPresence::InUse { .. } => colours.warning,
        HostPresence::Unknown | HostPresence::Off => colours.text2,
    }
}

/// What a row says under the name: the status when it is known, in its
/// colour; the address otherwise, quietly.
fn row_second_line(
    colours: &super::theme::Palette,
    saved: &SavedConnectionSummary,
    presence: &HostPresence,
) -> (String, egui::Color32) {
    presence.phrase().map_or_else(
        || {
            (
                shortened(&address_text(&saved.host, saved.port), 24, 27),
                colours.text2,
            )
        },
        |phrase| (phrase, status_colour(colours, presence)),
    )
}

fn sidebar_row(
    ui: &mut egui::Ui,
    index: usize,
    saved: &SavedConnectionSummary,
    presence: &HostPresence,
    selected: bool,
) -> Option<HomeAction> {
    let mut action = None;
    let colours = super::theme::colours(ui);

    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(SIDEBAR_W, SIDEBAR_ROW_H), egui::Sense::click());
    // The row's name in egui's widget info (the manual's screenshots place a
    // hotspot on it).
    resp.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            true,
            selected,
            &saved.name,
        )
    });

    if ui.is_rect_visible(rect) {
        let bg = if selected {
            Some(colours.selected)
        } else if resp.hovered() {
            Some(super::theme::tint_on(
                colours.panel,
                colours.text,
                super::theme::HOVER_TINT_PERCENT + 2,
            ))
        } else {
            None
        };
        if let Some(bg) = bg {
            ui.painter().rect_filled(rect, 0.0, bg);
        }
        if selected {
            ui.painter().rect_filled(
                egui::Rect::from_min_size(rect.min, egui::vec2(SELECTED_ROW_BAR_W, rect.height())),
                0.0,
                colours.accent,
            );
        }

        let inset = super::theme::LIST_INSET;
        let text_x = rect.left() + inset + 4.0;
        let name_y = rect.top() + super::theme::ROW_PADDING_Y + ROW_NAME_LINE / 2.0;
        let second_y = rect.bottom() - super::theme::ROW_PADDING_Y - ROW_ADDRESS_LINE / 2.0;
        ui.painter().text(
            egui::pos2(text_x, name_y),
            egui::Align2::LEFT_CENTER,
            shortened(&saved.name, 20, 22),
            egui::FontId::proportional(super::theme::TEXT_BODY),
            colours.text,
        );
        let (line, ink) = row_second_line(&colours, saved, presence);
        ui.painter().text(
            egui::pos2(text_x, second_y),
            egui::Align2::LEFT_CENTER,
            line,
            egui::FontId::proportional(super::theme::TEXT_LABEL),
            ink,
        );

        // Kebab (⋮): only on the selected or hovered row (or while it has
        // the focus or its menu is open), so the list stays quiet.
        let dot_x = rect.right() - inset - 2.0;
        let dot_rect = egui::Rect::from_center_size(
            egui::pos2(dot_x, rect.center().y),
            egui::vec2(20.0, 36.0),
        );
        let dot_resp = ui.interact(
            dot_rect,
            ui.id().with(("connection_menu", index)),
            egui::Sense::click(),
        );
        dot_resp.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Connection menu")
        });
        let menu_open =
            egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&dot_resp));
        if kebab_visible(
            selected,
            ui.rect_contains_pointer(rect),
            dot_resp.has_focus() || menu_open,
        ) {
            // Orange stays for the selection and Connect: a hovered menu
            // is the body ink.
            let dot_color = if dot_resp.hovered() {
                colours.text
            } else {
                colours.text2
            };
            super::icons::paint(
                ui.painter(),
                super::icons::Icon::Kebab,
                egui::pos2(dot_x, rect.center().y),
                super::icons::ICON_GRID,
                dot_color,
            );
        }

        egui::Popup::menu(&dot_resp).gap(2.0).show(|ui| {
            ui.set_min_width(140.0);
            if ui.button("Edit").clicked() {
                action = Some(HomeAction::Edit(index));
                ui.close();
            }
            if ui.button("Test connection").clicked() {
                action = Some(HomeAction::TestSaved(index));
                ui.close();
            }
            if ui.button("Export\u{2026}").clicked() {
                action = Some(HomeAction::ExportProfile(index));
                ui.close();
            }
            ui.separator();
            if ui
                .button(egui::RichText::new("Delete").color(colours.error))
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

/// Whether a row shows its ⋮ menu: on the selected row, the row under the
/// pointer, and while the menu has the focus or is open.
const fn kebab_visible(selected: bool, hovered: bool, engaged: bool) -> bool {
    selected || hovered || engaged
}

/// `text` when it has at most `limit` characters, otherwise its first
/// `keep` characters and "…". Counts characters, never bytes: a name from
/// an imported file may be any script.
fn shortened(text: &str, keep: usize, limit: usize) -> String {
    if text.chars().count() > limit {
        let mut short: String = text.chars().take(keep).collect();
        short.push('…');
        short
    } else {
        text.to_owned()
    }
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
/// the name alone until then or when it does not resolve; a port other
/// than the product's follows the host (`host:port`).
///
/// Resolution never runs on the UI thread (R08): a lookup starts on its own
/// thread and wakes the UI when it lands. Answers expire, failures sooner,
/// and [`forget_resolved_addresses`] drops them all when the network changes.
fn display_address(ctx: &egui::Context, host: &str, port: u16) -> String {
    let host = host.trim();
    let shown = address_text(host, port);
    if host.parse::<std::net::IpAddr>().is_ok() {
        return shown;
    }
    match resolver().lookup(host, port, std::time::Instant::now(), || {
        let ctx = ctx.clone();
        move || ctx.request_repaint()
    }) {
        Some(ip) => format!("{ip} ({shown})"),
        None => shown,
    }
}

/// Drops every resolved address, so the next paint resolves again (a shell
/// calls it when the network or location changes).
pub fn forget_resolved_addresses() {
    resolver().clear();
}

/// Developer-only (`doc-shots`): `host` reads as written for a while and is
/// not looked up, so a rendered scene touches no network and shows no
/// address.
#[cfg(feature = "doc-shots")]
pub fn keep_unresolved(host: &str, port: u16) {
    if let Ok(mut state) = resolver().state.lock() {
        state.entries.insert(
            (host.to_owned(), port),
            Resolution::Done(None, std::time::Instant::now()),
        );
    }
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

/// What Home says with no connection to show: what to do, with both ways
/// to do it.
pub const EMPTY_HOME_LINE: &str = "Add a connection, or import one from a file.";

/// Test connection's tooltip.
const TEST_CONNECTION_TIP: &str = "Checks the name, UDP 18444, the certificate, the Pier's greeting, \
     path MTU and round trip. Sends no user name or password.";

/// What the connection form says under a username it cannot keep.
const USERNAME_INVALID: &str = "Username must be at most 255 bytes, with no control characters.";

fn detail_empty(ui: &mut egui::Ui, nothing_saved: bool) -> Option<HomeAction> {
    use super::theme;
    let mut action = None;
    let colours = theme::colours(ui);
    detail_column(ui, "detail_scroll", 24.0, |ui| {
        scene_title(
            ui,
            if nothing_saved {
                "No connections yet"
            } else {
                "Select a connection"
            },
        );
        ui.add_space(theme::SPACE_SCALE[1]);
        ui.add(
            egui::Label::new(
                egui::RichText::new(EMPTY_HOME_LINE)
                    .color(colours.text2)
                    .size(theme::TEXT_BODY),
            )
            .wrap(),
        );
        ui.add_space(theme::GROUP_GAP);
        let width = super::widgets::button_width(ui, &["Add connection", "Import\u{2026}"]);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = theme::BUTTON_GAP;
            if super::widgets::primary_button(ui, "Add connection", width).clicked() {
                action = Some(HomeAction::BeginAdd);
            }
            if super::widgets::secondary_button(ui, "Import\u{2026}", width).clicked() {
                action = Some(HomeAction::ImportProfile);
            }
        });
    });
    action
}

/// The scene's column: centred in the pane, at most
/// [`super::theme::COLUMN_MAX`] wide, starting
/// [`super::theme::SCENE_TOP_INSET`] under the bar (as the sidebar's
/// header does), with `room_below` under it. The inset is inside the
/// scroll area. The scroll area spans the whole pane, so its bar sits at
/// the window's edge and never draws a line beside the column. Inside, the
/// gaps are the spacing scale's only: no implicit space between items.
fn detail_column<R>(
    ui: &mut egui::Ui,
    id: &str,
    room_below: f32,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    pane_column(ui, id, super::theme::COLUMN_MAX, room_below, add)
}

/// A column `max` wide at most, centred in the pane, in a scroll area as
/// wide as the pane.
fn pane_column<R>(
    ui: &mut egui::Ui,
    id: &str,
    max: f32,
    room_below: f32,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    egui::ScrollArea::vertical()
        .id_salt(id)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // The scroll area's content takes its parent's layout, which
            // is Home's left-to-right row: a column of its own, or the top
            // inset would be space to the left, and the scene would start
            // against the bar.
            ui.vertical(|ui| column_contents(ui, max, room_below, add))
                .inner
        })
        .inner
}

/// [`pane_column`]'s contents, in a top-down `ui` as wide as the pane.
fn column_contents<R>(
    ui: &mut egui::Ui,
    max: f32,
    room_below: f32,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    use super::theme;
    let pane = ui.available_width();
    let width = (pane - 2.0 * DETAIL_SIDE_MARGIN).clamp(0.0, max);
    ui.spacing_mut().item_spacing.y = 0.0;
    ui.add_space(theme::SCENE_TOP_INSET);
    let inner = ui
        .horizontal_top(|ui| {
            ui.add_space(((pane - width) / 2.0).max(0.0));
            ui.vertical(|ui| {
                ui.set_width(width);
                add(ui)
            })
            .inner
        })
        .inner;
    ui.add_space(room_below);
    inner
}

/// The column's width: every input, Connect and each row of buttons end on
/// its right edge.
fn control_width(ui: &egui::Ui) -> f32 {
    super::theme::COLUMN_MAX.min(ui.available_width())
}

/// A scene's title ("Select a connection"), on one line.
fn scene_title(ui: &mut egui::Ui, title: &str) {
    let colours = super::theme::colours(ui);
    ui.add(
        egui::Label::new(
            egui::RichText::new(title)
                .color(colours.text)
                .size(super::theme::TEXT_FORM_TITLE),
        )
        .truncate(),
    );
}

/// The machine's heading, centred in its column: the name (its address
/// under the pointer), then its status when one is known, then a warning
/// when the connection is not encrypted.
fn machine_heading(
    ui: &mut egui::Ui,
    name: &str,
    address: &str,
    status: Option<(String, egui::Color32)>,
    use_tls: bool,
) {
    use super::theme;
    let colours = theme::colours(ui);
    ui.vertical_centered(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.add(
            egui::Label::new(
                egui::RichText::new(name)
                    .color(colours.text)
                    .size(MACHINE_TITLE),
            )
            .truncate()
            .sense(egui::Sense::hover()),
        )
        .on_hover_text(address);
        if let Some((phrase, ink)) = status {
            ui.add_space(theme::TITLE_TO_SUBTITLE + 2.0);
            ui.label(
                egui::RichText::new(phrase)
                    .color(ink)
                    .size(theme::TEXT_BODY),
            );
        }
        if let Some(warning) = encryption_warning(use_tls) {
            ui.add_space(theme::TITLE_TO_SUBTITLE);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(warning)
                        .color(colours.warning)
                        .size(theme::TEXT_LABEL),
                )
                .wrap(),
            );
        }
    });
}

/// The heading's warning about encryption: nothing while the connection is
/// encrypted (every connection is, unless insecure mode turned it off).
const fn encryption_warning(use_tls: bool) -> Option<&'static str> {
    if use_tls {
        None
    } else {
        Some("\u{26a0} Not encrypted (insecure mode)")
    }
}

/// A quiet option under Connect.
fn quiet_option(ui: &mut egui::Ui, value: &mut bool, label: &str) -> egui::Response {
    let colours = super::theme::colours(ui);
    ui.scope(|ui| {
        // Its box as a secondary control's: the field, its border, the
        // tints under the pointer; its label keeps its colour.
        let visuals = ui.visuals_mut();
        super::widgets::secondary_visuals(visuals);
        for state in [
            &mut visuals.widgets.inactive,
            &mut visuals.widgets.hovered,
            &mut visuals.widgets.active,
        ] {
            state.corner_radius = egui::CornerRadius::same(3);
        }
        ui.checkbox(
            value,
            egui::RichText::new(label)
                .color(colours.text2)
                .size(super::theme::TEXT_LABEL),
        )
    })
    .inner
}

/// The connection test's report, when there is one.
fn test_report(
    ui: &mut egui::Ui,
    test: Option<&super::connection_test::ConnectionTest>,
) -> Option<super::connection_test::ConnectionTestAction> {
    let test = test?;
    ui.add_space(super::theme::SPACE_SCALE[3]);
    super::connection_test::render(ui, test)
}

/// The link that hands a connection's Displays back to Settings.
pub const USE_APP_DEFAULT: &str = "Use app default";

/// The machine column's line for a connection that keeps its own Displays
/// choice ("Displays: Windowed (this connection)"); none while it follows
/// Settings.
#[must_use]
pub fn own_displays_line(overrides: &crate::overrides::ConnectionOverrides) -> Option<String> {
    match overrides.displays_mode {
        crate::overrides::Override::Own(mode) => {
            Some(format!("Displays: {} (this connection)", mode.label()))
        }
        crate::overrides::Override::Inherit => None,
    }
}

/// Which credentials a machine's column asks for: the password unless the
/// user's own session resumes (or a resume asked for it), the username
/// unless resuming with one remembered.
fn asks_for(status: &HostPresence, username: &str, asked: bool) -> (bool, bool) {
    let password = !status.resumes() || asked;
    let username = password || username.trim().is_empty();
    (username, password)
}

/// The username and password of the column, bound to Home's draft, the
/// sign-in outcome under its field. Whether Return was pressed in one.
fn credentials(
    ui: &mut egui::Ui,
    state: &mut HomeViewState<'_>,
    connection_id: Option<&str>,
    (show_username, show_password): (bool, bool),
) -> bool {
    use super::theme;
    let focus = state.sign_in.take_focus();
    let mut lost = false;
    if show_username {
        let username = credential_field(ui, "Username", &mut state.direct.username, false);
        if focus == Some(SignInField::Username) {
            username.request_focus();
        }
        if username.changed() {
            state.sign_in.credentials_edited();
        }
        lost |= username.lost_focus();
        field_feedback(
            ui,
            state
                .sign_in
                .under_field(connection_id, SignInField::Username),
        );
        if show_password {
            ui.add_space(theme::FIELD_TO_LABEL);
        }
    }
    if show_password {
        let password = credential_field(ui, "Password", &mut state.direct.password, true);
        if focus == Some(SignInField::Password) {
            password.request_focus();
        }
        if password.changed() {
            state.sign_in.credentials_edited();
        }
        lost |= password.lost_focus();
        field_feedback(
            ui,
            state
                .sign_in
                .under_field(connection_id, SignInField::Password),
        );
    }
    submits_on_return(lost, ui.input(|input| input.key_pressed(egui::Key::Enter)))
}

fn detail_connection(
    ui: &mut egui::Ui,
    state: &mut HomeViewState<'_>,
    idx: usize,
    status: &HostPresence,
) -> Option<HomeAction> {
    use super::theme;
    let mut action = None;
    let saved = state.saved_connections[idx].clone();
    let colours = theme::colours(ui);
    // A resume that asked for a password keeps it asked for.
    let asked = state
        .sign_in
        .feedback()
        .is_some_and(|feedback| feedback.connection_id.as_deref() == Some(saved.id.as_str()));

    detail_column(ui, "detail_scroll", 24.0, |ui| {
        let address = display_address(ui.ctx(), &saved.host, saved.port);
        let line = status
            .phrase()
            .map(|phrase| (phrase, status_colour(&colours, status)));
        machine_heading(ui, &saved.name, &address, line, saved.use_tls);
        // A test started from the row's ⋮ menu reports here.
        if let Some(panel) = test_report(ui, state.connection_test) {
            action = Some(HomeAction::ConnectionTest(panel));
        }
        ui.add_space(theme::GROUP_GAP);

        let asks = asks_for(status, &state.direct.username, asked);
        let returned = credentials(ui, state, Some(&saved.id), asks);
        if asks.0 || asks.1 {
            ui.add_space(theme::GROUP_GAP);
        }

        // Never disabled: with a field empty, a click does what Return
        // does, the hint under the field and the focus in it.
        if super::widgets::primary_button(ui, status.verb(), control_width(ui)).clicked()
            || returned
        {
            let draft = card_draft(state.direct, &saved);
            action = Some(HomeAction::Connect(draft));
        }
        ui.add_space(theme::CONNECT_TO_OPTIONS);
        let _ = quiet_option(ui, state.remember_username, "Remember username");
        ui.add_space(theme::OPTION_GAP);
        let _ = quiet_option(ui, &mut state.direct.auto_reconnect, "Auto-reconnect");
        // Only a connection that keeps its own Displays choice says so,
        // with the way back to Settings beside it.
        if let Some(line) = own_displays_line(&saved.settings.overrides) {
            ui.add_space(theme::OPTION_GAP);
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new(line)
                        .color(colours.text2)
                        .size(theme::TEXT_LABEL),
                );
                if super::widgets::text_button(ui, USE_APP_DEFAULT, false).clicked() {
                    action = Some(HomeAction::FollowAppDisplays(idx));
                }
            });
        }
        ui.add_space(theme::CONNECT_TO_OPTIONS);
        ui.vertical_centered(|ui| {
            if super::widgets::text_button(ui, "Edit", false).clicked() {
                action = Some(HomeAction::Edit(idx));
            }
        });
    });

    action
}

// ── Add / Edit form ───────────────────────────────────────────────────────────

/// What the connection form keeps between frames (in egui's memory, gone
/// once no form shows): which form it is and whether Save was tried (what
/// is missing shows from then on).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FormMemory {
    form: Option<usize>,
    attempted: bool,
}

fn form_memory_id() -> egui::Id {
    egui::Id::new("home_connection_form")
}

/// The fields of the form the first invalid one belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormField {
    Address,
    Username,
}

/// The form's grid in a pane `pane` wide: its columns' width and whether
/// they sit side by side (below [`FORM_STACK_BELOW`] they stack).
fn form_grid(pane: f32) -> (f32, bool) {
    let side_by_side = pane >= FORM_STACK_BELOW;
    let column = if side_by_side {
        FORM_COLUMN_W
    } else {
        (pane - 2.0 * DETAIL_SIDE_MARGIN).clamp(0.0, super::theme::COLUMN_MAX)
    };
    (column, side_by_side)
}

/// A section title of the form ("Connection", "On this machine").
fn section_title(ui: &mut egui::Ui, title: &str) {
    let colours = super::theme::colours(ui);
    ui.label(
        egui::RichText::new(title)
            .color(colours.text)
            .size(SECTION_TITLE),
    );
    ui.add_space(super::theme::SPACE_SCALE[3]);
}

#[allow(
    clippy::too_many_lines,
    reason = "one form, top to bottom, as it reads on screen"
)]
fn connection_form(
    ui: &mut egui::Ui,
    state: &mut HomeViewState<'_>,
    editing_index: Option<usize>,
) -> Option<HomeAction> {
    use super::theme;
    let mut action = None;
    let is_edit = editing_index.is_some();
    let platform = super::wording::deck_platform(ui.ctx());
    let colours = theme::colours(ui);
    // Esc cancels the form, as Cancel does, and Enter saves it, unless the
    // key closes an open list or a box over the form: read before the form
    // draws, while the list that the key closes this pass still counts as
    // open. Enter on a focused picker opens it, and on a focused button is
    // that button's.
    let form_owns_keys = !state.overlay_open
        && !egui::Popup::is_any_open(ui.ctx())
        && !super::list_picker::any_open(ui.ctx())
        && ui.ctx().memory(|memory| memory.top_modal_layer().is_none());
    let escape_cancels = form_owns_keys && ui.input(|input| input.key_pressed(egui::Key::Escape));
    let enter_saves = form_owns_keys
        && !super::list_picker::has_focus(ui.ctx())
        && ui.input(|input| input.key_pressed(egui::Key::Enter));
    let mut memory = ui
        .data(|data| data.get_temp::<FormMemory>(form_memory_id()))
        .filter(|memory| memory.form == editing_index)
        .unwrap_or(FormMemory {
            form: editing_index,
            attempted: false,
        });
    let address = parse_address(state.form_host);
    let username_ok =
        state.form_username.trim().is_empty() || validate_username(state.form_username).is_some();
    let test_running = state
        .connection_test
        .is_some_and(super::connection_test::ConnectionTest::running);
    let (mut submit, mut test) = (false, false);
    let mut fields = Vec::new();
    let (column, side_by_side) = form_grid(ui.available_width());
    let form_width = if side_by_side {
        2.0 * column + FORM_GUTTER
    } else {
        column
    };

    pane_column(ui, "form_scroll", form_width, 24.0, |ui| {
        ui.label(
            egui::RichText::new(if is_edit {
                "Edit connection"
            } else {
                "New connection"
            })
            .color(colours.text)
            .size(FORM_TITLE),
        );
        ui.add_space(theme::TITLE_TO_FIELDS);

        let mut connection = |ui: &mut egui::Ui, fields: &mut Vec<(FormField, egui::Response)>| {
            ui.set_width(column);
            section_title(ui, "Connection");
            let field = form_field(
                ui,
                "Address",
                state.form_host,
                column,
                "Name or IP address, and :port if not 18444",
            );
            fields.push((FormField::Address, field));
            if memory.attempted
                && let Err(problem) = address
            {
                field_problem(ui, problem, column);
            }
            ui.add_space(theme::FIELD_TO_LABEL);
            // An empty name saves as the address, which its placeholder
            // shows.
            let name_hint = match state.form_host.trim() {
                "" => "The address".to_owned(),
                typed => typed.to_owned(),
            };
            let _ = form_field(ui, "Name (optional)", state.form_name, column, &name_hint);
            ui.add_space(theme::FIELD_TO_LABEL);
            let field = form_field(ui, "Username (optional)", state.form_username, column, "");
            fields.push((FormField::Username, field));
            if !username_ok {
                field_problem(ui, USERNAME_INVALID, column);
            }
        };
        let machine = |ui: &mut egui::Ui,
                       overrides: &mut crate::overrides::ConnectionOverrides,
                       app: crate::overrides::AppDefaults| {
            ui.set_width(column);
            section_title(ui, ON_THIS_MACHINE);
            on_this_machine_fields(ui, platform, app, overrides, column);
        };
        let app = state.app_defaults;
        if side_by_side {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.vertical(|ui| connection(ui, &mut fields));
                ui.add_space(FORM_GUTTER);
                ui.vertical(|ui| machine(ui, state.form_overrides, app));
            });
        } else {
            connection(ui, &mut fields);
            ui.add_space(theme::GROUP_GAP);
            machine(ui, state.form_overrides, app);
        }
        ui.add_space(theme::GROUP_GAP);

        // Test connection, a quiet text action, on the left; Cancel and
        // Save, one width, on the right in the platform's order, ending on
        // the form's right edge.
        let button_w = super::widgets::button_width(ui, &["Add connection", "Cancel", "Save"]);
        ui.allocate_ui_with_layout(
            egui::vec2(form_width, theme::PRIMARY_HEIGHT),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.set_width(form_width);
                ui.spacing_mut().item_spacing.x = theme::BUTTON_GAP;
                let label = if test_running {
                    "Testing\u{2026}"
                } else {
                    "Test connection"
                };
                if super::widgets::text_button(ui, label, test_running)
                    .on_hover_text(TEST_CONNECTION_TIP)
                    .clicked()
                {
                    test = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Right to left: the last button is laid out first.
                    for button in form_button_order(platform).into_iter().rev() {
                        match button {
                            FormButton::Save => {
                                let label = if is_edit { "Save" } else { "Add connection" };
                                if super::widgets::primary_button(ui, label, button_w).clicked() {
                                    submit = true;
                                }
                            }
                            FormButton::Cancel => {
                                if super::widgets::secondary_button(ui, "Cancel", button_w)
                                    .clicked()
                                {
                                    action = Some(HomeAction::CancelForm);
                                }
                            }
                        }
                    }
                });
            },
        );
        if let Some(panel) = test_report(ui, state.connection_test) {
            action = Some(HomeAction::ConnectionTest(panel));
        }
    });

    // Save and Enter never meet a greyed button: what is missing shows
    // under its field, and the first such field takes the focus.
    let first_problem = match (&address, username_ok) {
        (Err(_), _) => Some(FormField::Address),
        (Ok(_), false) => Some(FormField::Username),
        (Ok(_), true) => None,
    };
    let focus = |field: FormField| {
        if let Some((_, response)) = fields.iter().find(|(which, _)| *which == field) {
            response.request_focus();
        }
    };
    // Return on a focused Test connection tests: the button's own press
    // comes before the form's Return.
    if test {
        submit = false;
    }
    if action.is_none() && (submit || (enter_saves && !test)) {
        match (&address, first_problem) {
            (Ok((host, port)), None) => {
                let name = match state.form_name.trim() {
                    "" => state.form_host.trim().to_owned(),
                    name => name.to_owned(),
                };
                let username = state.form_username.trim().to_owned();
                let overrides = *state.form_overrides;
                action = Some(if let Some(index) = editing_index {
                    HomeAction::SaveEdit {
                        index,
                        name,
                        host: host.clone(),
                        port: *port,
                        username,
                        overrides,
                    }
                } else {
                    HomeAction::SaveNew {
                        name,
                        host: host.clone(),
                        port: *port,
                        username,
                        overrides,
                    }
                });
            }
            (_, problem) => {
                memory.attempted = true;
                if let Some(field) = problem {
                    focus(field);
                }
            }
        }
    }
    if action.is_none() && test {
        if let Ok((host, port)) = &address {
            action = Some(HomeAction::TestConnection {
                host: host.clone(),
                port: port.to_string(),
            });
        } else {
            memory.attempted = true;
            focus(FormField::Address);
        }
    }
    if action.is_none() && escape_cancels {
        action = Some(HomeAction::CancelForm);
    }
    ui.data_mut(|data| data.insert_temp(form_memory_id(), memory));
    action
}

/// The form's right column's title.
pub const ON_THIS_MACHINE: &str = "On this machine";

/// What is wrong with a field, under it, wrapped to its width.
fn field_problem(ui: &mut egui::Ui, problem: &str, width: f32) {
    let colours = super::theme::colours(ui);
    ui.add_space(super::theme::FIELD_TO_HELP);
    ui.scope(|ui| {
        ui.set_max_width(width);
        ui.add(
            egui::Label::new(
                egui::RichText::new(problem)
                    .color(colours.error)
                    .size(super::theme::TEXT_LABEL),
            )
            .wrap(),
        );
    });
}

// ── Small helpers ─────────────────────────────────────────────────────────────

/// A sign-in field: its label above it, the input below; the input's
/// response, so a sign-in outcome can focus it.
fn credential_field(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut String,
    password: bool,
) -> egui::Response {
    super::widgets::field_label(ui, label);
    let width = control_width(ui);
    super::widgets::text_field(ui, value, password, width, "")
}

/// What a saved connection's card connects with: what was typed on it, to
/// the address it has now ([`ConnectionDraft::for_saved`]). A draft of
/// another connection (a stand-in card, a deleted or unsaved one) keeps
/// only the username and password typed here.
fn card_draft(direct: &ConnectionDraft, saved: &SavedConnectionSummary) -> ConnectionDraft {
    direct.for_saved(saved)
}

/// Whether a credentials field submits: it lost its focus to Return (not
/// to Tab or a click elsewhere). Return in the username or password field
/// is Connect, on both Decks: a single-line field gives up its focus on
/// Return, and that press submits (an empty password then gets its hint,
/// as Connect does).
const fn submits_on_return(field_lost_focus: bool, return_pressed: bool) -> bool {
    field_lost_focus && return_pressed
}

/// A sign-in outcome under its field, aligned with the field, with the
/// technical reason behind "Details".
fn field_feedback(ui: &mut egui::Ui, feedback: Option<&SignInFeedback>) {
    let Some(feedback) = feedback else {
        return;
    };
    let colours = super::theme::colours(ui);
    // Under the field, at its left edge (the label is above it).
    ui.add_space(super::theme::FIELD_TO_HELP);
    ui.vertical(|ui| {
        ui.set_max_width(super::theme::COLUMN_MAX);
        ui.add(
            egui::Label::new(
                egui::RichText::new(&feedback.message)
                    .color(colours.error)
                    .size(super::theme::TEXT_LABEL),
            )
            .wrap(),
        );
        details(ui, feedback.detail.as_deref(), false);
    });
}

/// The technical reason under "Details": collapsed under a field, open in
/// the acknowledgement box, whose outcome is the host's reason to read.
fn details(ui: &mut egui::Ui, detail: Option<&str>, open: bool) {
    let Some(detail) = detail else {
        return;
    };
    let colours = super::theme::colours(ui);
    egui::CollapsingHeader::new(
        egui::RichText::new("Details")
            .size(super::theme::TEXT_LABEL)
            .color(colours.text2),
    )
    .id_salt(("sign_in_details", open))
    .default_open(open)
    .show(ui, |ui| {
        ui.add(
            egui::Label::new(
                egui::RichText::new(detail)
                    .color(colours.text2)
                    .size(super::theme::TEXT_LABEL),
            )
            .selectable(true)
            .wrap(),
        );
    });
}

/// The card of a connection that is not saved (`--connect`) after its
/// sign-in failed: its name, username and password, the outcome under its
/// field, and Connect.
fn detail_unsaved(ui: &mut egui::Ui, state: &mut HomeViewState<'_>) -> Option<HomeAction> {
    let mut action = None;
    detail_column(ui, "detail_scroll", 24.0, |ui| {
        let address = display_address(ui.ctx(), &state.direct.host, state.direct.port);
        let name = state.direct.display_name().to_owned();
        machine_heading(ui, &name, &address, None, state.direct.use_tls);
        ui.add_space(super::theme::GROUP_GAP);
        let returned = credentials(ui, state, None, (true, true));
        ui.add_space(super::theme::GROUP_GAP);
        if super::widgets::primary_button(ui, "Connect", control_width(ui)).clicked() || returned {
            action = Some(HomeAction::Connect(state.direct.clone()));
        }
    });
    action
}

/// The acknowledgement box for an outcome that is not about a field.
/// Returns whether it was acknowledged (OK, Enter or Escape).
fn render_acknowledgement(ctx: &egui::Context, feedback: Option<&SignInFeedback>) -> bool {
    let Some(feedback) = feedback else {
        return false;
    };
    let mut acknowledged = false;
    let modal = egui::Modal::new(egui::Id::new("sign_in_acknowledgement")).show(ctx, |ui| {
        let colours = super::theme::colours(ui);
        ui.set_max_width(380.0);
        ui.add(
            egui::Label::new(
                egui::RichText::new(&feedback.message)
                    .color(colours.text)
                    .size(super::theme::TEXT_BODY),
            )
            .wrap(),
        );
        ui.add_space(6.0);
        details(ui, feedback.detail.as_deref(), true);
        ui.add_space(10.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let width = super::widgets::button_width(ui, &["OK"]);
            if super::widgets::primary_button(ui, "OK", width).clicked() {
                acknowledged = true;
            }
        });
    });
    acknowledged || modal.should_close() || ctx.input(|input| input.key_pressed(egui::Key::Enter))
}

/// The form's "On this machine" settings: the ⌘ key, the Option key (a Mac
/// Deck), Typing and Displays, each a list picker that follows Settings
/// until the connection keeps its own ("App default (Characters)",
/// [`super::input_choices`]), `width` wide.
fn on_this_machine_fields(
    ui: &mut egui::Ui,
    platform: super::wording::DeckPlatform,
    app: crate::overrides::AppDefaults,
    overrides: &mut crate::overrides::ConnectionOverrides,
    width: f32,
) {
    use super::input_choices as choices;
    use super::wording::DeckPlatform;

    let field = choices::command_key_field(platform, overrides);
    override_picker(
        ui,
        "connection_keyboard_shortcuts",
        &field,
        &mut overrides.keyboard_shortcuts,
        width,
    );
    if platform == DeckPlatform::Mac {
        ui.add_space(super::theme::FIELD_TO_LABEL);
        let field = choices::option_key_field(app, overrides);
        override_picker(
            ui,
            "connection_option_keys",
            &field,
            &mut overrides.option_keys,
            width,
        );
    }
    ui.add_space(super::theme::FIELD_TO_LABEL);
    let field = choices::typing_field(app, overrides);
    override_picker(
        ui,
        "connection_keyboard_mode",
        &field,
        &mut overrides.typing_mode,
        width,
    );
    ui.add_space(super::theme::FIELD_TO_LABEL);
    let field = choices::displays_field(platform, app, overrides);
    override_picker(
        ui,
        "connection_displays_mode",
        &field,
        &mut overrides.displays_mode,
        width,
    );
}

/// One own-value setting as a list picker, `width` wide: its title, the
/// choices ("App default (…)" first) and one line on what the value in
/// force does.
fn override_picker<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    id: &str,
    field: &super::input_choices::OverrideField<T>,
    value: &mut crate::overrides::Override<T>,
    width: f32,
) {
    let _ = super::list_picker::list_picker(
        ui,
        id,
        Some(field.title),
        value,
        &field.choices,
        Some(field.line.as_str()),
        width,
    );
}

/// A labelled input of the add/edit form, `width` wide: the label above
/// it, `hint` in it while it is empty.
fn form_field(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut String,
    width: f32,
    hint: &str,
) -> egui::Response {
    super::widgets::field_label(ui, label);
    super::widgets::text_field(ui, value, false, width, hint)
}

fn render_operator_notice(ui: &mut egui::Ui, notice: Option<&str>) -> bool {
    let Some(notice) = notice else {
        return false;
    };
    let colours = super::theme::colours(ui);
    egui::Frame::new()
        .fill(colours.panel)
        .stroke(egui::Stroke::new(1.0, colours.border))
        .inner_margin(egui::Margin::symmetric(24, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(egui::RichText::new(notice).color(colours.warning).strong());
        });
    true
}

#[cfg(test)]
mod tests {
    use super::*;

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

    use crate::settings::SecurityMode;
    use crate::sign_in::SignInState;

    fn studio() -> Vec<SavedConnectionSummary> {
        vec![SavedConnectionSummary {
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
        }]
    }

    /// Home's borrowed state for one test, over fields it owns.
    struct HomeFixture {
        saved: Vec<SavedConnectionSummary>,
        direct: ConnectionDraft,
        search: String,
        add: bool,
        editing: Option<usize>,
        form: [String; 3],
        overrides: crate::overrides::ConnectionOverrides,
        remember: bool,
        sign_in: SignInState,
        overlay_open: bool,
        notice: Option<HomeNotice>,
    }

    impl HomeFixture {
        fn new() -> Self {
            Self {
                saved: studio(),
                direct: ConnectionDraft::new(ConnectionKind::DirectMachine),
                search: String::new(),
                add: false,
                editing: None,
                form: Default::default(),
                overrides: crate::overrides::ConnectionOverrides::default(),
                remember: false,
                sign_in: SignInState::default(),
                overlay_open: false,
                notice: None,
            }
        }

        fn state(&mut self) -> HomeViewState<'_> {
            let [name, host, user] = &mut self.form;
            HomeViewState {
                direct: &mut self.direct,
                saved_connections: &self.saved,
                search: &mut self.search,
                operator_notice: None,
                show_add_form: &mut self.add,
                editing_index: &mut self.editing,
                form_name: name,
                form_host: host,
                form_username: user,
                form_overrides: &mut self.overrides,
                app_defaults: crate::overrides::AppDefaults {
                    displays_mode: crate::settings::DisplaysMode::SinglePrimary,
                    ..crate::overrides::AppDefaults::default()
                },
                sign_in: &mut self.sign_in,
                overlay_open: self.overlay_open,
                remember_username: &mut self.remember,
                connection_test: None,
                notice: &mut self.notice,
            }
        }

        fn apply(&mut self, action: HomeAction) -> HomeOutcome {
            let repository = crate::settings::ClientConfigRepository::new(None);
            apply_home_action(&mut self.state(), action, &repository, SecurityMode::High)
        }

        /// Every text Home draws in one pass.
        fn texts(&mut self) -> Vec<String> {
            fn collect(shape: &egui::Shape, texts: &mut Vec<String>) {
                match shape {
                    egui::Shape::Text(text) => texts.push(text.galley.job.text.clone()),
                    egui::Shape::Vec(shapes) => {
                        for shape in shapes {
                            collect(shape, texts);
                        }
                    }
                    _ => {}
                }
            }
            let ctx = egui::Context::default();
            let mut state = self.state();
            let output = ctx.run_ui(egui::RawInput::default(), |ui| {
                let _ = render_home(ui, &mut state);
            });
            let mut texts = Vec::new();
            for clipped in &output.shapes {
                collect(&clipped.shape, &mut texts);
            }
            texts
        }

        /// One pass of Home on `ctx` with these input events.
        fn pass(&mut self, ctx: &egui::Context, events: Vec<egui::Event>) -> Option<HomeAction> {
            let mut action = None;
            let mut state = self.state();
            let _ = ctx.run_ui(
                egui::RawInput {
                    events,
                    ..egui::RawInput::default()
                },
                |ui| action = render_home(ui, &mut state),
            );
            action
        }

        fn frame(&mut self) -> Option<HomeAction> {
            let mut action = None;
            let mut state = self.state();
            egui::__run_test_ui(|ui| action = render_home(ui, &mut state));
            action
        }
    }

    #[test]
    fn an_unsaved_connection_keeps_its_own_card_to_sign_in_again() {
        let mut home = HomeFixture::new();
        // A Deck started with `--connect`: nothing saved is selected.
        let mut quick = ConnectionDraft::new(ConnectionKind::DirectMachine);
        quick.host = "203.0.113.20".to_owned();
        quick.username = "artist".to_owned();
        home.direct =
            home.sign_in
                .return_home(crate::sign_in::SignInFailure::PasswordRequired, quick, None);
        // Under the unsaved connection's own card, once, and not moved
        // under the first saved connection.
        assert_eq!(
            home.texts()
                .iter()
                .filter(|text| *text == "Enter your password to connect.")
                .count(),
            1
        );
        let action = home.frame();
        assert!(action.is_none(), "{action:?}");
        assert!(home.direct.saved_connection_id.is_none());
        assert_eq!(home.sign_in.take_focus(), None, "its card took the focus");
        // Connect there tries the same host again with the password.
        home.direct.password = "secret".to_owned();
        let outcome = home.apply(HomeAction::Connect(home.direct.clone()));
        assert!(
            matches!(outcome.effect, HomeEffect::Connect(draft) if draft.host == "203.0.113.20")
        );
        // Once the outcome is gone, Home selects the first connection as before.
        assert!(matches!(
            home.frame(),
            Some(HomeAction::SelectConnection(0))
        ));
    }

    #[test]
    fn a_sign_in_error_is_never_a_banner_and_goes_when_a_form_opens_or_the_selection_changes() {
        let wrong = "Wrong username or password.";
        let count =
            |home: &mut HomeFixture| home.texts().iter().filter(|text| *text == wrong).count();
        let mut home = HomeFixture::new();
        let _ = home.apply(HomeAction::SelectConnection(0));
        home.direct = home.sign_in.return_home(
            crate::sign_in::SignInFailure::CredentialsRefused,
            home.direct.clone(),
            None,
        );
        // Under the password only: no second copy above Home.
        assert_eq!(count(&mut home), 1);
        // New Connection: the outcome goes with the card it was under.
        let _ = home.apply(HomeAction::BeginAdd);
        assert!(home.sign_in.feedback().is_none());
        assert_eq!(count(&mut home), 0);
        let _ = home.apply(HomeAction::CancelForm);
        assert_eq!(count(&mut home), 0, "it does not come back after Cancel");
        // Edit Connection likewise.
        home.direct = home.sign_in.return_home(
            crate::sign_in::SignInFailure::CredentialsRefused,
            home.direct.clone(),
            None,
        );
        let _ = home.apply(HomeAction::Edit(0));
        assert!(home.sign_in.feedback().is_none());
        // A connection Home selects after saving it is covered against a
        // real folder in `tests/home_actions.rs`.
    }

    #[test]
    fn return_in_the_password_field_connects_or_asks_for_the_password() {
        let enter = || {
            vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }]
        };
        let mut home = HomeFixture::new();
        let _ = home.apply(HomeAction::SelectConnection(0));
        home.direct.username = "artist".to_owned();
        let ctx = egui::Context::default();
        // Connect with no password focuses it (the first pass draws it).
        let _ = home.apply(HomeAction::Connect(home.direct.clone()));
        assert!(home.pass(&ctx, Vec::new()).is_none());
        // Return there, still empty: Connect, which asks for the password.
        let action = home.pass(&ctx, enter());
        assert!(matches!(&action, Some(HomeAction::Connect(draft)) if draft.password.is_empty()));
        let outcome = home.apply(action.unwrap());
        assert!(matches!(outcome.effect, HomeEffect::None));
        assert!(
            home.sign_in
                .under_field(Some("studio"), SignInField::Password)
                .is_some()
        );
        // Typed, Return connects.
        assert!(home.pass(&ctx, Vec::new()).is_none());
        home.direct.password = "secret".to_owned();
        let action = home.pass(&ctx, enter());
        let outcome = home.apply(action.expect("Return connects"));
        assert!(matches!(outcome.effect, HomeEffect::Connect(draft) if draft.password == "secret"));
        // Return with no field focused submits nothing.
        assert!(home.pass(&ctx, Vec::new()).is_none());
        assert!(home.pass(&ctx, enter()).is_none());
        assert!(!submits_on_return(false, true));
        assert!(
            !submits_on_return(true, false),
            "Tab or a click is not Return"
        );
    }

    fn render_box() -> SavedConnectionSummary {
        SavedConnectionSummary {
            id: "render".to_owned(),
            name: "Render".to_owned(),
            host: "203.0.113.11".to_owned(),
            ..studio().remove(0)
        }
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn a_connection_s_certificate_trust_never_follows_the_selection_to_another() {
        use crate::runtime::tls::TlsTrustConfig;
        let mut home = HomeFixture::new();
        home.saved.push(render_box());
        // A command line's pin for Studio, after its attempt failed.
        let pinned = || Some(TlsTrustConfig::pinned_certificate([7; 32]));
        home.direct.saved_connection_id = Some("studio".to_owned());
        home.direct.host = "203.0.113.10".to_owned();
        home.direct.tls_trust = pinned();
        // A retry of Studio keeps it.
        let _ = home.apply(HomeAction::SelectConnection(0));
        assert!(home.direct.tls_trust.is_some());
        assert!(card_draft(&home.direct, &home.saved[0]).tls_trust.is_some());
        // Render's card never dials with it, and selecting Render starts
        // afresh.
        assert!(card_draft(&home.direct, &home.saved[1]).tls_trust.is_none());
        let _ = home.apply(HomeAction::SelectConnection(1));
        assert!(home.direct.tls_trust.is_none());
        assert_eq!(home.direct.host, "203.0.113.11");
        // An unsaved quick-connect's pin does not reach a saved card, nor
        // follows an import's selection.
        home.direct = ConnectionDraft::new(ConnectionKind::DirectMachine);
        home.direct.host = "203.0.113.10".to_owned();
        home.direct.tls_trust = pinned();
        assert!(card_draft(&home.direct, &home.saved[0]).tls_trust.is_none());
        super::super::profile::select(&mut home.direct, &mut home.sign_in, &home.saved, "studio");
        assert!(home.direct.tls_trust.is_none());
    }

    #[test]
    fn connect_after_an_edit_dials_the_saved_address_without_the_old_password() {
        let root = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .join(format!("arcen-home-edit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repository = crate::settings::ClientConfigRepository::at(root.clone());
        let store = repository.add_connection(studio().remove(0)).unwrap();
        let mut home = HomeFixture::new();
        home.saved = store.connections;
        let _ = home.apply(HomeAction::SelectConnection(0));
        home.direct.password = "typed-for-the-old-host".to_owned();
        #[cfg(feature = "runtime")]
        {
            home.direct.tls_trust = Some(crate::runtime::tls::TlsTrustConfig::pinned_certificate(
                [7; 32],
            ));
        }
        let outcome = apply_home_action(
            &mut home.state(),
            HomeAction::SaveEdit {
                index: 0,
                name: "Studio".to_owned(),
                host: "203.0.113.50".to_owned(),
                port: 18444,
                username: String::new(),
                overrides: crate::overrides::ConnectionOverrides::default(),
            },
            &repository,
            SecurityMode::High,
        );
        home.saved = outcome.store.expect("saved").connections;
        assert_eq!(home.direct.host, "203.0.113.50");
        assert!(home.direct.password.is_empty());
        #[cfg(feature = "runtime")]
        assert!(home.direct.tls_trust.is_none());
        // Even a draft that kept the old address dials the saved one.
        let mut stale = home.direct.clone();
        stale.host = "203.0.113.10".to_owned();
        stale.port = 18_445;
        let draft = card_draft(&stale, &home.saved[0]);
        assert_eq!((draft.host.as_str(), draft.port), ("203.0.113.50", 18444));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_form_over_an_unsaved_connection_s_card_does_not_hand_its_draft_to_a_saved_one() {
        let mut home = HomeFixture::new();
        let mut quick = ConnectionDraft::new(ConnectionKind::DirectMachine);
        quick.host = "203.0.113.20".to_owned();
        quick.username = "artist".to_owned();
        home.direct =
            home.sign_in
                .return_home(crate::sign_in::SignInFailure::PasswordRequired, quick, None);
        let _ = home.apply(HomeAction::BeginAdd);
        let _ = home.apply(HomeAction::CancelForm);
        // The unsaved connection's card comes back; no saved card stands in.
        assert!(home.frame().is_none());
        assert!(home.sign_in.keeps_unsaved_connection());

        // A saved card shown with another connection's draft dials its own
        // address, with what was typed on it.
        let mut stray = ConnectionDraft::new(ConnectionKind::DirectMachine);
        stray.host = "203.0.113.99".to_owned();
        stray.port = 1;
        stray.username = "artist".to_owned();
        let draft = card_draft(&stray, &home.saved[0]);
        assert_eq!(draft.saved_connection_id.as_deref(), Some("studio"));
        assert_eq!((draft.host.as_str(), draft.port), ("203.0.113.10", 18444));
        assert_eq!(draft.username, "artist");
    }

    #[test]
    fn connect_wipes_the_password_from_home() {
        let mut home = HomeFixture::new();
        let _ = home.apply(HomeAction::SelectConnection(0));
        home.direct.username = "artist".to_owned();
        home.direct.password = "secret".to_owned();
        let outcome = home.apply(HomeAction::Connect(home.direct.clone()));
        // The attempt carries it; the field is empty, so after Disconnect
        // retyping it does not append to the old one.
        assert!(
            matches!(&outcome.effect, HomeEffect::Connect(draft) if draft.password == "secret")
        );
        assert!(home.direct.password.is_empty());
        assert_eq!(home.direct.username, "artist");
    }

    #[test]
    fn editing_a_credential_clears_the_hint_under_it() {
        let mut home = HomeFixture::new();
        let _ = home.apply(HomeAction::SelectConnection(0));
        home.direct.username = "artist".to_owned();
        let _ = home.apply(HomeAction::Connect(home.direct.clone()));
        assert!(home.sign_in.feedback().is_some());
        home.sign_in.credentials_edited();
        assert!(home.sign_in.feedback().is_none());
    }

    #[test]
    fn an_address_takes_its_port_after_a_colon_and_defaults_to_18444() {
        for (typed, host, port) in [
            ("203.0.113.10", "203.0.113.10", 18444),
            (" pier.example.internal ", "pier.example.internal", 18444),
            (
                "pier.example.internal:19444",
                "pier.example.internal",
                19444,
            ),
            ("203.0.113.10:1", "203.0.113.10", 1),
            ("[2001:db8::7]:65535", "2001:db8::7", 65535),
            ("[2001:db8::7]", "2001:db8::7", 18444),
            ("2001:db8::7", "2001:db8::7", 18444),
        ] {
            assert_eq!(parse_address(typed), Ok((host.to_owned(), port)), "{typed}");
        }
        for (typed, problem) in [
            ("", ADDRESS_MISSING),
            ("   ", ADDRESS_MISSING),
            ("pier:0", PORT_INVALID),
            ("pier:65536", PORT_INVALID),
            ("pier:port", PORT_INVALID),
            ("[2001:db8::7]:", PORT_INVALID),
            ("[2001:db8::7]:99999", PORT_INVALID),
            ("2001:db8::7:x", ADDRESS_INVALID),
            ("[not-v6]:18444", ADDRESS_INVALID),
            ("a host", ADDRESS_INVALID),
        ] {
            assert_eq!(parse_address(typed), Err(problem), "{typed}");
        }
        // And back: the product port is left out, IPv6 is bracketed.
        assert_eq!(address_text("pier", 18444), "pier");
        assert_eq!(address_text("pier", 19444), "pier:19444");
        assert_eq!(address_text("2001:db8::7", 19444), "[2001:db8::7]:19444");
        assert_eq!(address_text("2001:db8::7", 18444), "2001:db8::7");
    }

    #[test]
    fn editing_a_connection_on_another_port_shows_it_in_the_address() {
        let mut home = HomeFixture::new();
        home.saved[0].port = 19444;
        let _ = home.apply(HomeAction::Edit(0));
        assert_eq!(home.form[1], "203.0.113.10:19444");
        home.saved[0].port = 18444;
        let _ = home.apply(HomeAction::Edit(0));
        assert_eq!(home.form[1], "203.0.113.10");
    }

    #[test]
    fn a_bad_address_shows_under_its_field_once_save_is_tried_and_will_not_save() {
        let ctx = egui::Context::default();
        let mut home = HomeFixture::new();
        let _ = home.apply(HomeAction::BeginAdd);
        home.form[1] = "203.0.113.10:18444203".to_owned();
        let says = |texts: &[(String, egui::Pos2)]| texts.iter().any(|(t, _)| t == PORT_INVALID);
        // Nothing is said before Save is tried.
        let (before, _) = home_frame(&ctx, &mut home, Vec::new());
        assert!(!says(&before));
        let (_, refused) = home_frame(&ctx, &mut home, key(egui::Key::Enter));
        assert!(refused.is_none(), "{refused:?}");
        let (after, _) = home_frame(&ctx, &mut home, Vec::new());
        assert!(says(&after), "{after:?}");
        // Fixed, it goes and the form saves.
        home.form[1] = "203.0.113.10:19444".to_owned();
        let (fixed, action) = home_frame(&ctx, &mut home, key(egui::Key::Enter));
        assert!(!says(&fixed));
        assert!(
            matches!(&action, Some(HomeAction::SaveNew { host, port: 19444, .. }) if host == "203.0.113.10"),
            "{action:?}"
        );
    }

    #[test]
    fn a_connection_with_no_name_saves_as_its_address() {
        let ctx = egui::Context::default();
        let mut home = HomeFixture::new();
        let _ = home.apply(HomeAction::BeginAdd);
        home.form = [
            String::new(),
            " pier.example.internal:19444 ".to_owned(),
            String::new(),
        ];
        let (shown, action) = home_frame(&ctx, &mut home, key(egui::Key::Enter));
        // The empty name's placeholder says so.
        assert!(
            shown
                .iter()
                .any(|(text, _)| text == "pier.example.internal:19444"),
            "{shown:?}"
        );
        assert!(
            matches!(
                &action,
                Some(HomeAction::SaveNew { name, host, port: 19444, .. })
                    if name == "pier.example.internal:19444" && host == "pier.example.internal"
            ),
            "{action:?}"
        );
    }

    #[test]
    fn the_form_s_buttons_follow_each_platform_s_order() {
        use super::super::wording::DeckPlatform;
        assert_eq!(
            form_button_order(DeckPlatform::Mac),
            [FormButton::Cancel, FormButton::Save]
        );
        assert_eq!(
            form_button_order(DeckPlatform::Pc),
            [FormButton::Save, FormButton::Cancel]
        );
        // As drawn: left to right on the action row.
        for platform in [DeckPlatform::Mac, DeckPlatform::Pc] {
            let ctx = egui::Context::default();
            super::super::wording::set_deck_platform(&ctx, platform);
            let mut home = HomeFixture::new();
            home.add = true;
            let (shown, _) = home_frame(&ctx, &mut home, Vec::new());
            let x = |label: &str| {
                shown
                    .iter()
                    .find(|(text, _)| text == label)
                    .map_or_else(|| panic!("no {label}: {shown:?}"), |(_, at)| at.x)
            };
            let (test, cancel, save) = (x("Test connection"), x("Cancel"), x("Add connection"));
            assert!(test < cancel.min(save), "{platform:?}");
            let order = if cancel < save {
                [FormButton::Cancel, FormButton::Save]
            } else {
                [FormButton::Save, FormButton::Cancel]
            };
            assert_eq!(order, form_button_order(platform), "{platform:?}");
        }
    }

    #[test]
    fn a_failed_sign_in_shows_under_the_password_of_its_own_card() {
        let mut home = HomeFixture::new();
        let _ = home.apply(HomeAction::SelectConnection(0));
        let mut attempt = home.direct.clone();
        attempt.password = "wrong".to_owned();
        home.direct = home.sign_in.return_home(
            crate::sign_in::SignInFailure::CredentialsRefused,
            attempt,
            Some("authentication failed: Invalid credentials".to_owned()),
        );
        assert!(home.direct.password.is_empty());
        assert!(
            home.sign_in
                .under_field(Some("studio"), SignInField::Password)
                .is_some()
        );
        assert!(home.frame().is_none());
        assert_eq!(home.sign_in.take_focus(), None, "the card took the focus");
    }

    #[test]
    fn connect_without_a_password_dials_nothing_and_asks_for_it() {
        let mut home = HomeFixture::new();
        let _ = home.apply(HomeAction::SelectConnection(0));
        home.direct.username = "artist".to_owned();
        let outcome = home.apply(HomeAction::Connect(home.direct.clone()));
        assert!(matches!(outcome.effect, HomeEffect::None));
        assert_eq!(
            home.sign_in
                .under_field(Some("studio"), SignInField::Password)
                .map(|feedback| feedback.message.as_str()),
            Some("Enter your password to connect.")
        );
        home.direct.password = "secret".to_owned();
        let outcome = home.apply(HomeAction::Connect(home.direct.clone()));
        assert!(matches!(outcome.effect, HomeEffect::Connect(_)));
        assert!(home.sign_in.feedback().is_none());
    }

    #[test]
    fn choosing_another_connection_clears_the_last_attempts_error() {
        let mut home = HomeFixture::new();
        let mut attempt = ConnectionDraft::new(ConnectionKind::DirectMachine);
        attempt.saved_connection_id = Some("elsewhere".to_owned());
        home.direct = home.sign_in.return_home(
            crate::sign_in::SignInFailure::SessionNotStarted,
            attempt,
            Some("the host refused the requested video: selects yuv444".to_owned()),
        );
        let _ = home.apply(HomeAction::SelectConnection(0));
        assert!(home.sign_in.feedback().is_none());
        // Re-selecting the connection the outcome belongs to keeps it.
        home.direct = home.sign_in.return_home(
            crate::sign_in::SignInFailure::CredentialsRefused,
            home.direct.clone(),
            None,
        );
        let _ = home.apply(HomeAction::SelectConnection(0));
        assert!(home.sign_in.feedback().is_some());
    }

    /// The "On this machine" group's text on `platform`.
    fn on_this_machine_text(
        platform: super::super::wording::DeckPlatform,
        overrides: &mut crate::overrides::ConnectionOverrides,
    ) -> Vec<String> {
        fn collect(shape: &egui::Shape, texts: &mut Vec<String>) {
            match shape {
                egui::Shape::Text(text) => texts.push(text.galley.job.text.clone()),
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| collect(shape, texts)),
                _ => {}
            }
        }
        let ctx = egui::Context::default();
        super::super::wording::set_deck_platform(&ctx, platform);
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            on_this_machine_fields(
                ui,
                platform,
                crate::overrides::AppDefaults::default(),
                overrides,
                360.0,
            );
        });
        let mut texts = Vec::new();
        for clipped in &output.shapes {
            collect(&clipped.shape, &mut texts);
        }
        texts
    }

    #[test]
    fn edit_connection_shows_what_a_connection_inherits_on_each_deck() {
        use super::super::wording::DeckPlatform;
        let mut overrides = crate::overrides::ConnectionOverrides::default();
        let mac = on_this_machine_text(DeckPlatform::Mac, &mut overrides);
        let app_displays = format!(
            "App default ({})",
            crate::overrides::AppDefaults::default()
                .displays_mode
                .label()
        );
        for wanted in [
            "⌘ key",
            "App default (Automatic)",
            "Option key",
            "Typing",
            "App default (Characters)",
            "Displays",
            app_displays.as_str(),
        ] {
            assert!(mac.iter().any(|text| text == wanted), "{wanted}: {mac:?}");
        }
        assert!(mac.iter().all(|text| !text.contains("AltGr")));
        let pc = on_this_machine_text(DeckPlatform::Pc, &mut overrides);
        assert!(pc.iter().any(|text| text == "Command key on a Mac host"));
        assert!(pc.iter().any(|text| text == "App default (Characters)"));
        assert!(pc.iter().all(|text| !text.contains("Option")), "{pc:?}");
        assert_eq!(overrides, crate::overrides::ConnectionOverrides::default());
    }

    #[test]
    fn long_names_are_shortened_by_characters_not_bytes() {
        let name = "日本語日本語日本語日本語日本語日本語日本語日本語";
        assert_eq!(shortened(name, 20, 22), format!("{}…", &name[..60]));
        assert_eq!(
            shortened("日本語日本語日本語日本語", 20, 22),
            "日本語日本語日本語日本語"
        );
        assert_eq!(
            shortened("👩‍👩‍👧‍👦".repeat(10).as_str(), 21, 24).chars().count(),
            22
        );
        assert_eq!(shortened("short", 20, 22), "short");
    }

    /// One Home pass with `events`; its text and the action taken.
    fn home_pass(
        home: &mut HomeFixture,
        events: Vec<egui::Event>,
    ) -> (Vec<String>, Option<HomeAction>) {
        fn texts(shape: &egui::Shape, out: &mut Vec<String>) {
            match shape {
                egui::Shape::Text(text) => out.push(text.galley.job.text.clone()),
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| texts(shape, out)),
                _ => {}
            }
        }
        let ctx = egui::Context::default();
        let mut action = None;
        let mut state = home.state();
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1120.0, 760.0),
                )),
                events,
                ..egui::RawInput::default()
            },
            |ui| action = render_home(ui, &mut state),
        );
        let mut shown = Vec::new();
        for clipped in &output.shapes {
            texts(&clipped.shape, &mut shown);
        }
        (shown, action)
    }

    /// One Home pass on `ctx` with `events`: every text drawn, where, and
    /// the action taken.
    fn home_frame(
        ctx: &egui::Context,
        home: &mut HomeFixture,
        events: Vec<egui::Event>,
    ) -> (Vec<(String, egui::Pos2)>, Option<HomeAction>) {
        fn texts(shape: &egui::Shape, out: &mut Vec<(String, egui::Pos2)>) {
            match shape {
                // The galley's own rectangle: the tests have no fonts, so a
                // glyph-bounds rectangle would be empty.
                egui::Shape::Text(text) => out.push((
                    text.galley.job.text.clone(),
                    text.galley.rect.translate(text.pos.to_vec2()).center(),
                )),
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| texts(shape, out)),
                _ => {}
            }
        }
        let mut action = None;
        let mut state = home.state();
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1120.0, 760.0),
                )),
                events,
                ..egui::RawInput::default()
            },
            |ui| action = render_home(ui, &mut state),
        );
        let mut shown = Vec::new();
        for clipped in &output.shapes {
            texts(&clipped.shape, &mut shown);
        }
        (shown, action)
    }

    fn key(key: egui::Key) -> Vec<egui::Event> {
        vec![egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }]
    }

    #[test]
    fn no_label_on_home_is_in_capitals() {
        let mut home = HomeFixture::new();
        let _ = home.apply(HomeAction::SelectConnection(0));
        let ctx = egui::Context::default();
        let (shown, _) = home_frame(&ctx, &mut home, Vec::new());
        for wanted in ["Username", "Password", "Remember username"] {
            assert!(
                shown.iter().any(|(text, _)| text == wanted),
                "{wanted}: {shown:?}"
            );
        }
        for (text, _) in &shown {
            let letters: Vec<char> = text.chars().filter(|c| c.is_alphabetic()).collect();
            assert!(
                letters.len() < 4 || letters.iter().any(|c| c.is_lowercase()),
                "tracked capitals: {text:?}"
            );
        }
        assert!(shown.iter().all(|(text, _)| text != "Credentials"));
    }

    #[test]
    fn the_header_warns_only_without_encryption_and_shows_no_label_table() {
        let warning = "\u{26a0} Not encrypted (insecure mode)";
        assert_eq!(encryption_warning(true), None);
        let ctx = egui::Context::default();
        let mut home = HomeFixture::new();
        let _ = home.apply(HomeAction::SelectConnection(0));
        let (shown, _) = home_frame(&ctx, &mut home, Vec::new());
        // The name and Edit; the address is the row's second line (no
        // status is known), never a line in the column.
        for wanted in ["Studio", "Edit"] {
            assert!(shown.iter().any(|(text, _)| text == wanted), "{wanted}");
        }
        let addresses: Vec<_> = shown
            .iter()
            .filter(|(text, _)| text == "203.0.113.10")
            .collect();
        assert_eq!(addresses.len(), 1, "{shown:?}");
        assert!(addresses[0].1.x < SIDEBAR_W, "in the list");
        // Select and connect: the test is in the row's ⋮ menu.
        for gone in [
            "Address",
            "Displays",
            "Encryption",
            warning,
            "Test connection",
        ] {
            assert!(shown.iter().all(|(text, _)| text != gone), "{gone}");
        }
        home.saved[0].use_tls = false;
        let (shown, _) = home_frame(&ctx, &mut home, Vec::new());
        assert!(shown.iter().any(|(text, _)| text == warning));
    }

    #[test]
    fn connect_is_enabled_without_a_password_and_its_click_asks_for_it() {
        let mut home = HomeFixture::new();
        let _ = home.apply(HomeAction::SelectConnection(0));
        home.direct.username = "artist".to_owned();
        let ctx = egui::Context::default();
        let (shown, _) = home_frame(&ctx, &mut home, Vec::new());
        let at = shown
            .iter()
            .find(|(text, _)| text == "Connect")
            .map_or_else(|| panic!("no Connect: {shown:?}"), |(_, at)| *at);
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let _ = home_frame(&ctx, &mut home, vec![egui::Event::PointerMoved(at)]);
        let _ = home_frame(&ctx, &mut home, vec![button(true)]);
        let (_, action) = home_frame(&ctx, &mut home, vec![button(false)]);
        let Some(action @ HomeAction::Connect(_)) = action else {
            panic!("the click did not connect: {action:?}");
        };
        // As Return does (#310): nothing is dialled, the hint is under the
        // password and the focus goes there.
        let outcome = home.apply(action);
        assert!(matches!(outcome.effect, HomeEffect::None));
        assert_eq!(
            home.sign_in
                .under_field(Some("studio"), SignInField::Password)
                .map(|feedback| feedback.message.as_str()),
            Some("Enter your password to connect.")
        );
        assert_eq!(home.sign_in.take_focus(), Some(SignInField::Password));
    }

    #[test]
    fn enter_saves_a_valid_connection_form() {
        let ctx = egui::Context::default();
        let mut home = HomeFixture::new();
        home.add = true;
        // Not while the form cannot be saved: the address asks for itself.
        let (_, action) = home_frame(&ctx, &mut home, key(egui::Key::Enter));
        assert!(action.is_none(), "{action:?}");
        let (shown, _) = home_frame(&ctx, &mut home, Vec::new());
        assert!(shown.iter().any(|(text, _)| text == ADDRESS_MISSING));
        home.form = [
            "Studio 2".to_owned(),
            "203.0.113.20".to_owned(),
            String::new(),
        ];
        let (_, action) = home_frame(&ctx, &mut home, key(egui::Key::Enter));
        assert!(
            matches!(&action, Some(HomeAction::SaveNew { name, .. }) if name == "Studio 2"),
            "{action:?}"
        );
        home.add = false;
        home.editing = Some(0);
        let (_, action) = home_frame(&ctx, &mut home, key(egui::Key::Enter));
        assert!(
            matches!(&action, Some(HomeAction::SaveEdit { index: 0, .. })),
            "{action:?}"
        );
    }

    #[test]
    fn the_plus_menu_adds_or_imports_and_no_add_button_sits_under_the_list() {
        assert_eq!(
            AddMenuItem::ALL.map(AddMenuItem::title),
            ["New connection\u{2026}", "Import from file\u{2026}"]
        );
        assert!(matches!(
            AddMenuItem::NewConnection.action(),
            HomeAction::BeginAdd
        ));
        assert!(matches!(
            AddMenuItem::ImportFromFile.action(),
            HomeAction::ImportProfile
        ));
        // With connections saved, nothing on Home says Add Connection or
        // Import: the ＋ menu does both.
        let mut home = HomeFixture::new();
        let _ = home.apply(HomeAction::SelectConnection(0));
        let (shown, _) = home_pass(&mut home, Vec::new());
        assert!(shown.iter().any(|text| text == "Connections"), "{shown:?}");
        for gone in ["+  Add Connection", "Add connection", "Import\u{2026}"] {
            assert!(shown.iter().all(|text| text != gone), "{gone}: {shown:?}");
        }
        // Only the empty state keeps its two buttons, once each.
        home.saved.clear();
        home.direct = ConnectionDraft::new(ConnectionKind::DirectMachine);
        let (shown, _) = home_pass(&mut home, Vec::new());
        for wanted in ["Add connection", "Import\u{2026}"] {
            assert_eq!(
                shown.iter().filter(|text| *text == wanted).count(),
                1,
                "{wanted}: {shown:?}"
            );
        }
    }

    #[test]
    fn command_n_opens_a_new_connection_on_home() {
        let press = || {
            vec![egui::Event::Key {
                key: egui::Key::N,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::COMMAND,
            }]
        };
        let mut home = HomeFixture::new();
        let (_, action) = home_pass(&mut home, press());
        assert!(matches!(action, Some(HomeAction::BeginAdd)), "{action:?}");
        // Not while a form is open.
        home.add = true;
        let (_, action) = home_pass(&mut home, press());
        assert!(!matches!(action, Some(HomeAction::BeginAdd)), "{action:?}");
        // Nor under the sign-in OK box: Return and Esc are its keys, and
        // nothing opens behind it.
        let mut home = HomeFixture::new();
        home.direct = home.sign_in.return_home(
            crate::sign_in::SignInFailure::HostRestarted,
            home.direct.clone(),
            None,
        );
        assert!(home.sign_in.acknowledgement().is_some());
        let (_, action) = home_pass(&mut home, press());
        assert!(!matches!(action, Some(HomeAction::BeginAdd)), "{action:?}");
    }

    #[test]
    fn a_selected_rows_text_keeps_4_5_to_1_on_its_tint() {
        use super::super::theme::{Palette, SELECTED_TINT_ALPHA, contrast_ratio};
        for colours in [Palette::light(), Palette::dark()] {
            assert_ne!(
                colours.selected, colours.accent,
                "a tint, not the solid accent"
            );
            for text in [colours.text, colours.text2] {
                let ratio = contrast_ratio(text, colours.selected);
                assert!(
                    ratio >= 4.5,
                    "{ratio:.2}:1 for {text:?} on {:?}",
                    colours.selected
                );
            }
        }
        // About 12-18 % of the accent.
        let share = f32::from(SELECTED_TINT_ALPHA) / 255.0;
        assert!((0.12..=0.18).contains(&share), "{share}");
    }

    #[test]
    fn esc_cancels_the_connection_form_unless_a_list_is_open() {
        fn press_escape(home: &mut HomeFixture, ctx: &egui::Context) -> Option<HomeAction> {
            let mut action = None;
            let input = egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..egui::RawInput::default()
            };
            let mut state = home.state();
            let _ = ctx.run_ui(input, |ui| action = render_home(ui, &mut state));
            action
        }
        for editing in [None, Some(0)] {
            let mut home = HomeFixture::new();
            home.add = editing.is_none();
            home.editing = editing;
            let ctx = egui::Context::default();
            assert!(matches!(
                press_escape(&mut home, &ctx),
                Some(HomeAction::CancelForm)
            ));
            // With a list open, Esc closes the list only.
            egui::Popup::open_id(&ctx, egui::Id::new("an open list"));
            assert!(press_escape(&mut home, &ctx).is_none());
        }
    }

    #[test]
    fn esc_with_a_help_window_open_closes_the_window_and_keeps_the_form() {
        use super::super::help_windows::{HelpWindow, HelpWindows};
        let escape = || egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..egui::RawInput::default()
        };
        let mut home = HomeFixture::new();
        home.add = true;
        let mut help = HelpWindows::default();
        help.open(HelpWindow::KeyboardShortcuts);
        let ctx = egui::Context::default();
        let mut action = None;
        let _ = ctx.run_ui(escape(), |ui| {
            // As a shell does: the window in front takes Esc first.
            assert_eq!(
                help.take_dismiss_key(ui.ctx()),
                Some(HelpWindow::KeyboardShortcuts)
            );
            home.overlay_open = help.modal();
            action = render_home(ui, &mut home.state());
        });
        assert!(action.is_none(), "{action:?}");
        assert!(!help.modal());
        // A window the key did not close (a text field had it) still keeps
        // Esc away from the form.
        home.overlay_open = true;
        let _ = ctx.run_ui(escape(), |ui| action = render_home(ui, &mut home.state()));
        assert!(action.is_none(), "{action:?}");
    }

    #[test]
    fn an_empty_home_says_what_to_do_and_offers_both_ways() {
        fn texts(shape: &egui::Shape, out: &mut Vec<String>) {
            match shape {
                egui::Shape::Text(text) => out.push(text.galley.job.text.clone()),
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| texts(shape, out)),
                _ => {}
            }
        }
        let ctx = egui::Context::default();
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            assert!(detail_empty(ui, true).is_none());
        });
        let mut shown = Vec::new();
        for clipped in &output.shapes {
            texts(&clipped.shape, &mut shown);
        }
        for wanted in [
            "No connections yet",
            EMPTY_HOME_LINE,
            "Add connection",
            "Import\u{2026}",
        ] {
            assert!(
                shown.iter().any(|text| text == wanted),
                "{wanted}: {shown:?}"
            );
        }
    }

    /// Every shape one Home pass on `ctx` draws, flattened, in a window
    /// of `size` (the Deck's default, under its 36-point bar).
    fn home_shapes(
        ctx: &egui::Context,
        home: &mut HomeFixture,
        size: egui::Vec2,
        events: Vec<egui::Event>,
    ) -> Vec<egui::Shape> {
        fn flatten(shape: egui::Shape, out: &mut Vec<egui::Shape>) {
            match shape {
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        flatten(shape, out);
                    }
                }
                shape => out.push(shape),
            }
        }
        let mut state = home.state();
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                events,
                ..egui::RawInput::default()
            },
            |ui| {
                let _ = render_home(ui, &mut state);
            },
        );
        let mut shapes = Vec::new();
        for clipped in output.shapes {
            flatten(clipped.shape, &mut shapes);
        }
        shapes
    }

    /// The controls a pass drew in `colours`: every outlined box (inputs,
    /// pickers, secondary buttons) and every filled primary box.
    fn controls(
        shapes: &[egui::Shape],
        colours: &super::super::theme::Palette,
    ) -> (Vec<egui::Rect>, Vec<egui::Rect>) {
        let (mut outlined, mut primary) = (Vec::new(), Vec::new());
        for shape in shapes {
            // A checkbox's small box and a panel's frame are not one of them.
            if let egui::Shape::Rect(rect) = shape
                && rect.rect.width() > 24.0
                && rect.rect.height() < 60.0
            {
                if rect.stroke.color == colours.field_border && rect.stroke.width > 0.0 {
                    outlined.push(rect.rect);
                }
                if rect.fill == colours.primary {
                    primary.push(rect.rect);
                }
            }
        }
        (outlined, primary)
    }

    /// The default window (both Decks open at 1120 x 760) less the bar.
    const DEFAULT_HOME: egui::Vec2 = egui::vec2(1120.0, 760.0 - 36.0);

    /// A context themed as the Deck, in the light or the dark palette.
    fn themed(dark: bool) -> egui::Context {
        let ctx = egui::Context::default();
        super::super::theme::apply(&ctx);
        ctx.set_theme(if dark {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        });
        ctx
    }

    type Open = fn(&mut HomeFixture);

    /// Home's scenes, as the owner clicks through them.
    fn scenes() -> [(&'static str, Open); 5] {
        [
            ("connect", |home| {
                let _ = home.apply(HomeAction::SelectConnection(0));
            }),
            ("new", |home| {
                let _ = home.apply(HomeAction::BeginAdd);
            }),
            ("edit", |home| {
                let _ = home.apply(HomeAction::Edit(0));
            }),
            ("edit, own settings", |home| {
                home.saved[0].settings.overrides.typing_mode =
                    crate::overrides::Override::Own(arcen_input::text::TypingMode::Keys);
                let _ = home.apply(HomeAction::Edit(0));
            }),
            ("empty", |home| {
                home.saved.clear();
                home.direct = ConnectionDraft::new(ConnectionKind::DirectMachine);
            }),
        ]
    }

    #[test]
    fn every_scene_has_one_44_point_primary_36_point_controls_and_fits() {
        use super::super::theme::{CONTROL_HEIGHT, PRIMARY_HEIGHT, Palette};
        for dark in [false, true] {
            let colours = Palette::of(dark);
            for (scene, open) in scenes() {
                let ctx = themed(dark);
                let mut home = HomeFixture::new();
                open(&mut home);
                // Twice: the first pass settles the layout.
                let _ = home_shapes(&ctx, &mut home, DEFAULT_HOME, Vec::new());
                let shapes = home_shapes(&ctx, &mut home, DEFAULT_HOME, Vec::new());
                let (outlined, primary) = controls(&shapes, &colours);
                assert_eq!(primary.len(), 1, "{scene}: {primary:?}");
                assert!(
                    (primary[0].height() - PRIMARY_HEIGHT).abs() < 0.01,
                    "{scene}: {:?}",
                    primary[0]
                );
                assert!(!outlined.is_empty(), "{scene}");
                for rect in &outlined {
                    assert!(
                        (rect.height() - CONTROL_HEIGHT).abs() < 0.01,
                        "{scene}: {rect:?}"
                    );
                }
                // Everything shows without scrolling while no list is open.
                for rect in outlined.iter().chain(&primary) {
                    assert!(rect.bottom() <= DEFAULT_HOME.y, "{scene}: {rect:?}");
                }
                if scene == "connect" {
                    // One right edge: the fields and Connect end on it.
                    let pane: Vec<_> = outlined
                        .iter()
                        .chain(&primary)
                        .filter(|rect| rect.left() > SIDEBAR_W)
                        .collect();
                    let right = primary[0].right();
                    for rect in &pane {
                        assert!((rect.right() - right).abs() < 0.5, "{scene}: {rect:?}");
                    }
                    assert!(
                        (primary[0].width() - super::super::theme::COLUMN_MAX).abs() < 0.5,
                        "{scene}: {:?}",
                        primary[0]
                    );
                }
                if scene.starts_with("edit") {
                    // Three fields and four pickers (a Mac Deck), and
                    // Cancel.
                    assert!(outlined.len() > 3 + 4, "{scene}: {}", outlined.len());
                }
            }
        }
    }

    /// The form's controls in a pane `size` wide: the fields, then the
    /// pickers (by their left edge), and the primary.
    fn form_columns(size: egui::Vec2) -> (Vec<egui::Rect>, Vec<egui::Rect>, egui::Rect) {
        let colours = super::super::theme::Palette::light();
        let ctx = themed(false);
        let mut home = HomeFixture::new();
        let _ = home.apply(HomeAction::Edit(0));
        let _ = home_shapes(&ctx, &mut home, size, Vec::new());
        let shapes = home_shapes(&ctx, &mut home, size, Vec::new());
        let (outlined, primary) = controls(&shapes, &colours);
        // The right (or, stacked, the second) section starts at its title.
        let section = shapes
            .iter()
            .find_map(|shape| match shape {
                egui::Shape::Text(text) if text.galley.job.text == ON_THIS_MACHINE => {
                    Some(text.pos)
                }
                _ => None,
            })
            .expect("the section drawn");
        let (mut pickers, mut fields): (Vec<_>, Vec<_>) = outlined
            .iter()
            .copied()
            .filter(|rect| rect.left() > SIDEBAR_W && rect.width() > 300.0)
            .partition(|rect| (rect.left() - section.x).abs() <= 1.0 && rect.top() > section.y);
        fields.sort_by(|a, b| a.top().total_cmp(&b.top()));
        pickers.sort_by(|a, b| a.top().total_cmp(&b.top()));
        (fields, pickers, primary[0])
    }

    #[test]
    fn the_form_sits_in_two_columns_on_one_top_line_and_stacks_when_narrow() {
        let (fields, pickers, save) = form_columns(DEFAULT_HOME);
        assert_eq!(pickers.len(), 4, "{pickers:?}");
        let address = fields[0];
        let first_picker = pickers[0];
        assert!(first_picker.left() > address.right(), "side by side");
        assert!(
            (first_picker.left() - address.right() - FORM_GUTTER).abs() < 0.5,
            "the gutter: {address:?} {first_picker:?}"
        );
        // Both sections' titles sit on one line, so the first label and
        // the first field of each column line up.
        let (shown, _) = {
            let ctx = themed(false);
            let mut home = HomeFixture::new();
            let _ = home.apply(HomeAction::Edit(0));
            let _ = home_frame(&ctx, &mut home, Vec::new());
            home_frame(&ctx, &mut home, Vec::new())
        };
        let y = |wanted: &str| {
            shown
                .iter()
                .find(|(text, _)| text == wanted)
                .map_or_else(|| panic!("no {wanted}: {shown:?}"), |(_, at)| at.y)
        };
        assert!((y("Connection") - y(ON_THIS_MACHINE)).abs() < 0.5);
        assert!((address.top() - first_picker.top()).abs() < 0.5);
        // The action row spans both columns: Save ends on the right one's
        // edge.
        assert!(
            (save.right() - first_picker.right()).abs() < 0.5,
            "{save:?}"
        );
        assert!(save.top() > pickers[3].bottom());

        // Narrower than the breakpoint: one column, Connection first.
        let narrow = egui::vec2(SIDEBAR_W + FORM_STACK_BELOW - 40.0, 1400.0);
        let (fields, pickers, _) = form_columns(narrow);
        assert!(
            (fields[0].left() - pickers[0].left()).abs() < 0.5,
            "stacked"
        );
        assert!(pickers[0].top() > fields.last().expect("fields").bottom());
    }

    #[test]
    fn no_disclosure_remains_on_the_form() {
        let ctx = themed(false);
        let mut home = HomeFixture::new();
        let _ = home.apply(HomeAction::Edit(0));
        let _ = home_frame(&ctx, &mut home, Vec::new());
        let (shown, _) = home_frame(&ctx, &mut home, Vec::new());
        for gone in ["Keyboard and displays", "All follow Settings"] {
            assert!(shown.iter().all(|(text, _)| text != gone), "{gone}");
        }
        for wanted in ["Connection", ON_THIS_MACHINE, "Typing", "Displays"] {
            assert!(shown.iter().any(|(text, _)| text == wanted), "{wanted}");
        }
    }

    /// A presence source with one answer for every machine.
    struct Fixed(HostPresence);

    impl PresenceSource for Fixed {
        fn presence(&self, _id: &str, _name: &str) -> HostPresence {
            self.0.clone()
        }
    }

    fn texts_with(home: &mut HomeFixture, presence: &dyn PresenceSource) -> Vec<String> {
        let ctx = egui::Context::default();
        let mut texts = Vec::new();
        for _ in 0..2 {
            let mut state = home.state();
            let output = ctx.run_ui(egui::RawInput::default(), |ui| {
                let _ = render_home_with(ui, &mut state, presence);
            });
            texts.clear();
            for clipped in output.shapes {
                if let egui::Shape::Text(text) = clipped.shape {
                    texts.push(text.galley.job.text.clone());
                }
            }
        }
        texts
    }

    #[test]
    fn an_unknown_presence_shows_the_address_and_no_status() {
        let mut home = HomeFixture::new();
        let _ = home.apply(HomeAction::SelectConnection(0));
        let unknown = texts_with(&mut home, &crate::presence::NoPresence);
        assert!(
            unknown.iter().any(|text| text == "203.0.113.10"),
            "{unknown:?}"
        );
        for status in ["Ready", "Off", "Resume", "In use by someone else"] {
            assert!(unknown.iter().all(|text| text != status), "{status}");
        }
        assert!(unknown.iter().any(|text| text == "Password"));
        // A known status replaces the address, and the user's own session
        // resumes without a password.
        let mine = Fixed(HostPresence::SignedIn {
            by_you: true,
            who: None,
            since: None,
        });
        home.direct.username = "artist".to_owned();
        let known = texts_with(&mut home, &mine);
        assert!(known.iter().all(|text| text != "203.0.113.10"), "{known:?}");
        assert!(
            known
                .iter()
                .any(|text| text == "You\u{2019}re signed in \u{b7} resume")
        );
        assert!(known.iter().any(|text| text == "Resume"));
        assert!(known.iter().all(|text| text != "Password"));
    }

    #[test]
    fn a_rows_menu_shows_on_the_selected_or_hovered_row_only() {
        assert!(kebab_visible(true, false, false));
        assert!(kebab_visible(false, true, false));
        assert!(kebab_visible(false, false, true));
        assert!(!kebab_visible(false, false, false));
        // As drawn: a menu is three small rings.
        let menus = |shapes: &[egui::Shape]| {
            shapes
                .iter()
                .filter(|shape| matches!(shape, egui::Shape::Circle(circle) if circle.radius < 1.0))
                .count()
                / 3
        };
        let mut home = HomeFixture::new();
        let mut second = home.saved[0].clone();
        second.id = "second".to_owned();
        second.name = "Second".to_owned();
        home.saved.push(second.clone());
        second.id = "third".to_owned();
        second.name = "Third".to_owned();
        home.saved.push(second);
        let _ = home.apply(HomeAction::SelectConnection(0));
        let ctx = egui::Context::default();
        let shapes = home_shapes(&ctx, &mut home, DEFAULT_HOME, Vec::new());
        assert_eq!(menus(&shapes), 1, "the selected row only");
        let (shown, _) = home_frame(&ctx, &mut home, Vec::new());
        let at = shown
            .iter()
            .find(|(text, _)| text == "Second")
            .map_or_else(|| panic!("no Second: {shown:?}"), |(_, at)| *at);
        let shapes = home_shapes(
            &ctx,
            &mut home,
            DEFAULT_HOME,
            vec![egui::Event::PointerMoved(at)],
        );
        assert_eq!(menus(&shapes), 2, "and the row under the pointer");
    }

    #[test]
    fn every_scene_starts_on_the_sidebar_header_s_line_under_the_bar() {
        type Open = fn(&mut HomeFixture);
        let scenes: [(&str, &str, Open); 4] = [
            ("Studio", "connect", |home| {
                let _ = home.apply(HomeAction::SelectConnection(0));
            }),
            ("New connection", "new", |home| {
                let _ = home.apply(HomeAction::BeginAdd);
            }),
            ("Edit connection", "edit, open", |home| {
                home.saved[0].settings.overrides.typing_mode =
                    crate::overrides::Override::Own(arcen_input::text::TypingMode::Keys);
                let _ = home.apply(HomeAction::Edit(0));
            }),
            ("No connections yet", "empty", |home| {
                home.saved.clear();
                home.direct = ConnectionDraft::new(ConnectionKind::DirectMachine);
            }),
        ];
        let inset = super::super::theme::SCENE_TOP_INSET;
        for (title, scene, open) in scenes {
            let ctx = egui::Context::default();
            let mut home = HomeFixture::new();
            open(&mut home);
            let _ = home_frame(&ctx, &mut home, Vec::new());
            let (shown, _) = home_frame(&ctx, &mut home, Vec::new());
            // The sidebar's header, or the pane's title (a row has the name).
            let top = |wanted: &str| {
                shown
                    .iter()
                    .find(|(text, at)| {
                        text == wanted && ((wanted == "Connections") == (at.x < SIDEBAR_W))
                    })
                    .map_or_else(|| panic!("{scene}: no {wanted}: {shown:?}"), |(_, at)| at.y)
            };
            assert!(
                (top("Connections") - inset).abs() < 0.5,
                "{scene}: {}",
                top("Connections")
            );
            assert!((top(title) - inset).abs() < 0.5, "{scene}: {}", top(title));
        }
    }

    #[test]
    fn a_rows_menu_offers_test_connection_between_edit_and_export() {
        let mut home = HomeFixture::new();
        let _ = home.apply(HomeAction::SelectConnection(0));
        let ctx = egui::Context::default();
        let shapes = home_shapes(&ctx, &mut home, DEFAULT_HOME, Vec::new());
        // The selected row's ⋮: its middle ring.
        let rings: Vec<egui::Pos2> = shapes
            .iter()
            .filter_map(|shape| match shape {
                egui::Shape::Circle(circle) if circle.radius < 1.0 => Some(circle.center),
                _ => None,
            })
            .collect();
        assert_eq!(rings.len(), 3, "{rings:?}");
        let at = rings[1];
        let click = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let _ = home_frame(&ctx, &mut home, vec![egui::Event::PointerMoved(at)]);
        let _ = home_frame(&ctx, &mut home, vec![click(true)]);
        let _ = home_frame(&ctx, &mut home, vec![click(false)]);
        let (shown, _) = home_frame(&ctx, &mut home, Vec::new());
        // The menu hangs by the list; the column's Edit is further right.
        let mut items: Vec<(String, f32)> = shown
            .iter()
            .filter(|(text, at)| {
                at.x < SIDEBAR_W + 120.0
                    && ["Edit", "Test connection", "Export\u{2026}", "Delete"]
                        .contains(&text.as_str())
            })
            .map(|(text, at)| (text.clone(), at.y))
            .collect();
        items.sort_by(|a, b| a.1.total_cmp(&b.1));
        let menu: Vec<&str> = items.iter().map(|(text, _)| text.as_str()).collect();
        assert_eq!(
            menu,
            ["Edit", "Test connection", "Export\u{2026}", "Delete"],
            "{shown:?}"
        );
        // It selects that connection and runs the test on its card.
        let outcome = home.apply(HomeAction::TestSaved(0));
        assert!(matches!(
            outcome.effect,
            HomeEffect::TestConnection { ref host, ref port }
                if host == "203.0.113.10" && port == "18444"
        ));
        assert_eq!(home.direct.saved_connection_id.as_deref(), Some("studio"));
    }

    /// Draws `add` in a themed context, idle, under the pointer, pressed
    /// and after a click (a list then open, its first item under the
    /// pointer): every text of every state with its colour and ground.
    fn every_state(
        dark: bool,
        mut add: impl FnMut(&mut egui::Ui) -> egui::Response,
    ) -> Vec<(&'static str, String, egui::Color32, egui::Color32)> {
        fn flat(output: egui::FullOutput) -> Vec<egui::Shape> {
            fn flatten(shape: egui::Shape, out: &mut Vec<egui::Shape>) {
                match shape {
                    egui::Shape::Vec(shapes) => {
                        for shape in shapes {
                            flatten(shape, out);
                        }
                    }
                    shape => out.push(shape),
                }
            }
            let mut shapes = Vec::new();
            for clipped in output.shapes {
                flatten(clipped.shape, &mut shapes);
            }
            shapes
        }
        let ctx = themed(dark);
        let base = super::super::theme::Palette::of(dark).bg;
        let drawn = std::cell::Cell::new(egui::Rect::NOTHING);
        let mut pass = |events: Vec<egui::Event>| {
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800.0, 600.0),
                    )),
                    events,
                    ..egui::RawInput::default()
                },
                |ui| {
                    ui.add_space(20.0);
                    ui.horizontal(|ui| {
                        ui.add_space(20.0);
                        drawn.set(add(ui).rect);
                    });
                },
            );
            flat(output)
        };
        let mut seen = Vec::new();
        let mut record = |state: &'static str, shapes: &[egui::Shape]| {
            for (text, colour, ground) in
                super::super::theme::checks::texts_on_their_ground(shapes, base)
            {
                seen.push((state, text, colour, ground));
            }
        };
        let idle = pass(Vec::new());
        record("inactive", &idle);
        let rect = drawn.get();
        let at = rect.center();
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        record("hovered", &pass(vec![egui::Event::PointerMoved(at)]));
        record("active", &pass(vec![button(true)]));
        let _ = pass(vec![button(false)]);
        let away = egui::Event::PointerMoved(at + egui::vec2(0.0, 300.0));
        // A list in place slides open over a few passes.
        for _ in 0..8 {
            let _ = pass(vec![away.clone()]);
        }
        let open = pass(vec![away]);
        record("open", &open);
        // A list that opened: its first item under the pointer.
        if let Some(item) = open.iter().rev().find_map(|shape| match shape {
            egui::Shape::Text(text) if text.pos.y > rect.bottom() + 1.0 => Some(text.pos),
            _ => None,
        }) {
            record(
                "open, item hovered",
                &pass(vec![egui::Event::PointerMoved(item + egui::vec2(4.0, 4.0))]),
            );
        }
        seen
    }

    #[test]
    fn every_control_s_text_keeps_4_5_to_1_on_its_fill_in_every_state() {
        use super::super::theme::contrast_ratio;
        use super::super::widgets;
        let check =
            |what: &str, seen: Vec<(&'static str, String, egui::Color32, egui::Color32)>| {
                assert!(!seen.is_empty(), "{what}: drew no text");
                for state in ["inactive", "hovered", "active"] {
                    assert!(seen.iter().any(|(s, ..)| *s == state), "{what}: no {state}");
                }
                for (state, text, colour, ground) in seen {
                    let ratio = contrast_ratio(colour, ground);
                    assert!(
                        ratio >= 4.5,
                        "{what}, {state}: {text:?} is {ratio:.2}:1 ({colour:?} on {ground:?})"
                    );
                }
            };
        for dark in [false, true] {
            let palette = if dark { "dark" } else { "light" };
            check(
                &format!("primary, {palette}"),
                every_state(dark, |ui| widgets::primary_button(ui, "Connect", 200.0)),
            );
            check(
                &format!("secondary, {palette}"),
                every_state(dark, |ui| widgets::secondary_button(ui, "Cancel", 120.0)),
            );
            check(
                &format!("text button, {palette}"),
                every_state(dark, |ui| {
                    widgets::text_button(ui, "Test connection", false)
                }),
            );
            let mut on = true;
            check(
                &format!("option, {palette}"),
                every_state(dark, |ui| quiet_option(ui, &mut on, "Remember username")),
            );
            let mut overrides = crate::overrides::ConnectionOverrides::default();
            let seen = every_state(dark, |ui| {
                let field = super::super::input_choices::typing_field(
                    crate::overrides::AppDefaults::default(),
                    &overrides,
                );
                super::super::list_picker::list_picker(
                    ui,
                    "typing",
                    Some(field.title),
                    &mut overrides.typing_mode,
                    &field.choices,
                    Some(field.line.as_str()),
                    340.0,
                )
                .response
            });
            assert!(
                seen.iter()
                    .any(|(state, ..)| *state == "open, item hovered"),
                "{palette}: the list did not open"
            );
            check(&format!("picker and its list, {palette}"), seen);
            // The ＋ draws no text of its own: its menu's items are checked.
            let seen = every_state(dark, |ui| add_menu_button(ui).0);
            assert!(
                seen.iter()
                    .any(|(state, ..)| *state == "open, item hovered"),
                "{palette}: the menu did not open"
            );
            for (state, text, colour, ground) in seen {
                let ratio = contrast_ratio(colour, ground);
                assert!(
                    ratio >= 4.5,
                    "+ menu, {palette}, {state}: {text:?} is {ratio:.2}:1"
                );
            }
        }
    }

    #[test]
    fn no_line_or_fill_is_lighter_than_the_border_in_the_dark_palette() {
        for dark in [true, false] {
            let colours = super::super::theme::Palette::of(dark);
            for (scene, open) in scenes() {
                let ctx = themed(dark);
                let mut home = HomeFixture::new();
                open(&mut home);
                let _ = home_shapes(&ctx, &mut home, DEFAULT_HOME, Vec::new());
                let shapes = home_shapes(&ctx, &mut home, DEFAULT_HOME, Vec::new());
                for colour in super::super::theme::checks::painted_colours(&shapes, colours.bg) {
                    assert!(
                        super::super::theme::checks::quiet_enough(&colours, colour),
                        "{scene}, dark {dark}: {colour:?}"
                    );
                }
            }
        }
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

    /// The Edit form's Address field and Typing picker, as drawn on `ctx`.
    fn form_targets(ctx: &egui::Context, home: &mut HomeFixture) -> (egui::Rect, egui::Rect) {
        let colours = super::super::theme::Palette::light();
        let _ = home_shapes(ctx, home, DEFAULT_HOME, Vec::new());
        let shapes = home_shapes(ctx, home, DEFAULT_HOME, Vec::new());
        let (outlined, _) = controls(&shapes, &colours);
        let label = |wanted: &str| {
            shapes
                .iter()
                .find_map(|shape| match shape {
                    egui::Shape::Text(text) if text.galley.job.text == wanted => Some(text.pos),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no {wanted}"))
        };
        let under = |at: egui::Pos2| {
            outlined
                .iter()
                .copied()
                .filter(|rect| (rect.left() - at.x).abs() <= 1.0 && rect.top() > at.y)
                .min_by(|a, b| a.top().total_cmp(&b.top()))
                .expect("a control under its label")
        };
        (under(label("Address")), under(label("Typing")))
    }

    #[test]
    fn an_export_or_import_outcome_shows_under_the_list_until_dismissed() {
        let ctx = egui::Context::default();
        let mut home = HomeFixture::new();
        home.notice = Some(HomeNotice {
            text: "Not imported: This file is too large to be an Arcen connection (the limit \
                   is 64 KB)."
                .to_owned(),
            problem: true,
        });
        let (shown, _) = home_frame(&ctx, &mut home, Vec::new());
        let at = |shown: &[(String, egui::Pos2)], wanted: &str| {
            shown
                .iter()
                .find(|(text, _)| text.contains(wanted))
                .map(|(_, at)| *at)
        };
        let notice = at(&shown, "Not imported").expect("the notice is on Home");
        let row = at(&shown, "Studio").expect("the connection list");
        // In the sidebar, under the list.
        assert!(
            notice.y > row.y && notice.x < SIDEBAR_W,
            "{notice:?} {row:?}"
        );
        assert!(!shown.iter().any(|(text, _)| text.contains('\u{00b7}')));
        // Not a sign-in outcome: none is set, and none appears.
        assert!(home.sign_in.feedback().is_none());
        let dismiss = at(&shown, "Dismiss").expect("Dismiss");
        let _ = home_frame(&ctx, &mut home, click_at(dismiss));
        assert_eq!(home.notice, None);
        let (shown, _) = home_frame(&ctx, &mut home, Vec::new());
        assert!(at(&shown, "Not imported").is_none());
    }

    fn click_at(at: egui::Pos2) -> Vec<egui::Event> {
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
    }

    /// One pass of Home on `ctx` at the default size; its action.
    fn form_pass(
        ctx: &egui::Context,
        home: &mut HomeFixture,
        events: Vec<egui::Event>,
    ) -> Option<HomeAction> {
        let mut action = None;
        let mut state = home.state();
        let _ = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, DEFAULT_HOME)),
                events,
                ..egui::RawInput::default()
            },
            |ui| action = render_home(ui, &mut state),
        );
        action
    }

    #[test]
    fn after_a_picker_esc_cancels_the_form_and_return_in_the_address_saves() {
        // Esc closes the open list; the next Esc cancels the form.
        let ctx = themed(false);
        let mut home = HomeFixture::new();
        let _ = home.apply(HomeAction::Edit(0));
        let (address, typing) = form_targets(&ctx, &mut home);
        let _ = form_pass(&ctx, &mut home, click_at(typing.center()));
        let _ = form_pass(&ctx, &mut home, Vec::new());
        assert!(super::super::list_picker::any_open(&ctx));
        assert!(form_pass(&ctx, &mut home, key(egui::Key::Escape)).is_none());
        assert!(!super::super::list_picker::any_open(&ctx));
        assert!(matches!(
            form_pass(&ctx, &mut home, key(egui::Key::Escape)),
            Some(HomeAction::CancelForm)
        ));

        // Shift+Tab out of an open list closes it; Return in the Address
        // then saves.
        let ctx = themed(false);
        let mut home = HomeFixture::new();
        let _ = home.apply(HomeAction::Edit(0));
        let _ = form_targets(&ctx, &mut home);
        let _ = form_pass(&ctx, &mut home, click_at(typing.center()));
        let _ = form_pass(&ctx, &mut home, Vec::new());
        let shift_tab = vec![egui::Event::Key {
            key: egui::Key::Tab,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::SHIFT,
        }];
        let _ = form_pass(&ctx, &mut home, shift_tab);
        let _ = form_pass(&ctx, &mut home, Vec::new());
        assert!(!super::super::list_picker::any_open(&ctx));
        let _ = form_pass(&ctx, &mut home, click_at(address.center()));
        assert!(matches!(
            form_pass(&ctx, &mut home, key(egui::Key::Enter)),
            Some(HomeAction::SaveEdit { .. })
        ));
    }

    #[test]
    fn a_picker_left_open_on_another_page_does_not_hold_the_forms_keys() {
        let ctx = themed(false);
        // A Settings page with its list open, then Done: Home's form.
        let mut level = crate::settings::LogLevel::ALL[0];
        let levels: Vec<_> = crate::settings::LogLevel::ALL
            .iter()
            .map(|level| (*level, level.label().to_string()))
            .collect();
        let mut value = egui::Rect::NOTHING;
        for _ in 0..2 {
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                value = super::super::list_picker::list_picker(
                    ui,
                    "log_level",
                    None,
                    &mut level,
                    &levels,
                    None,
                    200.0,
                )
                .response
                .rect;
            });
        }
        let _ = ctx.run_ui(
            egui::RawInput {
                events: click_at(value.center()),
                ..egui::RawInput::default()
            },
            |ui| {
                let _ = super::super::list_picker::list_picker(
                    ui,
                    "log_level",
                    None,
                    &mut level,
                    &levels,
                    None,
                    200.0,
                );
            },
        );
        assert!(super::super::list_picker::any_open(&ctx));
        let mut home = HomeFixture::new();
        let _ = home.apply(HomeAction::Edit(0));
        let (address, _) = form_targets(&ctx, &mut home);
        let _ = form_pass(&ctx, &mut home, click_at(address.center()));
        assert!(matches!(
            form_pass(&ctx, &mut home, key(egui::Key::Enter)),
            Some(HomeAction::SaveEdit { .. })
        ));
    }

    #[test]
    fn return_on_a_focused_test_connection_tests_and_saves_nothing() {
        let ctx = themed(false);
        let colours = super::super::theme::Palette::light();
        let mut home = HomeFixture::new();
        let _ = home.apply(HomeAction::Edit(0));
        let (address, _) = form_targets(&ctx, &mut home);
        let _ = form_pass(&ctx, &mut home, click_at(address.center()));
        // Tab until the focus ring is round Test connection.
        let mut found = false;
        for _ in 0..20 {
            let tab = vec![egui::Event::Key {
                key: egui::Key::Tab,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }];
            let _ = form_pass(&ctx, &mut home, tab);
            let shapes = home_shapes(&ctx, &mut home, DEFAULT_HOME, Vec::new());
            let label = shapes.iter().find_map(|shape| match shape {
                egui::Shape::Text(text) if text.galley.job.text == "Test connection" => {
                    Some(text.pos + egui::vec2(2.0, 2.0))
                }
                _ => None,
            });
            found = label.is_some_and(|at| {
                shapes.iter().any(|shape| {
                    matches!(shape, egui::Shape::Rect(rect)
                        if rect.stroke.color == colours.accent
                            && (rect.stroke.width - super::super::theme::FOCUS_RING_WIDTH).abs() < 0.01
                            && rect.rect.contains(at))
                })
            });
            if found {
                break;
            }
        }
        assert!(found, "Tab reaches Test connection");
        assert!(matches!(
            form_pass(&ctx, &mut home, key(egui::Key::Enter)),
            Some(HomeAction::TestConnection { .. })
        ));
    }

    #[test]
    fn only_a_connection_with_its_own_displays_says_so_with_the_way_back() {
        let ctx = egui::Context::default();
        let mut home = HomeFixture::new();
        let _ = home.apply(HomeAction::SelectConnection(0));
        let _ = home_frame(&ctx, &mut home, Vec::new());
        let (shown, _) = home_frame(&ctx, &mut home, Vec::new());
        assert!(shown.iter().all(|(text, _)| text != USE_APP_DEFAULT));
        assert!(shown.iter().all(|(text, _)| !text.starts_with("Displays:")));

        home.saved[0].settings.overrides.displays_mode =
            crate::overrides::Override::Own(crate::settings::DisplaysMode::Windowed);
        let _ = home_frame(&ctx, &mut home, Vec::new());
        let (shown, _) = home_frame(&ctx, &mut home, Vec::new());
        assert!(
            shown
                .iter()
                .any(|(text, _)| text == "Displays: Windowed (this connection)"),
            "{shown:?}"
        );
        let at = shown
            .iter()
            .find(|(text, _)| text == USE_APP_DEFAULT)
            .map(|(_, at)| *at)
            .expect("the link");
        let _ = home_frame(&ctx, &mut home, vec![egui::Event::PointerMoved(at)]);
        let mut chosen = None;
        for events in click_at(at).into_iter().skip(1) {
            let (_, action) = home_frame(&ctx, &mut home, vec![events]);
            chosen = chosen.or(action);
        }
        assert!(
            matches!(chosen, Some(HomeAction::FollowAppDisplays(0))),
            "{chosen:?}"
        );
    }
}
