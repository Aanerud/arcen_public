// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Home's actions against a real configuration directory: both Decks save,
//! select and delete connections through `apply_home_action`.
#![cfg(feature = "egui")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use arcen_deck_core::config::{ConnectionKind, SavedConnectionSummary};
use arcen_deck_core::connection_draft::ConnectionDraft;
use arcen_deck_core::screens::home::{HomeAction, HomeEffect, HomeViewState, apply_home_action};
use arcen_deck_core::settings::{ClientConfigRepository, DisplaysMode, SecurityMode};
use arcen_input::keyboard::KeyboardSettings;

struct ScratchDir(std::path::PathBuf);

impl ScratchDir {
    fn new(name: &str) -> Self {
        let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("arcen-home-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Form {
    search: String,
    show_add: bool,
    editing: Option<usize>,
    name: String,
    host: String,
    port: String,
    username: String,
    displays: Option<DisplaysMode>,
    keyboard: KeyboardSettings,
    remember: bool,
}

fn apply(
    form: &mut Form,
    direct: &mut ConnectionDraft,
    saved: &mut Vec<SavedConnectionSummary>,
    repository: &ClientConfigRepository,
    action: HomeAction,
) -> (HomeEffect, Option<String>) {
    let mut state = HomeViewState {
        direct,
        saved_connections: saved,
        search: &mut form.search,
        operator_notice: None,
        show_add_form: &mut form.show_add,
        editing_index: &mut form.editing,
        form_name: &mut form.name,
        form_host: &mut form.host,
        form_port: &mut form.port,
        form_username: &mut form.username,
        form_displays_mode: &mut form.displays,
        form_keyboard: &mut form.keyboard,
        app_displays_mode: DisplaysMode::SinglePrimary,
        connection_error: None,
        remember_username: &mut form.remember,
        connection_test: None,
    };
    let outcome = apply_home_action(&mut state, action, repository, SecurityMode::Medium);
    if let Some(store) = outcome.store {
        *saved = store.connections;
    }
    (outcome.effect, outcome.status)
}

#[test]
fn a_saved_connection_is_selected_and_deleting_it_clears_the_selection() {
    let dir = ScratchDir::new("save-delete");
    let repository = ClientConfigRepository::at(dir.0.clone());
    let mut form = Form::default();
    let mut direct = ConnectionDraft::new(ConnectionKind::DirectMachine);
    let mut saved = Vec::new();

    apply(
        &mut form,
        &mut direct,
        &mut saved,
        &repository,
        HomeAction::BeginAdd,
    );
    assert!(form.show_add);
    assert_eq!(form.port, "18444");

    let (effect, status) = apply(
        &mut form,
        &mut direct,
        &mut saved,
        &repository,
        HomeAction::SaveNew {
            name: "Studio".to_owned(),
            host: "203.0.113.82".to_owned(),
            port: "0".to_owned(),
            username: "alice".to_owned(),
            displays_mode: None,
            keyboard: KeyboardSettings::default(),
        },
    );
    assert!(matches!(effect, HomeEffect::None));
    assert_eq!(status.as_deref(), Some("Added Studio"));
    assert_eq!(saved.len(), 1);
    assert_eq!(
        saved[0].port, 18444,
        "port 0 falls back to the product port"
    );
    assert_eq!(
        direct.saved_connection_id.as_deref(),
        Some(saved[0].id.as_str())
    );
    assert_eq!(direct.username, "alice");
    assert!(!form.show_add);

    let (_, status) = apply(
        &mut form,
        &mut direct,
        &mut saved,
        &repository,
        HomeAction::Delete(0),
    );
    assert_eq!(status.as_deref(), Some("Removed Studio"));
    assert!(saved.is_empty());
    assert_eq!(direct.saved_connection_id, None);
}

#[test]
fn use_settings_hands_a_connections_displays_choice_back_and_persists() {
    // The field case: Settings › Displays is Primary display only, the saved
    // connection keeps Match my layout; its panel's "Use Settings" clears it.
    let dir = ScratchDir::new("follow-displays");
    let repository = ClientConfigRepository::at(dir.0.clone());
    let mut form = Form::default();
    let mut direct = ConnectionDraft::new(ConnectionKind::DirectMachine);
    let mut saved = Vec::new();
    apply(
        &mut form,
        &mut direct,
        &mut saved,
        &repository,
        HomeAction::SaveNew {
            name: "Z8".to_owned(),
            host: "203.0.113.100".to_owned(),
            port: "18444".to_owned(),
            username: String::new(),
            displays_mode: Some(DisplaysMode::MatchLayout),
            keyboard: KeyboardSettings::default(),
        },
    );
    assert_eq!(
        saved[0].settings.displays_mode,
        Some(DisplaysMode::MatchLayout)
    );
    let (effect, status) = apply(
        &mut form,
        &mut direct,
        &mut saved,
        &repository,
        HomeAction::FollowAppDisplays(0),
    );
    assert!(matches!(effect, HomeEffect::None));
    assert_eq!(
        status.as_deref(),
        Some(
            "\u{201c}Z8\u{201d} now connects with Settings \u{203a} Displays: Primary display only"
        )
    );
    assert_eq!(saved[0].settings.displays_mode, None);
    let file = repository.connections_file().expect("connections file");
    let reloaded =
        arcen_deck_core::config::parse_saved_connections(&std::fs::read(file).unwrap()).unwrap();
    assert_eq!(reloaded.connections[0].settings.displays_mode, None);
}

#[test]
fn connect_and_test_are_left_to_the_shell() {
    let dir = ScratchDir::new("effects");
    let repository = ClientConfigRepository::at(dir.0.clone());
    let mut form = Form::default();
    let mut direct = ConnectionDraft::new(ConnectionKind::DirectMachine);
    let mut saved = Vec::new();
    let mut draft = ConnectionDraft::new(ConnectionKind::DirectMachine);
    draft.host = "pier.example.internal".to_owned();
    let (effect, status) = apply(
        &mut form,
        &mut direct,
        &mut saved,
        &repository,
        HomeAction::Connect(draft),
    );
    assert!(matches!(effect, HomeEffect::Connect(draft) if draft.host == "pier.example.internal"));
    assert_eq!(status, None);
    let (effect, _) = apply(
        &mut form,
        &mut direct,
        &mut saved,
        &repository,
        HomeAction::TestConnection {
            host: "pier.example.internal".to_owned(),
            port: "18444".to_owned(),
        },
    );
    assert!(matches!(effect, HomeEffect::TestConnection { .. }));
}
