// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionKind {
    DirectMachine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SavedTransport {
    #[cfg(feature = "wss-compat")]
    Wss,
    Quic,
}

impl SavedTransport {
    pub const fn as_key(self) -> &'static str {
        match self {
            #[cfg(feature = "wss-compat")]
            Self::Wss => "wss",
            Self::Quic => "quic",
        }
    }

    pub const fn default_port(self) -> u16 {
        match self {
            #[cfg(feature = "wss-compat")]
            Self::Wss => 18_443,
            Self::Quic => 18_444,
        }
    }

    pub const fn accepts_port(self, port: u16) -> bool {
        match self {
            #[cfg(feature = "wss-compat")]
            Self::Wss => port == Self::Wss.default_port(),
            Self::Quic => port != 0,
        }
    }

    pub const fn port_validation_message(self) -> &'static str {
        match self {
            #[cfg(feature = "wss-compat")]
            Self::Wss => "WSS (TCP) requires port 18443.",
            Self::Quic => "QUIC (UDP) requires a port from 1 to 65535; 18444 is the default.",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            #[cfg(feature = "wss-compat")]
            Self::Wss => "WSS (TCP)",
            Self::Quic => "QUIC (UDP)",
        }
    }
}

impl ConnectionKind {
    pub const fn icon(self) -> &'static str {
        match self {
            Self::DirectMachine => "◉",
        }
    }

    pub const fn title(self) -> &'static str {
        match self {
            Self::DirectMachine => "Direct Machine",
        }
    }

    pub const fn short_description(self) -> &'static str {
        match self {
            Self::DirectMachine => "Connect to a specific workstation by address.",
        }
    }

    pub const fn primary_action(self) -> &'static str {
        match self {
            Self::DirectMachine => "Connect",
        }
    }

    pub const fn default_port(self) -> u16 {
        match self {
            // Direct-to-Pier product sessions use QUIC on UDP 18444.
            Self::DirectMachine => 18444,
        }
    }

    pub const fn as_key(self) -> &'static str {
        match self {
            Self::DirectMachine => "direct",
        }
    }

    /// Every saved connection is a direct machine; an unknown or retired
    /// kind key loads as one.
    pub fn from_key(_key: &str) -> Self {
        Self::DirectMachine
    }
}

#[derive(Clone)]
pub struct ConnectionDraft {
    pub saved_connection_id: Option<String>,
    pub kind: ConnectionKind,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub use_tls: bool,
    /// Request QUIC transport for this connection.
    /// QUIC remains TLS-authenticated and uses the saved UDP port.
    pub use_quic: bool,
    pub tls_trust: Option<crate::transport::tls::TlsTrustConfig>,
    pub auto_reconnect: bool,
}

impl std::fmt::Debug for ConnectionDraft {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConnectionDraft")
            .field("kind", &self.kind)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("use_tls", &self.use_tls)
            .field("use_quic", &self.use_quic)
            .field("tls_trust", &self.tls_trust)
            .field("auto_reconnect", &self.auto_reconnect)
            .finish()
    }
}

impl Drop for ConnectionDraft {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.password);
    }
}

