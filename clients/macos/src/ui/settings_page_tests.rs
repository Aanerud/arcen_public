// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The shared Settings page with the fonts this Deck really draws with
//! (the shared crate's own tests have no fonts): every symbol the screens
//! use has a glyph, and no text runs past the window's right edge.

use arcen_deck_core::screens::settings::{
    render_settings, SettingsFacts, SettingsTab, SettingsValues,
};
use arcen_deck_core::screens::theme;
use arcen_deck_core::screens::wording::{set_deck_platform, DeckPlatform};

fn themed_context(platform: DeckPlatform) -> egui::Context {
    let ctx = egui::Context::default();
    theme::apply(&ctx);
    set_deck_platform(&ctx, platform);
    // Fonts set before a pass are in use from that pass on.
    let _ = ctx.run_ui(egui::RawInput::default(), |_| {});
    ctx
}

/// Where the glyph `c` is drawn from in the font atlas.
fn glyph_uv(ctx: &egui::Context, font: &egui::FontId, c: char) -> ([u16; 2], [u16; 2]) {
    let galley = ctx
        .fonts_mut(|fonts| fonts.layout_no_wrap(c.to_string(), font.clone(), egui::Color32::BLACK));
    let uv = galley.rows[0].glyphs[0].uv_rect;
    (uv.min, uv.max)
}

/// Whether the fonts draw `c` as itself. `has_glyph` answers no for any
/// glyph of the face that also holds the replacement box (an egui TODO),
/// which is the emoji face, so a symbol also counts when it draws
/// differently from a character no font has.
fn draws(ctx: &egui::Context, font: &egui::FontId, c: char) -> bool {
    ctx.fonts_mut(|fonts| fonts.has_glyph(font, c))
        || glyph_uv(ctx, font, c) != glyph_uv(ctx, font, '\u{E000}')
}

#[test]
fn the_fonts_draw_every_symbol_the_screens_use() {
    let ctx = themed_context(DeckPlatform::Mac);
    for size in [11.0, 16.0, 19.0] {
        let font = egui::FontId::proportional(size);
        let missing: String = theme::DRAWN_SYMBOLS
            .chars()
            .filter(|symbol| !draws(&ctx, &font, *symbol))
            .collect();
        assert_eq!(missing, "", "no glyph at {size} points");
        // The keys no bundled font has, which the screens spell out.
        for spelled in ['⌃', '⌥'] {
            assert!(!draws(&ctx, &font, spelled), "{spelled:?} is drawn now");
        }
    }
}

fn text_rects(
    shape: &egui::Shape,
    clip: egui::Rect,
    out: &mut Vec<(String, egui::Rect, egui::Rect)>,
) {
    match shape {
        egui::Shape::Text(text) => out.push((
            text.galley.job.text.clone(),
            text.visual_bounding_rect(),
            clip,
        )),
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                text_rects(shape, clip, out);
            }
        }
        _ => {}
    }
}

#[test]
fn no_settings_text_runs_past_the_right_edge_of_a_narrow_window() {
    for platform in [DeckPlatform::Mac, DeckPlatform::Pc] {
        for tab in [
            SettingsTab::Input,
            SettingsTab::Tablet,
            SettingsTab::Advanced,
            SettingsTab::Microphone,
            SettingsTab::Displays,
        ] {
            let ctx = themed_context(platform);
            let settings = arcen_deck_core::settings::ClientSettings::default();
            let mut values = SettingsValues::from_client_settings(&settings, tab);
            let facts = SettingsFacts {
                hdr_screen: false,
                notch: None,
                monitors: &[],
                host_clipboard_notice: None,
                connections: &[],
                native_tablet: None,
                decode: None,
                support_bundle: None,
                connection_displays: None,
                canvas_available: false,
                system_shortcut_permission: Some(false),
            };
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(640.0, 3000.0));
            let input = egui::RawInput {
                screen_rect: Some(screen),
                ..egui::RawInput::default()
            };
            let output = ctx.run_ui(input, |ui| {
                let _ = render_settings(ui, &mut values, &facts);
            });
            let mut texts = Vec::new();
            for clipped in &output.shapes {
                text_rects(&clipped.shape, clipped.clip_rect, &mut texts);
            }
            assert!(!texts.is_empty());
            for (text, rect, clip) in texts {
                let right = clip.max.x.min(screen.max.x);
                assert!(
                    rect.max.x <= right + 1.0,
                    "{tab:?} on {platform:?}: {text:?} ends at {} past {right}",
                    rect.max.x
                );
            }
        }
    }
}

