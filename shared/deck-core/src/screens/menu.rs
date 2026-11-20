// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Deck's command model: every menu command, its title and chord, and
//! the menus that list them. The macOS Deck renders it as its `NSMenu` bar;
//! the Windows Deck as a window menu bar and, in fullscreen, a connection
//! bar (PARITY §23 Q-1). Both Decks dispatch the same [`MenuCommand`]s.

use arcen_input::keyboard::{DeckShortcut, SendKey};

use super::wording::DeckPlatform;

/// A menu command, applied by the shell on the UI thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuCommand {
    /// Quit, from the app menu or the quit chord. `from_keyboard` is
    /// decided when the menu fires: the chord in a session is hold-to-quit,
    /// a click quits.
    Quit {
        from_keyboard: bool,
    },
    /// About Arcen Deck (a PC Deck's Help menu; the Mac uses the system's
    /// standard About panel).
    About,
    ToggleHealth,
    /// Show/hide the compact stream statistics (RTT, loss, fps, codec,
    /// SDR/HDR, lossless coverage).
    ToggleStreamStats,
    /// Show/hide the lossless-tile debug view.
    ToggleLosslessTiles,
    Disconnect,
    /// Release Stuck Keys: every key and button held on the host, and the
    /// pointer lock (what F9 did before it went to the host).
    ReleaseModifiers,
    /// Send Ctrl+Alt+Del to the host (a PC keyboard's own never reaches an
    /// application).
    SendSecureAttention,
    /// Give the keyboard to this computer or back to the host
    /// ([`DeckShortcut::ReleaseKeyboard`]).
    ToggleKeyboardCapture,
    /// Connection ▸ Send Key: one key the keyboard lacks.
    SendKey(SendKey),
    /// Help ▸ Keyboard Shortcuts: the cheat sheet.
    KeyboardShortcuts,
    ToggleFullscreen,
    TogglePointerLock,
    ToggleTabletMonitor,
    /// Type the clipboard's plain text into the host as keystrokes.
    PasteAsKeystrokes,
    /// Stop a paste being typed (Esc does the same).
    StopTyping,
    /// Keyboard mode for this session: physical keys.
    KeyboardPhysical,
    /// Keyboard mode for this session: characters.
    KeyboardCharacters,
    /// Show/hide the negotiated-truth session-info panel: actual
    /// codec/chroma/depth/range/matrix, hardware-vs-software encode and
    /// decode, and any degradation from what settings asked for.
    ToggleNegotiatedTruth,
    /// Show/hide the Colour check: what the host serves, what this machine
    /// shows, and whether the code values arrive exactly.
    ToggleColourCheck,
    /// Cycle which test pattern the live pixel-exactness readout compares
    /// the decoded frame against (developer builds).
    CycleTestPattern,
}

