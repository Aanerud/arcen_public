// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Home's actions against a real configuration directory: both Decks save,
//! select and delete connections through `apply_home_action`.
#![cfg(feature = "egui")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use arcen_deck_core::config::{ConnectionKind, SavedConnectionSummary};
use arcen_deck_core::connection_draft::ConnectionDraft;
use arcen_deck_core::overrides::{ConnectionOverrides, Override};
use arcen_deck_core::screens::home::{HomeAction, HomeEffect, HomeViewState, apply_home_action};
use arcen_deck_core::settings::{ClientConfigRepository, DisplaysMode, SecurityMode};

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
    username: String,
    overrides: ConnectionOverrides,
    remember: bool,
    sign_in: arcen_deck_core::sign_in::SignInState,
    notice: Option<arcen_deck_core::screens::home::HomeNotice>,
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
        form_username: &mut form.username,
        form_overrides: &mut form.overrides,
        app_defaults: arcen_deck_core::overrides::AppDefaults {
            displays_mode: DisplaysMode::SinglePrimary,
            ..arcen_deck_core::overrides::AppDefaults::default()
        },
        sign_in: &mut form.sign_in,
        overlay_open: false,
        remember_username: &mut form.remember,
        connection_test: None,
        notice: &mut form.notice,
    };
    let outcome = apply_home_action(&mut state, action, repository, SecurityMode::Medium);
    if let Some(store) = outcome.store {
        *saved = store.connections;
    }
    (outcome.effect, outcome.status)
}

#[test]
fn saving_a_new_connection_drops_another_connection_s_sign_in_error() {
    let dir = ScratchDir::new("save-clears-sign-in");
    let repository = ClientConfigRepository::at(dir.0.clone());
    let mut form = Form::default();
    let mut direct = ConnectionDraft::new(ConnectionKind::DirectMachine);
    direct.saved_connection_id = Some("lab".to_owned());
    direct.host = "203.0.113.82".to_owned();
    direct = form.sign_in.return_home(
        arcen_deck_core::sign_in::SignInFailure::CredentialsRefused,
        direct,
        None,
    );
    let mut saved = Vec::new();
    let (_, status) = apply(
        &mut form,
        &mut direct,
        &mut saved,
        &repository,
        HomeAction::SaveNew {
            name: "Unreachable test".to_owned(),
            host: "203.0.113.10".to_owned(),
            port: 18444,
            username: String::new(),
            overrides: ConnectionOverrides::default(),
        },
    );
    assert_eq!(status.as_deref(), Some("Added Unreachable test"));
    assert_eq!(
        direct.saved_connection_id.as_deref(),
        Some(saved[0].id.as_str())
    );
    assert!(form.sign_in.feedback().is_none());
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
    assert!(form.host.is_empty());

    let (effect, status) = apply(
        &mut form,
        &mut direct,
        &mut saved,
        &repository,
        HomeAction::SaveNew {
            name: "Studio".to_owned(),
            host: "203.0.113.82".to_owned(),
            port: 0,
            username: "alice".to_owned(),
            overrides: ConnectionOverrides::default(),
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
fn use_app_defaults_hands_every_own_setting_back_and_persists() {
    // Settings › Displays is Primary display only; the saved connection
    // keeps its own Displays, Typing and system shortcuts. "Use app
    // defaults" under its Connect clears all of them.
    let dir = ScratchDir::new("follow-defaults");
    let repository = ClientConfigRepository::at(dir.0.clone());
    let mut form = Form::default();
    let mut direct = ConnectionDraft::new(ConnectionKind::DirectMachine);
    let mut saved = Vec::new();
    let own = ConnectionOverrides {
        displays_mode: Override::Own(DisplaysMode::MatchLayout),
        typing_mode: Override::Own(arcen_input::text::TypingMode::Keys),
        system_shortcuts: Override::Own(true),
        ..ConnectionOverrides::default()
    };
    apply(
        &mut form,
        &mut direct,
        &mut saved,
        &repository,
        HomeAction::SaveNew {
            name: "Z8".to_owned(),
            host: "203.0.113.100".to_owned(),
            port: 18444,
            username: String::new(),
            overrides: own,
        },
    );
    assert_eq!(saved[0].settings.overrides, own);
    let (effect, status) = apply(
        &mut form,
        &mut direct,
        &mut saved,
        &repository,
        HomeAction::FollowAppDefaults(0),
    );
    assert!(matches!(effect, HomeEffect::None));
    assert_eq!(
        status.as_deref(),
        Some("\u{201c}Z8\u{201d} now uses the app defaults")
    );
    assert_eq!(saved[0].settings.overrides, ConnectionOverrides::default());
    let file = repository.connections_file().expect("connections file");
    let reloaded =
        arcen_deck_core::config::parse_saved_connections(&std::fs::read(file).unwrap()).unwrap();
    assert_eq!(
        reloaded.connections[0].settings.overrides,
        ConnectionOverrides::default()
    );
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
    draft.username = "artist".to_owned();
    draft.password = "dummy-password".to_owned();
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