impl ConnectionDraft {
    pub fn new(kind: ConnectionKind) -> Self {
        Self {
            saved_connection_id: None,
            kind,
            host: String::new(),
            port: kind.default_port(),
            username: String::new(),
            password: String::new(),
            use_tls: true,
            use_quic: true,
            tls_trust: None,
            auto_reconnect: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SavedConnectionSettings {
    pub version: u32,
    pub remembered_username: Option<String>,
    pub tablet_mode_requested: crate::protocol::messages::TabletModeMsg,
    /// This connection's own Displays choice, or `None` to follow the
    /// application-wide default.
    ///
    /// Deliberately an `Option` rather than a concrete mode: the display
    /// layout a host can serve is a property of that host, not of the user's
    /// general preference. A host that advertises `max_monitors: 1` rejects
    /// Match My Layout outright, and before this the only remedy was to
    /// change the global setting and change it back after switching hosts.
    /// `None` (the default, and the value every previously saved connection
    /// parses as) preserves exactly the old application-wide behaviour.
    pub displays_mode: Option<crate::ui::app::DisplaysMode>,
    /// What Command, Control and Option are on this connection's host.
    /// The default (and every connection saved before it existed) follows
    /// the host's platform, learnt from its hello.
    pub keyboard: arcen_input::keyboard::KeyboardSettings,
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

impl Default for SavedConnectionSettings {
    fn default() -> Self {
        Self {
            version: 1,
            remembered_username: None,
            tablet_mode_requested: crate::protocol::messages::TabletModeMsg::LocalTermination,
            displays_mode: None,
            keyboard: arcen_input::keyboard::KeyboardSettings::default(),
            unknown: serde_json::Map::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SavedConnectionSummary {
    pub id: String,
    pub kind: ConnectionKind,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub use_tls: bool,
    pub transport: SavedTransport,
    pub identity_security_mode: Option<String>,
    pub settings: SavedConnectionSettings,
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

/// All the transient state the Home screen reads and writes. The caller
/// (app.rs `ArcenApp`) owns all the underlying fields; this struct is a
/// short-lived borrow assembled each frame.
pub struct HomeViewState<'a> {
    /// Working draft for the selected connection (holds username/password).
    pub direct: &'a mut ConnectionDraft,
    pub saved_connections: &'a [SavedConnectionSummary],
    pub search: &'a mut String,
    pub operator_notice: Option<&'a str>,
    /// `true` → show the "Add connection" form in the detail panel.
    pub show_add_form: bool,
    /// `Some(i)` → show the "Edit connection" form for saved index `i`.
    pub editing_index: Option<usize>,
    // Add / edit form field buffers
    pub form_name: &'a mut String,
    pub form_host: &'a mut String,
    pub form_port: &'a mut String,
    pub form_username: &'a mut String,
    /// Working value for the form's Displays selector. `None` renders as
    /// "Use app default".
    pub form_displays_mode: &'a mut Option<crate::ui::app::DisplaysMode>,
    /// Working value for the form's Keyboard settings.
    pub form_keyboard: &'a mut arcen_input::keyboard::KeyboardSettings,
    /// Connection error to show under the Connect button.
    pub connection_error: Option<&'a str>,
    pub remember_username: &'a mut bool,
    /// The connection test shown in the editor or detail panel, if any.
    pub connection_test: Option<&'a crate::ui::connection_test::ConnectionTest>,
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
        displays_mode: Option<crate::ui::app::DisplaysMode>,
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
        displays_mode: Option<crate::ui::app::DisplaysMode>,
        /// This connection's keyboard settings.
        keyboard: arcen_input::keyboard::KeyboardSettings,
    },
    /// Cancel the add/edit form.
    CancelForm,
    /// Run the connection test against this address. Sends no credentials.
    TestConnection { host: String, port: String },
    /// A button in the connection test's report.
    ConnectionTest(crate::ui::connection_test::ConnectionTestAction),
}

// ── Main entry point ─────────────────────────────────────────────────────────

pub fn render_home(ui: &mut egui::Ui, state: &mut HomeViewState<'_>) -> Option<HomeAction> {
    let mut action = None;

    render_operator_notice(ui, state.operator_notice);

    // Capture available size *after* any operator notice, then allocate a
    // fixed rect so the sidebar + detail panel fill the entire remaining area.
    let available = ui.available_size();

    ui.allocate_ui_with_layout(
        available,
        egui::Layout::left_to_right(egui::Align::TOP),
        |ui| {
            let search_needle = state.search.trim().to_lowercase();
            let selected_index = state
                .direct
                .saved_connection_id
                .as_ref()
                .and_then(|sel_id| state.saved_connections.iter().position(|c| &c.id == sel_id));
            let fallback_index = if selected_index.is_none()
                && !state.show_add_form
                && state.editing_index.is_none()
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
            let detail_action = if state.show_add_form {
                connection_form(ui, state, None)
            } else if let Some(edit_idx) = state.editing_index {
                connection_form(ui, state, Some(edit_idx))
            } else if let Some(i) = selected_index.or(fallback_index) {
                detail_connection(ui, state, i)
            } else {
                detail_empty(ui);
                None
            };
            if let Some(a) = detail_action {
                action = Some(a);
            }
            if action.is_none() {
                if let Some(i) = fallback_index {
                    action = Some(HomeAction::SelectConnection(i));
                }
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

fn display_address(host: &str, port: u16) -> String {
    let host = host.trim();
    if host.parse::<std::net::IpAddr>().is_ok() {
        return host.to_string();
    }
    match resolve_host_ip(host, port) {
        Some(ip) => format!("{ip} ({host})"),
        None => host.to_string(),
    }
}

fn resolve_host_ip(host: &str, port: u16) -> Option<String> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, Option<String>>>,
    > = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    if let Ok(guard) = cache.lock() {
        if let Some(cached) = guard.get(host) {
            return cached.clone();
        }
    }

    let resolved = std::net::ToSocketAddrs::to_socket_addrs(&(host, port))
        .ok()
        .and_then(|iter| {
            let ips: Vec<std::net::IpAddr> = iter.map(|socket| socket.ip()).collect();
            ips.iter()
                .copied()
                .find(std::net::IpAddr::is_ipv4)
                .or_else(|| ips.first().copied())
        })
        .map(|ip| ip.to_string());

    if let Ok(mut guard) = cache.lock() {
        guard.insert(host.to_string(), resolved.clone());
    }
    resolved
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
                            let address = display_address(&saved.host, saved.port);
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
                            if let Some(err) = state.connection_error {
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
                                    draft.host = saved.host.clone();
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
fn connection_test_controls(
    ui: &mut egui::Ui,
    test: Option<&crate::ui::connection_test::ConnectionTest>,
    enabled: bool,
) -> Option<Option<crate::ui::connection_test::ConnectionTestAction>> {
    let mut action = None;
    let running = test.is_some_and(crate::ui::connection_test::ConnectionTest::running);
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
        if let Some(panel) = crate::ui::connection_test::render(ui, test) {
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
fn displays_field(ui: &mut egui::Ui, value: &mut Option<crate::ui::app::DisplaysMode>) {
    use crate::ui::app::DisplaysMode;

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
fn keyboard_field(ui: &mut egui::Ui, value: &mut arcen_input::keyboard::KeyboardSettings) {
    use arcen_input::keyboard::ShortcutPreference;

    const FIELD_W: f32 = 340.0;

    ui.label(
        egui::RichText::new("Keyboard shortcuts")
            .color(egui::Color32::DARK_GRAY)
            .size(12.0),
    );
    ui.add_space(3.0);
    egui::ComboBox::from_id_salt("connection_keyboard_shortcuts")
        .selected_text(shortcut_label(value.shortcuts))
        .width(FIELD_W)
        .show_ui(ui, |ui| {
            for preference in ShortcutPreference::ALL {
                ui.selectable_value(&mut value.shortcuts, preference, shortcut_label(preference));
            }
        });
    ui.add_space(3.0);
    ui.label(
        egui::RichText::new(shortcut_description(value.shortcuts))
            .color(egui::Color32::GRAY)
            .size(11.0),
    );
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
    ui.label(
        egui::RichText::new(match value.typing {
            None => "Follows the Typing setting in Settings.",
            Some(arcen_input::text::KeyboardMode::Physical) => {
                "Keys go as keys and the remote keyboard layout decides each character. Best \
                 for games and applications that read keys."
            }
            Some(arcen_input::text::KeyboardMode::Characters) => {
                "Typed characters arrive as this Mac types them, whatever the remote layout; \
                 shortcuts, arrows and function keys stay keys. Needs a host that types text."
            }
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

const fn shortcut_label(preference: arcen_input::keyboard::ShortcutPreference) -> &'static str {
    use arcen_input::keyboard::ShortcutPreference;
    match preference {
        ShortcutPreference::Automatic => "Automatic (by host)",
        ShortcutPreference::WindowsApp => "⌘ is the Windows key, Mac edit shortcuts",
        ShortcutPreference::SwapCommandControl => "⌘ and Control swapped",
        ShortcutPreference::CommandIsControl => "⌘ is Control",
    }
}

const fn shortcut_description(
    preference: arcen_input::keyboard::ShortcutPreference,
) -> &'static str {
    use arcen_input::keyboard::ShortcutPreference;
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
    fn connection_draft_debug_redacts_password() {
        let mut draft = ConnectionDraft::new(ConnectionKind::DirectMachine);
        draft.password = "dummy-password".to_string();
        let debug = format!("{draft:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("dummy-password"));
    }

    #[test]
    fn a_direct_machine_uses_the_product_udp_port() {
        assert_eq!(ConnectionKind::DirectMachine.default_port(), 18444);
        assert_eq!(ConnectionKind::DirectMachine.title(), "Direct Machine");
    }

    #[test]
    #[cfg(feature = "wss-compat")]
    fn quic_accepts_custom_nonzero_ports_while_wss_keeps_its_product_port() {
        assert!(SavedTransport::Quic.accepts_port(18_444));
        assert!(SavedTransport::Quic.accepts_port(19_444));
        assert!(!SavedTransport::Quic.accepts_port(0));
        assert!(SavedTransport::Wss.accepts_port(18_443));
        assert!(!SavedTransport::Wss.accepts_port(19_443));
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