impl MenuCommand {
    /// The menu title. Commands with a Deck chord take the shared
    /// [`DeckShortcut::title`], so both Decks list them the same way.
    #[must_use]
    pub const fn title(self, platform: DeckPlatform) -> &'static str {
        if let Some(shortcut) = self.shortcut() {
            return match (shortcut, platform) {
                (DeckShortcut::Quit, DeckPlatform::Pc) => "Exit",
                _ => shortcut.title(),
            };
        }
        match self {
            Self::About => "About Arcen Deck",
            Self::ToggleHealth => "Connection Health",
            Self::ToggleStreamStats => "Stream Statistics",
            Self::ToggleLosslessTiles => "Lossless Tile View",
            Self::ReleaseModifiers => "Release Stuck Keys",
            Self::SendKey(key) => key.title(),
            Self::KeyboardShortcuts => "Keyboard Shortcuts",
            Self::TogglePointerLock => "Pointer Lock",
            Self::ToggleTabletMonitor => "Tablet Monitor",
            Self::StopTyping => "Stop Typing",
            Self::ToggleNegotiatedTruth => "Negotiated Truth",
            Self::ToggleColourCheck => "Colour Check…",
            Self::CycleTestPattern => "Cycle Test Pattern Readout",
            Self::Quit { .. }
            | Self::Disconnect
            | Self::SendSecureAttention
            | Self::ToggleKeyboardCapture
            | Self::ToggleFullscreen
            | Self::PasteAsKeystrokes
            | Self::KeyboardPhysical
            | Self::KeyboardCharacters => "",
        }
    }

    /// The Deck chord this command has, if any.
    #[must_use]
    pub const fn shortcut(self) -> Option<DeckShortcut> {
        Some(match self {
            Self::Quit { .. } => DeckShortcut::Quit,
            Self::Disconnect => DeckShortcut::Disconnect,
            Self::SendSecureAttention => DeckShortcut::SendSecureAttention,
            Self::ToggleKeyboardCapture => DeckShortcut::ReleaseKeyboard,
            Self::ToggleFullscreen => DeckShortcut::ToggleFullscreen,
            Self::PasteAsKeystrokes => DeckShortcut::PasteAsKeystrokes,
            Self::KeyboardPhysical => DeckShortcut::KeyboardPhysical,
            Self::KeyboardCharacters => DeckShortcut::KeyboardCharacters,
            _ => return None,
        })
    }

    /// The command a Deck chord stands for on `platform` (the keyboard
    /// reports the chord; the shell applies the command as if chosen in the
    /// menu). The quit chord is the keyboard's hold-to-quit.
    #[must_use]
    pub fn for_shortcut(platform: DeckPlatform, shortcut: DeckShortcut) -> Option<Self> {
        if shortcut == DeckShortcut::Quit {
            return Some(Self::Quit {
                from_keyboard: true,
            });
        }
        deck_menus(platform, false)
            .into_iter()
            .flat_map(|menu| menu.entries)
            .flat_map(MenuEntry::commands)
            .find(|command| command.shortcut() == Some(shortcut))
    }

    /// Whether the command means anything without a session.
    #[must_use]
    pub const fn available_without_session(self) -> bool {
        matches!(
            self,
            Self::Quit { .. } | Self::About | Self::ToggleColourCheck | Self::KeyboardShortcuts
        )
    }
}

/// Connection ▸ Send Key's items, in [`SendKey::ALL`] order.
pub const SEND_KEY_COMMANDS: [MenuCommand; SendKey::ALL.len()] = {
    let mut commands = [MenuCommand::ReleaseModifiers; SendKey::ALL.len()];
    let mut index = 0;
    while index < commands.len() {
        commands[index] = MenuCommand::SendKey(SendKey::ALL[index]);
        index += 1;
    }
    commands
};

/// One line of a menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuEntry {
    Command(MenuCommand),
    Separator,
    /// A submenu of commands (Send Key).
    Submenu {
        title: &'static str,
        commands: &'static [MenuCommand],
    },
}

impl MenuEntry {
    /// The commands on this line, a submenu's included.
    #[must_use]
    pub fn commands(self) -> Vec<MenuCommand> {
        match self {
            Self::Command(command) => vec![command],
            Self::Separator => Vec::new(),
            Self::Submenu { commands, .. } => commands.to_vec(),
        }
    }
}

/// A top-level menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Menu {
    pub title: &'static str,
    pub entries: Vec<MenuEntry>,
}