#[test]
fn learn_more_never_breaks_across_lines() {
    for platform in [DeckPlatform::Mac, DeckPlatform::Pc] {
        for tab in [
            SettingsTab::Input,
            SettingsTab::Tablet,
            SettingsTab::Displays,
        ] {
            let ctx = themed_context(platform);
            let settings = arcen_deck_core::settings::ClientSettings::default();
            let facts = SettingsFacts {
                hdr_screen: false,
                notch: None,
                monitors: &[],
                host_clipboard_notice: None,
                connections: &[],
                native_tablet: None,
                decode: None,
                support_bundle: None,
                connection_displays: None,
                canvas_available: false,
                system_shortcut_permission: Some(false),
            };
            // Every window width puts the end of the intro line somewhere
            // else; the link must never be split at any of them.
            for width in (560..=1100).step_by(6) {
                let mut values = SettingsValues::from_client_settings(&settings, tab);
                let screen =
                    egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width as f32, 3000.0));
                let input = egui::RawInput {
                    screen_rect: Some(screen),
                    ..egui::RawInput::default()
                };
                let output = ctx.run_ui(input, |ui| {
                    let _ = render_settings(ui, &mut values, &facts);
                });
                let mut texts = Vec::new();
                for clipped in &output.shapes {
                    text_rects(&clipped.shape, clipped.clip_rect, &mut texts);
                }
                let links: Vec<_> = texts
                    .iter()
                    .filter(|(text, ..)| {
                        text == arcen_deck_core::screens::input_choices::LEARN_MORE
                    })
                    .collect();
                assert_eq!(links.len(), 1, "{tab:?} on {platform:?} at {width}");
                for (_, rect, _) in links {
                    assert!(
                        rect.height() < 30.0,
                        "{tab:?} on {platform:?} at {width}: Learn more breaks across lines"
                    );
                }
            }
        }
    }
}

/// A Home scene to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HomeScene {
    Connect,
    New,
    Edit,
    /// Edit, with a connection that keeps its own Typing.
    EditOwn,
    Empty,
}

/// Every text one Home scene draws with the real fonts in a window `size`
/// points (less the 36-point bar), after a pass to settle: its text, where
/// and its clip.
fn home_scene_texts(
    platform: DeckPlatform,
    scene: HomeScene,
    size: egui::Vec2,
) -> Vec<(String, egui::Rect, egui::Rect)> {
    use arcen_deck_core::config::{
        ConnectionKind, SavedConnectionSettings, SavedConnectionSummary, SavedTransport,
    };
    use arcen_deck_core::screens::home::{
        apply_home_action, render_home, ConnectionDraft, HomeAction, HomeViewState,
    };
    let mut saved = vec![SavedConnectionSummary {
        id: "studio".to_owned(),
        kind: ConnectionKind::DirectMachine,
        name: "A workstation with a long name".to_owned(),
        host: "workstation.example.internal".to_owned(),
        port: 18444,
        use_tls: true,
        transport: SavedTransport::Quic,
        identity_security_mode: None,
        settings: SavedConnectionSettings::default(),
        unknown: serde_json::Map::new(),
    }];
    if scene == HomeScene::EditOwn {
        saved[0].settings.overrides.typing_mode =
            arcen_deck_core::overrides::Override::Own(arcen_input::text::TypingMode::Keys);
    }
    if scene == HomeScene::Empty {
        saved.clear();
    }
    let ctx = themed_context(platform);
    let mut direct = ConnectionDraft::new(ConnectionKind::DirectMachine);
    let (mut search, mut show_add_form, mut editing) = (String::new(), false, None);
    let mut form = [String::new(), String::new(), String::new()];
    let mut overrides = arcen_deck_core::overrides::ConnectionOverrides::default();
    let mut sign_in = arcen_deck_core::sign_in::SignInState::default();
    let mut remember = true;
    let mut notice = None;
    let [name, host, user] = &mut form;
    let mut state = HomeViewState {
        direct: &mut direct,
        saved_connections: &saved,
        search: &mut search,
        operator_notice: None,
        show_add_form: &mut show_add_form,
        editing_index: &mut editing,
        form_name: name,
        form_host: host,
        form_username: user,
        form_overrides: &mut overrides,
        app_defaults: arcen_deck_core::overrides::AppDefaults::default(),
        sign_in: &mut sign_in,
        overlay_open: false,
        remember_username: &mut remember,
        connection_test: None,
        notice: &mut notice,
    };
    let opening = match scene {
        HomeScene::Connect => Some(HomeAction::SelectConnection(0)),
        HomeScene::New => Some(HomeAction::BeginAdd),
        HomeScene::Edit | HomeScene::EditOwn => Some(HomeAction::Edit(0)),
        HomeScene::Empty => None,
    };
    if let Some(action) = opening {
        let repository = arcen_deck_core::settings::ClientConfigRepository::new(None);
        let _ = apply_home_action(
            &mut state,
            action,
            &repository,
            arcen_deck_core::settings::SecurityMode::High,
        );
    }
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(size.x, size.y - 36.0));
    let input = || egui::RawInput {
        screen_rect: Some(screen),
        ..egui::RawInput::default()
    };
    let _ = ctx.run_ui(input(), |ui| {
        let _ = render_home(ui, &mut state);
    });
    let output = ctx.run_ui(input(), |ui| {
        let _ = render_home(ui, &mut state);
    });
    let mut texts = Vec::new();
    for clipped in &output.shapes {
        text_rects(&clipped.shape, clipped.clip_rect, &mut texts);
    }
    texts
}

