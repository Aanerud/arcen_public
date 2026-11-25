// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Golden fixtures of the Deck's on-disk configuration: `settings.json`,
//! `connections.json` and `trusted_pins.json`, exactly as this Deck writes
//! and re-writes them today.
//!
//! Written before the settings model, saved connections and the config
//! repository move to `arcen-deck-core` (ADR 0024, extraction step 2): the
//! moved code must produce the same bytes from the same inputs, so a user's
//! files keep loading and keep their unknown fields across the move.
//!
//! The fixtures live in `shared/deck-core/tests/fixtures/deck-config/`
//! (written by the macOS Deck before the move; the outputs were regenerated
//! once for 0.20.7's settings version 2, `config::migrate_client_settings`).
//! `*.in.json` are hand-written inputs (older shapes, unknown fields);
//! everything else is golden output. Regenerate the golden files only for an
//! intended format change: `ARCEN_UPDATE_GOLDEN=1 cargo test -p
//! arcen-deck-core --test config_golden`.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use arcen_deck_core::config::*;
use arcen_deck_core::overrides::Override;
use arcen_deck_core::settings::*;
use arcen_deck_core::trusted_pins;
use arcen_protocol::messages::{CursorMode, TabletModeMsg};

fn fixtures() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/deck-config")
}

fn fixture(name: &str) -> Vec<u8> {
    let path = fixtures().join(name);
    std::fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn golden(name: &str, actual: &[u8]) {
    let path = fixtures().join(name);
    if std::env::var_os("ARCEN_UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = fixture(name);
    assert_eq!(
        String::from_utf8_lossy(actual),
        String::from_utf8_lossy(&expected),
        "{name}: the Deck's on-disk format drifted"
    );
}

struct ScratchDir(std::path::PathBuf);

impl ScratchDir {
    fn new(name: &str) -> Self {
        let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("arcen-golden-{name}-{}", std::process::id()));
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

fn every_field_changed() -> ClientSettings {
    ClientSettings {
        log_level: LogLevel::Debug,
        security_mode: SecurityMode::Low,
        performance_mode: PerformanceMode::Standard,
        displays_mode: DisplaysMode::Windowed,
        cursor_preference: CursorMode::Host,
        clipboard_enabled: false,
        microphone_enabled: true,
        hidpi_streaming: true,
        fullscreen_uses_notch_area: true,
        remote_ui_scale_percent: 150,
        full_color_display_mask: 0b0001,
        hidpi_display_mask: 0b0010,
        tablet_input_enabled: false,
        tablet_mode_requested: TabletModeMsg::DisabledMouseCompat,
        pen_buttons: arcen_input::pen_buttons::PenButtonSettings {
            barrel: arcen_input::pen_buttons::PenButtonAction::MiddleClick,
            lower: arcen_input::pen_buttons::PenButtonAction::Off,
            upper: arcen_input::pen_buttons::PenButtonAction::RightClick,
        },
        exact_still_image: false,
        keep_desk_private: false,
        canvas: false,
        canvas_cache_mib: 32,
        color_fidelity: ColorFidelitySettings {
            preset: ColorFidelity::GradingReference,
            overrides: ColorFidelityOverrides {
                codec: Some(arcen_media::VideoCodec::Av1),
                chroma: Some(arcen_media::ChromaSubsampling::Yuv422),
                bit_depth: Some(arcen_media::BitDepth::Twelve),
                range: Some(arcen_media::ColorRange::Limited),
                matrix: Some(arcen_media::ColorMatrix::Identity),
            },
            variant_override: Some(
                arcen_media::video::VideoVariant::from_id("av1-444-12-full-bt709")
                    .expect("valid probe-matrix id"),
            ),
        },
        encode_intent: arcen_media::EncodeIntent::Quality,
        typing_mode: arcen_input::text::TypingMode::Keys,
        option_key_mode: arcen_input::text::OptionKeyMode::BothAlt.into(),
        paste: arcen_input::text::PasteSettings {
            confirm_above: 500,
            chars_per_second: 10,
        },
        keyboard_capture: arcen_input::keyboard::KeyboardCapturePolicy::ThisComputer,
        capture_mac_system_shortcuts: true,
        mac_system_shortcut_offer_answered: true,
        keyboard_switch: arcen_input::keyboard::KeyboardSwitchChord::CONTROL_ALT_SHIFT_Z,
    }
}

#[test]
fn settings_json_golden_for_defaults_every_field_and_a_legacy_file() {
    // Debug builds default to Info logging, release builds to Critical; the
    // golden is the test (debug) build's.
    let defaults = ScratchDir::new("settings-default");
    let repository = ClientConfigRepository::at(defaults.0.clone());
    save_client_settings(&repository, &ClientSettings::default()).unwrap();
    golden(
        "settings.default.json",
        &std::fs::read(defaults.0.join("settings.json")).unwrap(),
    );

    let full = ScratchDir::new("settings-full");
    let repository = ClientConfigRepository::at(full.0.clone());
    save_client_settings(&repository, &every_field_changed()).unwrap();
    let written = std::fs::read(full.0.join("settings.json")).unwrap();
    golden("settings.full.json", &written);
    // Loading the golden and saving it again is byte-stable.
    let reloaded = load_client_settings(&repository).unwrap();
    std::fs::remove_file(full.0.join("settings.json")).unwrap();
    save_client_settings(&repository, &reloaded).unwrap();
    assert_eq!(
        std::fs::read(full.0.join("settings.json")).unwrap(),
        written
    );

    // An older file keeps its unknown keys through a load and a save.
    let legacy = ScratchDir::new("settings-legacy");
    std::fs::write(
        legacy.0.join("settings.json"),
        fixture("settings.legacy.in.json"),
    )
    .unwrap();
    let repository = ClientConfigRepository::at(legacy.0.clone());
    let loaded = load_client_settings(&repository).unwrap();
    assert_eq!(loaded.security_mode, SecurityMode::High);
    assert!(!loaded.clipboard_enabled);
    // The keyboard grab's `never` is "this computer keeps its shortcuts".
    assert_eq!(
        loaded.keyboard_capture,
        arcen_input::keyboard::KeyboardCapturePolicy::ThisComputer
    );
    // The ⌘Tab switch stays; the separate ⌃←/⌃→ setting is gone (those keys
    // follow the keyboard switch) and the save drops it. A chord the
    // system never delivers falls back to the default.
    assert!(loaded.capture_mac_system_shortcuts);
    assert_eq!(
        loaded.keyboard_switch,
        arcen_input::keyboard::KeyboardSwitchChord::DEFAULT
    );
    save_client_settings(&repository, &loaded).unwrap();
    let saved = std::fs::read_to_string(legacy.0.join("settings.json")).unwrap();
    assert!(!saved.contains("mac_spaces_to_host"));
    golden(
        "settings.legacy.out.json",
        &std::fs::read(legacy.0.join("settings.json")).unwrap(),
    );
}

/// A file a newer Deck wrote keeps its version and the keys this Deck does
/// not know through a load and a save, so its own migrations never run twice.
#[test]
fn a_newer_settings_file_keeps_its_version_and_unknown_keys() {
    let dir = ScratchDir::new("settings-v3");
    std::fs::write(
        dir.0.join("settings.json"),
        br#"{"version":3,"typing_mode":"keys","a_version_3_setting":{"kept":true}}"#,
    )
    .unwrap();
    let repository = ClientConfigRepository::at(dir.0.clone());
    let loaded = load_client_settings(&repository).unwrap();
    assert_eq!(loaded.typing_mode, arcen_input::text::TypingMode::Keys);
    save_client_settings(&repository, &loaded).unwrap();
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.0.join("settings.json")).unwrap()).unwrap();
    assert_eq!(saved["version"], 3);
    assert_eq!(saved["a_version_3_setting"]["kept"], true);
}

/// The stored cursor choice is never migrated: it is what a Deck asks a
/// host older than 0.20.7 for, alongside the automatic cursor that newer
/// hosts follow, so a Host choice keeps Host on those hosts.
#[test]
fn the_stored_cursor_choice_is_kept_for_older_hosts() {
    let dir = ScratchDir::new("cursor-fallback");
    let repository = ClientConfigRepository::at(dir.0.clone());
    let file = dir.0.join("settings.json");
    for (before, expected) in [
        (r#"{"cursor_preference":"host"}"#, CursorMode::Host),
        (
            r#"{"version":2,"cursor_preference":"host"}"#,
            CursorMode::Host,
        ),
        (r#"{"cursor_preference":"local"}"#, CursorMode::Local),
    ] {
        std::fs::write(&file, before).unwrap();
        let loaded = load_client_settings(&repository).unwrap();
        assert_eq!(loaded.cursor_preference, expected, "{before}");
        save_client_settings(&repository, &loaded).unwrap();
        assert_eq!(
            load_client_settings(&repository).unwrap().cursor_preference,
            expected
        );
    }
}

#[test]
fn connections_json_golden_for_a_current_and_a_legacy_file() {
    for (input, output) in [
        ("connections.v2.in.json", "connections.v2.out.json"),
        ("connections.v1.in.json", "connections.v1.out.json"),
        ("connections.wss.in.json", "connections.wss.out.json"),
    ] {
        let store = parse_saved_connections(&fixture(input)).unwrap();
        let written = serialize_saved_connections(&store).unwrap();
        golden(output, &written);
        let again =
            serialize_saved_connections(&parse_saved_connections(&written).unwrap()).unwrap();
        assert_eq!(again, written, "{output} is not stable across a reload");
    }

    let store = parse_saved_connections(&fixture("connections.v2.in.json")).unwrap();
    assert_eq!(store.connections.len(), 2);
    assert_eq!(store.unparsed_connections.len(), 1);
    assert_eq!(
        store.last_selected_connection_id.as_deref(),
        Some("studio-a")
    );
    let studio = &store.connections[0];
    assert_eq!(
        studio.settings.remembered_username.as_deref(),
        Some(r"AD\artist.user")
    );
    // 0.20.7 (connection settings version 2): the connection's own tablet
    // mode and Left Option as AltGr are dropped, its Displays and Command
    // choices stay its own, and its keyboard mode "characters" is its own
    // Typing: Characters.
    assert_eq!(
        studio.settings.overrides,
        arcen_deck_core::overrides::ConnectionOverrides {
            typing_mode: Override::Own(arcen_input::text::TypingMode::Auto),
            keyboard_shortcuts: Override::Own(
                arcen_input::keyboard::ShortcutPreference::SwapCommandControl
            ),
            displays_mode: Override::Own(DisplaysMode::SinglePrimary),
            ..Default::default()
        }
    );
    for retired in [
        "tablet_mode_requested",
        "left_option_altgr",
        "keyboard_mode",
    ] {
        assert!(!studio.settings.unknown.contains_key(retired), "{retired}");
    }
    assert_eq!(studio.settings.unknown["future_setting"], "kept");
}

/// The Deck no longer has a WSS transport: every saved connection an older
/// Deck wrote as `"wss"` loads, and is written back, as QUIC. The WSS product
/// port 18443 becomes the QUIC port 18444; any other port, the id, the name,
/// the per-connection settings and unknown fields are kept; an entry whose
/// transport this Deck does not know (`"ws"`) stays on disk untouched.
#[test]
fn connections_json_saved_as_wss_load_as_quic_without_losing_data() {
    let store = parse_saved_connections(&fixture("connections.wss.in.json")).unwrap();
    assert_eq!(
        store.last_selected_connection_id.as_deref(),
        Some("studio-wss")
    );
    assert_eq!(store.connections.len(), 4);
    assert!(
        store
            .connections
            .iter()
            .all(|connection| connection.transport == SavedTransport::Quic)
    );
    let ports: Vec<_> = store.connections.iter().map(|c| c.port).collect();
    assert_eq!(ports, [18_444, 18_444, 443, 18_443]);

    let studio = &store.connections[0];
    assert_eq!(studio.id, "studio-wss");
    assert_eq!(studio.name, "Studio over WSS");
    assert_eq!(studio.identity_security_mode.as_deref(), Some("high"));
    assert_eq!(
        studio.settings.remembered_username.as_deref(),
        Some(r"AD\artist.user")
    );
    assert_eq!(
        studio.settings.overrides.displays_mode,
        Override::Own(DisplaysMode::SinglePrimary)
    );
    assert_eq!(studio.settings.overrides.typing_mode, Override::Inherit);
    assert_eq!(studio.unknown["future_entry_field"]["kept"], true);
    // An entry without an id gets the same id a QUIC entry at the QUIC port
    // would, so a later load finds the same connection.
    let unnamed = &store.connections[1];
    assert_eq!(
        unnamed.id,
        migrated_saved_connection_id(
            ConnectionKind::DirectMachine,
            "203.0.113.30",
            18_444,
            true,
            None,
            "WSS without an id or a port",
            1,
        )
    );
    assert_eq!(store.unparsed_connections.len(), 1);
}

#[test]
fn trusted_pins_json_golden_keeps_readable_pins_and_their_shape() {
    let dir = ScratchDir::new("pins");
    let input = dir.0.join("in.json");
    std::fs::write(&input, fixture("trusted_pins.in.json")).unwrap();
    let store = trusted_pins::load(&input);
    assert_eq!(store.pins.len(), 3, "unreadable entries are kept on disk");
    let resolved = store.resolved();
    assert_eq!(resolved.len(), 2, "unreadable entries are never trusted");

    let output = dir.0.join("trusted_pins.json");
    trusted_pins::save(&output, &store).unwrap();
    let written = std::fs::read(&output).unwrap();
    // The map's order is not stable, so the golden compares the document.
    let written_value: serde_json::Value = serde_json::from_slice(&written).unwrap();
    if std::env::var_os("ARCEN_UPDATE_GOLDEN").is_some() {
        golden(
            "trusted_pins.out.json",
            &serde_json::to_vec_pretty(&written_value).unwrap(),
        );
    }
    let expected: serde_json::Value =
        serde_json::from_slice(&fixture("trusted_pins.out.json")).unwrap();
    assert_eq!(written_value, expected, "trusted_pins.json drifted");

    // One entry is byte-exact: the on-disk formatting itself.
    let mut single = trusted_pins::TrustedPinStore::default();
    single.pins.insert(
        "pier-a.example.internal:18444".to_string(),
        store.pins["pier-a.example.internal:18444"].clone(),
    );
    let single_path = dir.0.join("single.json");
    trusted_pins::save(&single_path, &single).unwrap();
    golden(
        "trusted_pins.single.json",
        &std::fs::read(&single_path).unwrap(),
    );
}

#[test]
fn a_symbolic_link_in_the_configurations_place_is_never_replaced() {
    let dir = ScratchDir::new("symlink");
    let target = dir.0.join("elsewhere.json");
    std::fs::write(&target, b"{}").unwrap();
    let link = dir.0.join("settings.json");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link).unwrap();
    // Creating a link on Windows needs Developer Mode or the privilege.
    #[cfg(windows)]
    if std::os::windows::fs::symlink_file(&target, &link).is_err() {
        return;
    }
    let refused = atomic_write_json(&link, b"{\"replaced\":true}");
    assert!(matches!(refused, Err(ClientConfigError::Invalid(_))));
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"{}");
}