/// The Deck's menus, after the Mac's application menu (which macOS owns).
/// A PC Deck adds its keyboard commands, Exit and About (PARITY Q-1).
#[must_use]
pub fn deck_menus(platform: DeckPlatform, dev_tools: bool) -> Vec<Menu> {
    use MenuCommand as C;
    use MenuEntry::{Command, Separator};
    let pc = platform == DeckPlatform::Pc;
    let mut connection = vec![Command(C::Disconnect), Command(C::ReleaseModifiers)];
    if pc {
        connection.push(Command(C::SendSecureAttention));
    }
    connection.push(Command(C::ToggleKeyboardCapture));
    connection.push(MenuEntry::Submenu {
        title: "Send Key",
        commands: &SEND_KEY_COMMANDS,
    });
    connection.extend([
        Separator,
        Command(C::PasteAsKeystrokes),
        Command(C::StopTyping),
        Separator,
        Command(C::KeyboardPhysical),
        Command(C::KeyboardCharacters),
    ]);
    if pc {
        connection.extend([
            Separator,
            Command(C::Quit {
                from_keyboard: false,
            }),
        ]);
    }
    let view = vec![
        Command(C::ToggleFullscreen),
        Command(C::TogglePointerLock),
        Command(C::ToggleTabletMonitor),
    ];
    let mut session = vec![
        Command(C::ToggleHealth),
        Command(C::ToggleStreamStats),
        Command(C::ToggleLosslessTiles),
        Command(C::ToggleNegotiatedTruth),
        Command(C::ToggleColourCheck),
    ];
    if dev_tools {
        session.push(Command(C::CycleTestPattern));
    }
    let mut menus = vec![
        Menu {
            title: "Connection",
            entries: connection,
        },
        Menu {
            title: "View",
            entries: view,
        },
        Menu {
            title: "Session",
            entries: session,
        },
    ];
    menus.push(Menu {
        title: "Help",
        entries: if pc {
            vec![Command(C::About), Command(C::KeyboardShortcuts)]
        } else {
            vec![Command(C::KeyboardShortcuts)]
        },
    });
    menus
}

/// How long the quit chord (⌘Q on a Mac, Alt+F4 on a PC) must be held in
/// a session to quit the Deck; a shorter tap goes to the host.
pub const QUIT_HOLD: std::time::Duration = std::time::Duration::from_secs(3);

/// What a held quit chord does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuitHoldStep {
    /// Still held, not long enough yet.
    Holding,
    /// Held for [`QUIT_HOLD`]: the Deck quits.
    Quit,
    /// Let go sooner: the tap was for the host.
    SendToHost,
}

/// The next step of a quit hold `elapsed` long, by whether the chord is
/// still `held`.
#[must_use]
pub const fn quit_hold_step(held: bool, elapsed: std::time::Duration) -> QuitHoldStep {
    if !held {
        QuitHoldStep::SendToHost
    } else if elapsed.as_millis() >= QUIT_HOLD.as_millis() {
        QuitHoldStep::Quit
    } else {
        QuitHoldStep::Holding
    }
}

/// The notice shown while the quit chord is held, `elapsed` into the hold.
pub fn paint_quit_hold_notice(
    ctx: &egui::Context,
    platform: DeckPlatform,
    elapsed: std::time::Duration,
) {
    let remaining = QUIT_HOLD.saturating_sub(elapsed).as_secs_f32();
    let (hold, let_go) = match platform {
        DeckPlatform::Mac => (
            format!("Hold ⌘Q to quit Arcen Deck  ({remaining:.0}s)"),
            "Let go to send ⌘Q to the remote Mac",
        ),
        DeckPlatform::Pc => (
            format!("Hold Alt+F4 to exit Arcen Deck  ({remaining:.0}s)"),
            "Let go to send Alt+F4 to the remote computer",
        ),
    };
    egui::Area::new(egui::Id::new("arcen-quit-hold"))
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 48.0))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(egui::Color32::from_black_alpha(210))
                .corner_radius(10.0)
                .inner_margin(egui::Margin::symmetric(18, 12))
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(hold)
                            .size(16.0)
                            .color(egui::Color32::WHITE),
                    );
                    ui.label(
                        egui::RichText::new(let_go)
                            .size(12.0)
                            .color(egui::Color32::from_gray(200)),
                    );
                });
        });
}

const CONNECTION_BAR_ID: &str = "arcen_connection_bar";
const CONNECTION_BAR_REVEAL_HALF_WIDTH: f32 = 180.0;
const CONNECTION_BAR_REVEAL_HEIGHT: f32 = 5.0;