const ALL_HOME_SCENES: [HomeScene; 5] = [
    HomeScene::Connect,
    HomeScene::New,
    HomeScene::Edit,
    HomeScene::EditOwn,
    HomeScene::Empty,
];

#[test]
fn no_home_text_runs_past_the_right_edge_of_the_narrowest_window() {
    for platform in [DeckPlatform::Mac, DeckPlatform::Pc] {
        for scene in ALL_HOME_SCENES {
            // The window's narrowest (860 points wide).
            let size = egui::vec2(860.0, 1400.0);
            let texts = home_scene_texts(platform, scene, size);
            assert!(texts.iter().any(|(text, ..)| text == "Connections"));
            // An empty field draws an empty text with no extent.
            for (text, rect, clip) in texts.into_iter().filter(|(text, ..)| !text.is_empty()) {
                let right = clip.max.x.min(size.x);
                assert!(
                    rect.max.x <= right + 1.0,
                    "{platform:?}, {scene:?}: {text:?} ends at {} past {right}",
                    rect.max.x
                );
            }
        }
    }
}

#[test]
fn every_home_button_shows_at_the_default_window_size_without_scrolling() {
    // Both Decks open at 1120 x 760, with no list open.
    let size = egui::vec2(1120.0, 760.0);
    for platform in [DeckPlatform::Mac, DeckPlatform::Pc] {
        for (scene, buttons) in [
            (HomeScene::Connect, &["Edit", "Connect"][..]),
            (
                HomeScene::New,
                &["Test connection", "Cancel", "Add connection"][..],
            ),
            (HomeScene::Edit, &["Test connection", "Cancel", "Save"][..]),
            (HomeScene::Empty, &["Add connection", "Import\u{2026}"][..]),
        ] {
            let texts = home_scene_texts(platform, scene, size);
            for button in buttons {
                let Some((_, rect, clip)) = texts.iter().find(|(text, ..)| text == button) else {
                    panic!("{platform:?}, {scene:?}: no {button:?}");
                };
                // Inside the window, not clipped away by the scroll area.
                assert!(
                    rect.max.y <= clip.max.y.min(size.y - 36.0),
                    "{platform:?}, {scene:?}: {button:?} at {rect:?} below {clip:?}"
                );
                // Edit's label fits its fixed width with room either side
                // (the other buttons are measured from their labels).
                if *button == "Edit" {
                    assert!(
                        rect.width() <= theme::EDIT_BUTTON_WIDTH - 2.0 * 16.0,
                        "{platform:?}: Edit is {} wide",
                        rect.width()
                    );
                }
            }
        }
    }
}

#[test]
fn the_connection_form_s_help_never_runs_wider_than_its_fields() {
    let size = egui::vec2(1120.0, 1400.0);
    for platform in [DeckPlatform::Mac, DeckPlatform::Pc] {
        let texts = home_scene_texts(platform, HomeScene::EditOwn, size);
        let Some((_, label, _)) = texts.iter().find(|(text, ..)| text == "Typing") else {
            panic!("{platform:?}: no Typing picker on the form");
        };
        let right = label.min.x + theme::COLUMN_MAX;
        // Everything under the form's title stays inside the inputs' column.
        for (text, rect, _) in texts.iter().filter(|(text, rect, _)| {
            !text.is_empty() && rect.min.x >= label.min.x - 1.0 && rect.min.y > label.min.y
        }) {
            assert!(
                rect.max.x <= right + 1.0,
                "{platform:?}: {text:?} ends at {} past {right}",
                rect.max.x
            );
        }
    }
}