/// The strip at the top edge of a fullscreen session that drops the
/// connection bar down when the pointer reaches it: centred, 360 points
/// wide, 5 points tall.
#[must_use]
pub fn connection_bar_reveal_zone(screen: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_max(
        egui::pos2(
            screen.center().x - CONNECTION_BAR_REVEAL_HALF_WIDTH,
            screen.top(),
        ),
        egui::pos2(
            screen.center().x + CONNECTION_BAR_REVEAL_HALF_WIDTH,
            screen.top() + CONNECTION_BAR_REVEAL_HEIGHT,
        ),
    )
}

/// Whether the fullscreen connection bar is open: the pointer is in the
/// top-edge strip above it ([`connection_bar_reveal_zone`]) or on the open
/// bar, or one of its menus is open. Fullscreen draws nothing over the
/// video otherwise.
#[must_use]
pub fn connection_bar_open(ctx: &egui::Context) -> bool {
    let zone = connection_bar_reveal_zone(ctx.content_rect());
    let at_top_edge = ctx
        .input(|input| input.pointer.hover_pos())
        .is_some_and(|at| zone.contains(at));
    let held_open = ctx.data(|data| {
        data.get_temp::<bool>(egui::Id::new(CONNECTION_BAR_ID))
            .unwrap_or(false)
    });
    at_top_edge || held_open || egui::Popup::is_any_open(ctx)
}

/// One pass of the fullscreen connection bar.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ConnectionBarPass {
    /// The command chosen this pass.
    pub command: Option<MenuCommand>,
    /// Where the open bar is (points); `None` while it is closed.
    pub area: Option<egui::Rect>,
    /// One of its menus is open: the Deck's UI has the whole window.
    pub menu_open: bool,
}

/// The fullscreen connection bar (PARITY Q-1): the same menus as the
/// window's menu bar, pinned top-centre over the session. Nothing is drawn
/// while it is closed, so the video is pixel for pixel what the host sent;
/// it opens when the pointer reaches the top edge above it, or while
/// `pinned` (Ctrl+Alt+Home gave it the keyboard), and stays open while the
/// pointer is on it or one of its menus is open. `title` names the
/// connection; `enabled` says which commands can be chosen.
pub fn render_connection_bar(
    ctx: &egui::Context,
    platform: DeckPlatform,
    title: &str,
    pinned: bool,
    enabled: impl Fn(MenuCommand) -> bool,
) -> ConnectionBarPass {
    let id = egui::Id::new(CONNECTION_BAR_ID);
    if !(pinned || connection_bar_open(ctx)) {
        ctx.data_mut(|data| data.insert_temp(id, false));
        return ConnectionBarPass::default();
    }
    let mut chosen = None;
    let area = egui::Area::new(id)
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::CENTER_TOP, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            let visuals = ui.visuals().clone();
            egui::Frame::new()
                .fill(visuals.window_fill)
                .stroke(visuals.window_stroke)
                .corner_radius(egui::CornerRadius {
                    nw: 0,
                    ne: 0,
                    sw: 6,
                    se: 6,
                })
                .inner_margin(egui::Margin::symmetric(10, 4))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(title).strong());
                        ui.separator();
                        for menu in deck_menus(platform, false) {
                            ui.menu_button(menu.title, |ui| {
                                for entry in &menu.entries {
                                    match entry {
                                        MenuEntry::Separator => {
                                            ui.separator();
                                        }
                                        MenuEntry::Command(command) => {
                                            if let Some(command) =
                                                bar_command(ui, platform, *command, &enabled)
                                            {
                                                chosen = Some(command);
                                            }
                                        }
                                        MenuEntry::Submenu { title, commands } => {
                                            ui.menu_button(*title, |ui| {
                                                for command in *commands {
                                                    if let Some(command) = bar_command(
                                                        ui, platform, *command, &enabled,
                                                    ) {
                                                        chosen = Some(command);
                                                    }
                                                }
                                            });
                                        }
                                    }
                                }
                            });
                        }
                    });
                });
        });
    let keep_open = area.response.contains_pointer();
    ctx.data_mut(|data| data.insert_temp(id, keep_open));
    ConnectionBarPass {
        command: chosen,
        area: Some(area.response.rect),
        menu_open: egui::Popup::is_any_open(ctx),
    }
}

/// One command button of the connection bar; the command when clicked.
fn bar_command(
    ui: &mut egui::Ui,
    platform: DeckPlatform,
    command: MenuCommand,
    enabled: &impl Fn(MenuCommand) -> bool,
) -> Option<MenuCommand> {
    let mut button = egui::Button::new(command.title(platform));
    if let Some(shortcut) = command.shortcut() {
        let keys = super::keyboard_help::shortcut_chord_text(
            platform,
            shortcut,
            super::wording::keyboard_switch(ui.ctx()),
        );
        if !keys.is_empty() {
            button = button.shortcut_text(keys);
        }
    }
    if !ui.add_enabled(enabled(command), button).clicked() {
        return None;
    }
    ui.close();
    Some(match command {
        MenuCommand::Quit { .. } => MenuCommand::Quit {
            from_keyboard: false,
        },
        other => other,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles(platform: DeckPlatform) -> Vec<(&'static str, Vec<&'static str>)> {
        deck_menus(platform, false)
            .into_iter()
            .map(|menu| {
                let items = menu
                    .entries
                    .iter()
                    .map(|entry| match entry {
                        MenuEntry::Command(command) => command.title(platform),
                        MenuEntry::Separator => "-",
                        MenuEntry::Submenu { title, .. } => title,
                    })
                    .collect();
                (menu.title, items)
            })
            .collect()
    }

    #[test]
    fn the_mac_menus_are_the_ones_its_menu_bar_has_always_shown() {
        assert_eq!(
            titles(DeckPlatform::Mac),
            vec![
                (
                    "Connection",
                    vec![
                        "Disconnect",
                        "Release Stuck Keys",
                        "Keyboard: Host or This Computer",
                        "Send Key",
                        "-",
                        "Paste as Keystrokes",
                        "Stop Typing",
                        "-",
                        "Keyboard: Physical Keys",
                        "Keyboard: Characters",
                    ]
                ),
                (
                    "View",
                    // "Tablet Orientation Left-handed" left with Q-17
                    // (2026-11-15): handedness is the pen driver's setting.
                    vec!["Show Fullscreen", "Pointer Lock", "Tablet Monitor"]
                ),
                (
                    "Session",
                    vec![
                        "Connection Health",
                        "Stream Statistics",
                        "Lossless Tile View",
                        "Negotiated Truth",
                        "Colour Check…",
                    ]
                ),
                ("Help", vec!["Keyboard Shortcuts"]),
            ]
        );
    }

    #[test]
    fn a_pc_adds_its_keyboard_commands_exit_and_help() {
        let pc = titles(DeckPlatform::Pc);
        assert_eq!(pc.len(), 4);
        assert_eq!(
            pc[0].1,
            vec![
                "Disconnect",
                "Release Stuck Keys",
                "Send Ctrl+Alt+Del",
                "Keyboard: Host or This Computer",
                "Send Key",
                "-",
                "Paste as Keystrokes",
                "Stop Typing",
                "-",
                "Keyboard: Physical Keys",
                "Keyboard: Characters",
                "-",
                "Exit",
            ]
        );
        assert_eq!(
            pc[3],
            ("Help", vec!["About Arcen Deck", "Keyboard Shortcuts"])
        );
        for (_, items) in pc {
            assert!(items.iter().all(|title| !title.is_empty()));
        }
    }

    /// The bar after three passes with the pointer at `pointer`.
    fn connection_bar_pass(pointer: Option<egui::Pos2>, pinned: bool) -> ConnectionBarPass {
        let ctx = egui::Context::default();
        let size = egui::vec2(1280.0, 800.0);
        let mut pass = ConnectionBarPass::default();
        for _ in 0..3 {
            let mut input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                ..Default::default()
            };
            if let Some(at) = pointer {
                input.events.push(egui::Event::PointerMoved(at));
            }
            let output = ctx.run_ui(input, |ui| {
                pass = render_connection_bar(ui.ctx(), DeckPlatform::Pc, "pier", pinned, |_| true);
            });
            if pass.area.is_none() {
                // Fullscreen is pixel-perfect: nothing at all is drawn.
                assert!(output.shapes.is_empty(), "{:?}", output.shapes);
            }
        }
        pass
    }

    #[test]
    fn nothing_is_drawn_until_the_pointer_reaches_the_top_edge_or_the_bar_is_pinned() {
        assert_eq!(connection_bar_pass(None, false).area, None);
        assert_eq!(
            connection_bar_pass(Some(egui::pos2(640.0, 400.0)), false).area,
            None
        );
        assert_eq!(
            connection_bar_pass(Some(egui::pos2(900.0, 0.0)), false).area,
            None
        );
        let open = connection_bar_pass(Some(egui::pos2(640.0, 1.0)), false)
            .area
            .expect("open at the top edge");
        assert!(open.height() > 15.0, "open bar {open:?}");
        assert!(open.contains(egui::pos2(640.0, 1.0)));
        // Ctrl+Alt+Home: open wherever the pointer is.
        assert!(connection_bar_pass(None, true).area.is_some());
        assert!(
            connection_bar_pass(Some(egui::pos2(640.0, 400.0)), true)
                .area
                .is_some()
        );
    }

    #[test]
    fn the_reveal_zone_is_the_top_edge_strip_above_the_bar() {
        let zone = connection_bar_reveal_zone(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1280.0, 800.0),
        ));
        assert_eq!(zone.min, egui::pos2(460.0, 0.0));
        assert_eq!(zone.max, egui::pos2(820.0, 5.0));
    }

    #[test]
    fn the_connection_bar_is_open_only_with_the_pointer_at_the_top_edge_above_it() {
        let open_with = |pointer: Option<egui::Pos2>| {
            let ctx = egui::Context::default();
            let mut input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 800.0),
                )),
                ..Default::default()
            };
            if let Some(at) = pointer {
                input.events.push(egui::Event::PointerMoved(at));
            }
            let mut open = false;
            let _ = ctx.run_ui(input, |ui| open = connection_bar_open(ui.ctx()));
            open
        };
        assert!(!open_with(None));
        assert!(!open_with(Some(egui::pos2(640.0, 400.0))));
        assert!(!open_with(Some(egui::pos2(900.0, 0.0))));
        assert!(open_with(Some(egui::pos2(640.0, 1.0))));
    }

    #[test]
    fn send_key_lists_the_shared_catalogue_on_both_decks() {
        for platform in [DeckPlatform::Mac, DeckPlatform::Pc] {
            let connection = deck_menus(platform, false).remove(0);
            let send_key = connection
                .entries
                .iter()
                .find_map(|entry| match entry {
                    MenuEntry::Submenu { title, commands } if *title == "Send Key" => {
                        Some(*commands)
                    }
                    _ => None,
                })
                .expect("Send Key");
            let titles: Vec<_> = send_key
                .iter()
                .map(|command| command.title(platform))
                .collect();
            assert_eq!(titles.len(), 18);
            assert_eq!(titles[..3], ["Print Screen", "Scroll Lock", "Pause"]);
            assert_eq!(titles[17], "F24");
        }
    }

    #[test]
    fn every_menu_chord_maps_back_to_its_command() {
        for platform in [DeckPlatform::Mac, DeckPlatform::Pc] {
            for menu in deck_menus(platform, false) {
                for command in menu.entries.into_iter().flat_map(MenuEntry::commands) {
                    if let Some(shortcut) = command.shortcut()
                        && shortcut != DeckShortcut::Quit
                    {
                        assert_eq!(MenuCommand::for_shortcut(platform, shortcut), Some(command));
                    }
                }
            }
        }
        assert_eq!(
            MenuCommand::for_shortcut(DeckPlatform::Pc, DeckShortcut::Quit),
            Some(MenuCommand::Quit {
                from_keyboard: true
            })
        );
    }
}
