// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The words of Settings › Input, Settings › Tablet, Settings › Advanced ›
//! Input and Edit Connection's "On this machine" group (0.20.7): one
//! heading per group, one line per choice, plain words. What a choice does
//! in depth is in the user guide ("Learn more").
//!
//! These are view models: [`super::settings`] and [`super::home`] only lay
//! them out, so both Decks show the same words and the tests can read them
//! without drawing.

use arcen_input::keyboard::{KeyboardSwitchChord, ShortcutPreference};
use arcen_input::text::{OptionKeyMode, OptionKeyPreference, TypingMode};
use arcen_protocol::messages::TabletModeMsg;

use super::wording::DeckPlatform;
use crate::overrides::{AppDefaults, ConnectionOverrides, Effective, Override, SettingSource};
use crate::settings::DisplaysMode;

/// One choice in a group: its value, a short title, an optional badge
/// ("(Recommended)") and one line saying what it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice<T> {
    pub value: T,
    pub title: String,
    pub badge: Option<&'static str>,
    pub line: String,
}

impl<T> Choice<T> {
    fn new(value: T, title: impl Into<String>, line: impl Into<String>) -> Self {
        Self {
            value,
            title: title.into(),
            badge: None,
            line: line.into(),
        }
    }

    const fn recommended(mut self) -> Self {
        self.badge = Some(RECOMMENDED);
        self
    }
}

/// The badge of the choice to take when unsure.
pub const RECOMMENDED: &str = "(Recommended)";

/// The link to the user guide.
pub const LEARN_MORE: &str = "Learn more";

/// The user guide's keyboard section.
pub const KEYBOARD_GUIDE_SECTION: &str = "6-keyboard";

/// The user guide's pen and tablet section.
pub const TABLET_GUIDE_SECTION: &str = "5-pen-and-tablet";

/// A section of the user guide, in the public source every release is
/// tagged in.
#[must_use]
pub fn user_guide_url(section: &str) -> String {
    format!(
        "{}/blob/main/docs/user-guide.md#{section}",
        crate::about::SOURCE_URL
    )
}

// ── Settings › Input ──────────────────────────────────────────────────────

/// The two lines at the top of Settings › Input; "Learn more" follows the
/// second.
#[must_use]
pub fn input_intro(platform: DeckPlatform, switch: KeyboardSwitchChord) -> [String; 2] {
    [
        "Arcen sends your keyboard, mouse and pen to the remote computer.".to_owned(),
        format!(
            "Press {} to move the keyboard between {} and the remote computer.",
            super::keyboard_help::switch_chord_text(platform, switch),
            platform.this_machine()
        ),
    ]
}

/// Settings › Input › Keyboard.
pub const KEYBOARD_HEADING: &str = "Keyboard";

/// The one system-shortcuts switch
/// ([`crate::settings::ClientSettings::system_shortcuts_to_host`]).
pub const SYSTEM_SHORTCUTS_TITLE: &str = "System shortcuts go to the remote computer";

/// What the switch sends.
#[must_use]
pub const fn system_shortcuts_line(platform: DeckPlatform) -> &'static str {
    match platform {
        DeckPlatform::Mac => {
            "⌘Tab, Mission Control and other shortcuts reach the remote computer while it has \
             the keyboard."
        }
        DeckPlatform::Pc => {
            "Alt+Tab, the Windows key and other shortcuts reach the remote computer while it \
             has the keyboard."
        }
    }
}

/// The permission line under the switch, only when the switch is on and the
/// Deck knows it lacks the permission (`permission`: `None` on a Deck that
/// needs none).
#[must_use]
pub const fn system_shortcuts_permission(
    on: bool,
    permission: Option<bool>,
) -> Option<&'static str> {
    match (on, permission) {
        (true, Some(false)) => Some("Needs Accessibility permission."),
        _ => None,
    }
}

/// The button beside the permission line.
pub const OPEN_SYSTEM_SETTINGS: &str = "Open System Settings…";

/// Settings › Input › Typing.
pub const TYPING_HEADING: &str = "Typing";

/// The Typing a setting can hold, as a title: "always characters" is only
/// a session's Connection-menu choice now, and reads as Characters.
#[must_use]
pub const fn typing_title(mode: TypingMode) -> &'static str {
    match mode {
        TypingMode::Auto | TypingMode::Characters => "Characters",
        TypingMode::Keys => "Keys only",
    }
}

/// The two Typing choices.
#[must_use]
pub fn typing_choices() -> [Choice<TypingMode>; 2] {
    [
        Choice::new(
            TypingMode::Auto,
            typing_title(TypingMode::Auto),
            "Sends the letters you type, whatever the remote keyboard layout.",
        )
        .recommended(),
        Choice::new(
            TypingMode::Keys,
            typing_title(TypingMode::Keys),
            "Sends physical keys, for games and apps that read key positions.",
        ),
    ]
}

/// "1 connection uses its own", "3 connections use their own".
#[must_use]
pub fn connections_use_their_own(count: usize) -> String {
    if count == 1 {
        "1 connection uses its own".to_owned()
    } else {
        format!("{count} connections use their own")
    }
}

/// The note beside a setting a connection can override.
#[must_use]
pub fn default_for_all_connections(own: usize) -> String {
    if own == 0 {
        "Default for all connections".to_owned()
    } else {
        format!(
            "Default for all connections. {}.",
            connections_use_their_own(own)
        )
    }
}

/// Settings › Input › Clipboard.
pub const CLIPBOARD_HEADING: &str = "Clipboard";

/// The user's consent to share the clipboard.
pub const CLIPBOARD_TITLE: &str = "Share the clipboard with remote computers";

/// The line under a switch a connection can override: how many
/// connections keep their own, none while every one follows it.
#[must_use]
pub fn use_their_own_line(own: usize) -> Option<String> {
    (own > 0).then(|| connections_use_their_own(own))
}

/// The line under the clipboard switch: how many connections keep their
/// own (a connection profile can only withhold the clipboard).
#[must_use]
pub fn clipboard_line(own: usize) -> Option<String> {
    use_their_own_line(own)
}

// ── Settings › Tablet ─────────────────────────────────────────────────────

/// The tablet mode's choices, one for the whole app.
#[must_use]
pub fn tablet_choices() -> [Choice<TabletModeMsg>; 3] {
    [
        Choice::new(
            TabletModeMsg::LocalTermination,
            "Tablet support",
            "Your pen works as a pen on the remote computer.",
        )
        .recommended(),
        Choice::new(
            TabletModeMsg::UsbTabletBridge,
            "Native tablet",
            "The remote computer's tablet driver talks to your tablet.",
        ),
        Choice::new(
            TabletModeMsg::DisabledMouseCompat,
            "Mouse only",
            "Your pen moves the pointer like a mouse.",
        ),
    ]
}

/// Native tablet's badge in a build without the bridge.
pub const NATIVE_TABLET_NOT_IN_BUILD: &str = "(not in this build)";

/// The Tablet page's footer.
pub const TABLET_FOOTER: &str = "Changes apply from the next connection.";

// ── Settings › Advanced › Input ───────────────────────────────────────────

/// Settings › Advanced › Input.
pub const ADVANCED_INPUT_HEADING: &str = "Input";

/// The keyboard switch's chord.
pub const KEYBOARD_SWITCH_HEADING: &str = "Keyboard switch";

/// Paste as Keystrokes' tuning.
pub const PASTE_HEADING: &str = "Paste as Keystrokes";

/// The speed's label.
pub const PASTE_SPEED: &str = "Speed";

/// A speed choice.
#[must_use]
pub fn paste_speed_choice(chars_per_second: u32) -> String {
    if chars_per_second == arcen_input::text::paste::DEFAULT_CHARS_PER_SECOND {
        format!("{chars_per_second} characters a second (default)")
    } else {
        format!("{chars_per_second} characters a second")
    }
}

/// The ask-first limit's label; the choice follows it.
pub const PASTE_ASK_ABOVE: &str = "Ask before pasting more than";

/// An ask-first choice.
#[must_use]
pub fn paste_ask_choice(chars: usize) -> String {
    format!("{chars} characters")
}

// ── Edit Connection ───────────────────────────────────────────────────────

/// A connection's own value for one setting, as its list picker shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverrideField<T> {
    /// The field's title.
    pub title: &'static str,
    /// What the closed selector shows: "App default (Characters)" while the
    /// connection follows Settings.
    pub selected: String,
    /// What the value in force does.
    pub line: String,
    /// The choices, "App default (…)" first.
    pub choices: Vec<(Override<T>, String)>,
}

/// How a value in force reads: "App default (Characters)" when it comes
/// from Settings, the value itself when the connection keeps its own.
#[must_use]
pub fn effective_label<T: Copy>(effective: Effective<T>, label: impl Fn(T) -> String) -> String {
    match effective.source {
        SettingSource::AppDefault => format!("App default ({})", label(effective.value)),
        SettingSource::ThisConnection => label(effective.value),
    }
}

fn override_field<T: Copy + PartialEq>(
    title: &'static str,
    current: Override<T>,
    app_default: T,
    own: &[T],
    label: impl Fn(T) -> String,
    line: impl Fn(T) -> String,
) -> OverrideField<T> {
    let effective = current.resolve(app_default);
    let app = Effective {
        value: app_default,
        source: SettingSource::AppDefault,
    };
    let mut choices = vec![(Override::Inherit, effective_label(app, &label))];
    choices.extend(
        own.iter()
            .map(|value| (Override::Own(*value), label(*value))),
    );
    // A value only a connection profile can hold stays visible and chosen.
    if let Override::Own(value) = current
        && !own.contains(&value)
    {
        choices.push((current, label(value)));
    }
    OverrideField {
        title,
        selected: effective_label(effective, &label),
        line: line(effective.value),
        choices,
    }
}

/// The Command key's title: "⌘ key" on a Mac; a PC keyboard has no
/// Command key, so it names the host it matters for.
#[must_use]
pub const fn command_key_title(platform: DeckPlatform) -> &'static str {
    match platform {
        DeckPlatform::Mac => "⌘ key",
        DeckPlatform::Pc => "Command key on a Mac host",
    }
}

/// The choices a PC Deck offers (PARITY Q-3), stored in the same
/// per-connection key as the Mac's: "Ctrl, as on Windows" is Automatic (a
/// Mac Deck reading it keeps its own default) and "Windows key, keys as
/// labelled" is the Mac's Windows App mapping. Windows and Linux hosts get
/// every PC key as itself whatever is chosen; there is no Deck-wide swap.
pub const PC_SHORTCUT_CHOICES: [ShortcutPreference; 2] = [
    ShortcutPreference::Automatic,
    ShortcutPreference::WindowsApp,
];

/// A Command-key choice's title.
#[must_use]
pub const fn shortcut_label(
    preference: ShortcutPreference,
    platform: DeckPlatform,
) -> &'static str {
    if let DeckPlatform::Pc = platform {
        // A Mac's swap or "⌘ is Control" means Ctrl on a PC.
        return match preference {
            ShortcutPreference::WindowsApp => "Windows key, keys as labelled",
            ShortcutPreference::Automatic
            | ShortcutPreference::SwapCommandControl
            | ShortcutPreference::CommandIsControl => "Ctrl, as on Windows",
        };
    }
    match preference {
        ShortcutPreference::Automatic => "Automatic",
        ShortcutPreference::WindowsApp => "⌘ is the Windows key, Mac edit shortcuts",
        ShortcutPreference::SwapCommandControl => "⌘ and Control swapped",
        ShortcutPreference::CommandIsControl => "⌘ is Control",
    }
}

/// What a Command-key choice does, in one line.
#[must_use]
pub const fn shortcut_description(
    preference: ShortcutPreference,
    platform: DeckPlatform,
) -> &'static str {
    if let DeckPlatform::Pc = platform {
        return match preference {
            ShortcutPreference::WindowsApp => "The key next to Alt is Command.",
            ShortcutPreference::Automatic
            | ShortcutPreference::SwapCommandControl
            | ShortcutPreference::CommandIsControl => "Ctrl+C copies on the Mac.",
        };
    }
    // One sentence each, saying which hosts it applies to: a Mac host keeps
    // ⌘ whatever is chosen. The user guide has the full mapping per host.
    match preference {
        ShortcutPreference::Automatic => {
            "⌘C, ⌘V and the other edit shortcuts work as Ctrl on Windows and Linux hosts."
        }
        ShortcutPreference::WindowsApp => {
            "As Microsoft's Windows App on Windows and Linux hosts: ⌘ alone opens Start."
        }
        ShortcutPreference::SwapCommandControl => {
            "⌘ is Ctrl for every key on Windows and Linux hosts; Control is the Windows key."
        }
        ShortcutPreference::CommandIsControl => {
            "⌘ and Control are both Ctrl on Windows and Linux hosts."
        }
    }
}

/// The ⌘ key field. Automatic is the host platform's own mapping, which is
/// also the default (there is no setting for it in Settings).
#[must_use]
pub fn command_key_field(
    platform: DeckPlatform,
    overrides: &ConnectionOverrides,
) -> OverrideField<ShortcutPreference> {
    let own: &[ShortcutPreference] = match platform {
        DeckPlatform::Mac => &ShortcutPreference::ALL[1..],
        DeckPlatform::Pc => &PC_SHORTCUT_CHOICES[1..],
    };
    override_field(
        command_key_title(platform),
        overrides.keyboard_shortcuts,
        ShortcutPreference::Automatic,
        own,
        |preference| shortcut_label(preference, platform).to_owned(),
        |preference| shortcut_description(preference, platform).to_owned(),
    )
}

/// An Option-key choice's title.
#[must_use]
pub const fn option_key_label(preference: OptionKeyPreference) -> &'static str {
    match preference {
        OptionKeyPreference::Automatic => "Automatic",
        OptionKeyPreference::Chosen(OptionKeyMode::LeftAltRightCharacters) => {
            "Left Option is Alt, right Option types characters"
        }
        OptionKeyPreference::Chosen(OptionKeyMode::BothAlt) => "Both Option keys are Alt",
        OptionKeyPreference::Chosen(OptionKeyMode::BothCharacters) => {
            "Both Option keys type characters"
        }
    }
}

/// What an Option-key choice does, in one line.
#[must_use]
pub const fn option_key_description(preference: OptionKeyPreference) -> &'static str {
    match preference {
        OptionKeyPreference::Automatic => {
            "A Mac host keeps both Option keys; on other hosts left Option is Alt and right \
             Option types this Mac's characters."
        }
        OptionKeyPreference::Chosen(OptionKeyMode::LeftAltRightCharacters) => {
            "Left Option sends Alt shortcuts; right Option types this Mac's characters."
        }
        OptionKeyPreference::Chosen(OptionKeyMode::BothAlt) => {
            "Both Option keys send Alt shortcuts."
        }
        OptionKeyPreference::Chosen(OptionKeyMode::BothCharacters) => {
            "Both Option keys type this Mac's characters."
        }
    }
}

/// The Option key field (a Mac Deck only: a PC keyboard has Alt and
/// `AltGr`, which a connection profile can still set).
#[must_use]
pub fn option_key_field(
    app: AppDefaults,
    overrides: &ConnectionOverrides,
) -> OverrideField<OptionKeyPreference> {
    override_field(
        "Option key",
        overrides.option_keys,
        app.option_keys,
        &[
            OptionKeyPreference::Chosen(OptionKeyMode::LeftAltRightCharacters),
            OptionKeyPreference::Chosen(OptionKeyMode::BothAlt),
            OptionKeyPreference::Chosen(OptionKeyMode::BothCharacters),
        ],
        |preference| option_key_label(preference).to_owned(),
        |preference| option_key_description(preference).to_owned(),
    )
}

/// The Typing field, with the same words as Settings.
#[must_use]
pub fn typing_field(
    app: AppDefaults,
    overrides: &ConnectionOverrides,
) -> OverrideField<TypingMode> {
    let lines = typing_choices();
    override_field(
        TYPING_HEADING,
        overrides.typing_mode,
        app.typing_mode,
        &[TypingMode::Auto, TypingMode::Keys],
        |mode| typing_title(mode).to_owned(),
        |mode| {
            let mode = crate::overrides::stored_typing_mode(mode);
            lines
                .iter()
                .find(|choice| choice.value == mode)
                .map_or_else(String::new, |choice| choice.line.clone())
        },
    )
}

/// The Displays field.
#[must_use]
pub fn displays_field(
    platform: DeckPlatform,
    app: AppDefaults,
    overrides: &ConnectionOverrides,
) -> OverrideField<DisplaysMode> {
    override_field(
        "Displays",
        overrides.displays_mode,
        app.displays_mode,
        &DisplaysMode::ALL,
        |mode| mode.label().to_owned(),
        |mode| displays_line(mode, platform).to_owned(),
    )
}

/// A system-shortcuts choice's title.
#[must_use]
pub const fn on_off_label(on: bool) -> &'static str {
    if on { "On" } else { "Off" }
}

/// What the system shortcuts do when they stay on this computer.
#[must_use]
pub const fn system_shortcuts_off_line(platform: DeckPlatform) -> &'static str {
    match platform {
        DeckPlatform::Mac => "⌘Tab, Mission Control and other shortcuts act on this Mac.",
        DeckPlatform::Pc => "Alt+Tab, the Windows key and other shortcuts act on this PC.",
    }
}

/// The system shortcuts field: Settings › Input's switch, with its words,
/// for this connection, and one line on what the value in force does. The
/// Accessibility permission a Mac needs stays one for the whole app.
#[must_use]
pub fn system_shortcuts_field(
    platform: DeckPlatform,
    app: AppDefaults,
    overrides: &ConnectionOverrides,
) -> OverrideField<bool> {
    override_field(
        SYSTEM_SHORTCUTS_TITLE,
        overrides.system_shortcuts,
        app.system_shortcuts,
        &[true, false],
        |on| on_off_label(on).to_owned(),
        |on| {
            if on {
                system_shortcuts_line(platform)
            } else {
                system_shortcuts_off_line(platform)
            }
            .to_owned()
        },
    )
}

// ── Home ──────────────────────────────────────────────────────────────────

/// The link on Home that hands every one of a connection's own settings
/// back to Settings.
pub const USE_APP_DEFAULTS: &str = "Use app defaults";

/// How a ⌘-key choice reads in Home's summary.
const fn shortcut_phrase(preference: ShortcutPreference, platform: DeckPlatform) -> &'static str {
    if let DeckPlatform::Pc = platform {
        return match preference {
            ShortcutPreference::WindowsApp => "Windows key is Command",
            ShortcutPreference::Automatic
            | ShortcutPreference::SwapCommandControl
            | ShortcutPreference::CommandIsControl => "Ctrl is Command",
        };
    }
    match preference {
        ShortcutPreference::Automatic => "automatic ⌘ key",
        ShortcutPreference::WindowsApp => "⌘ is the Windows key",
        ShortcutPreference::SwapCommandControl => "⌘ and Control swapped",
        ShortcutPreference::CommandIsControl => "⌘ is Control",
    }
}

/// How an Option-key choice reads in Home's summary. A PC keyboard has no
/// Option key; only a connection profile sets its `AltGr` there.
const fn option_phrase(preference: OptionKeyPreference, platform: DeckPlatform) -> &'static str {
    if let DeckPlatform::Pc = platform {
        return "its own AltGr setting";
    }
    match preference {
        OptionKeyPreference::Automatic => "automatic Option keys",
        OptionKeyPreference::Chosen(OptionKeyMode::LeftAltRightCharacters) => "left Option is Alt",
        OptionKeyPreference::Chosen(OptionKeyMode::BothAlt) => "both Option keys are Alt",
        OptionKeyPreference::Chosen(OptionKeyMode::BothCharacters) => {
            "both Option keys type characters"
        }
    }
}

/// How a Typing choice reads in Home's summary.
const fn typing_phrase(mode: TypingMode) -> &'static str {
    match mode {
        TypingMode::Auto | TypingMode::Characters => "character typing",
        TypingMode::Keys => "keys only typing",
    }
}

/// How a Displays choice reads in Home's summary.
const fn displays_phrase(mode: DisplaysMode) -> &'static str {
    match mode {
        DisplaysMode::MatchLayout => "matches your display layout",
        DisplaysMode::SinglePrimary => "primary display only",
        DisplaysMode::Windowed => "opens in a window",
    }
}

/// How the system-shortcuts switch reads in Home's summary.
const fn system_shortcuts_phrase(on: bool, platform: DeckPlatform) -> &'static str {
    match (on, platform) {
        (true, _) => "system shortcuts go to the remote computer",
        (false, DeckPlatform::Mac) => "system shortcuts stay on this Mac",
        (false, DeckPlatform::Pc) => "system shortcuts stay on this PC",
    }
}

/// Home's one quiet line under Connect: every setting this connection keeps
/// its own value for that differs from the app default ("This connection:
/// Keys only typing, left Option is Alt."), in the form's order. `None`
/// while there is none.
#[must_use]
pub fn own_settings_summary(
    platform: DeckPlatform,
    app: AppDefaults,
    overrides: &ConnectionOverrides,
) -> Option<String> {
    fn differs<T: Copy>(
        current: Override<T>,
        app: T,
        phrase: impl Fn(T) -> &'static str,
    ) -> Option<&'static str> {
        let own = current.own()?;
        (phrase(own) != phrase(app)).then(|| phrase(own))
    }
    let mut phrases: Vec<&'static str> = [
        differs(
            overrides.keyboard_shortcuts,
            ShortcutPreference::Automatic,
            |preference| shortcut_phrase(preference, platform),
        ),
        differs(overrides.option_keys, app.option_keys, |preference| {
            option_phrase(preference, platform)
        }),
        differs(overrides.typing_mode, app.typing_mode, typing_phrase),
        differs(overrides.displays_mode, app.displays_mode, displays_phrase),
        differs(overrides.system_shortcuts, app.system_shortcuts, |on| {
            system_shortcuts_phrase(on, platform)
        }),
    ]
    .into_iter()
    .flatten()
    .collect();
    let clipboard = overrides.effective_clipboard_enabled(app.clipboard_enabled);
    if clipboard.source == SettingSource::ThisConnection && clipboard.value != app.clipboard_enabled
    {
        phrases.push("clipboard off");
    }
    let list = phrases.join(", ");
    let mut letters = list.chars();
    let first = letters.next()?;
    Some(format!(
        "This connection: {}{}.",
        first.to_uppercase(),
        letters.as_str()
    ))
}

// ── Settings › Microphone ─────────────────────────────────────────────────

/// What the microphone switch does, in plain words.
#[must_use]
pub fn microphone_line(platform: DeckPlatform) -> String {
    let os = match platform {
        DeckPlatform::Mac => "macOS",
        DeckPlatform::Pc => "Windows",
    };
    format!(
        "Off each time Arcen Deck starts. When it is on, the remote computer gets your \
         microphone only if the host allows it and {os} lets Arcen Deck use the microphone. \
         Turning it off stops the microphone; reconnect to use it again."
    )
}

// ── Settings › Advanced › Certificate checking ────────────────────────────

/// The group's heading.
pub const CERTIFICATE_HEADING: &str = "Certificate checking";

/// The line above the choices.
pub const CERTIFICATE_INTRO: &str =
    "How strictly the Deck checks a host's certificate before it connects.";

/// What Settings says when the saved level is Low but this Deck may not
/// skip certificate checks ([`crate::settings::low_security_allowed`]).
pub const LOW_SECURITY_OFF_NOTE: &str =
    "Low is saved, but this Deck checks as Medium: Low needs ARCEN_ACCEPT_INSECURE=1.";

/// The levels, one line each (the default is Medium). Low only where this
/// Deck may use it (`low_allowed`): elsewhere it could only fail every
/// connection, and nothing in the UI turns it on.
#[must_use]
pub fn certificate_choices(low_allowed: bool) -> Vec<Choice<crate::settings::SecurityMode>> {
    use crate::settings::SecurityMode;
    let mut choices = vec![
        Choice::new(
            SecurityMode::High,
            "High",
            "Connects only to a host with a certificate from an authority this computer trusts.",
        ),
        Choice {
            badge: Some("(Default)"),
            ..Choice::new(
                SecurityMode::Medium,
                "Medium",
                "Asks you to verify a host whose certificate no trusted authority issued.",
            )
        },
        Choice::new(
            SecurityMode::Low,
            "Low",
            "Connects without checking the host's certificate; avoid it.",
        ),
    ];
    if !low_allowed {
        choices.retain(|choice| choice.value != SecurityMode::Low);
    }
    choices
}

// ── Settings › Displays ───────────────────────────────────────────────────

/// The line above the Displays choices.
pub const DISPLAYS_INTRO: &str =
    "How many of your displays the remote computer gets, and how they are shown.";

/// The user guide's displays section.
pub const DISPLAYS_GUIDE_SECTION: &str = "9-several-displays-and-retina";

/// What a Displays choice does, in one line, in this Deck's own terms (a
/// Mac's Match my layout needs its separate Spaces; the rest is in the
/// user guide).
#[must_use]
pub const fn displays_line(mode: DisplaysMode, platform: DeckPlatform) -> &'static str {
    match (mode, platform) {
        (DisplaysMode::MatchLayout, DeckPlatform::Mac) => {
            "Each of your displays, up to four, fullscreen; needs separate Spaces."
        }
        (DisplaysMode::MatchLayout, DeckPlatform::Pc) => {
            "Each of your displays, up to four, fullscreen and pixel for pixel."
        }
        (DisplaysMode::SinglePrimary, _) => {
            "Your main display only, fullscreen and pixel for pixel."
        }
        (DisplaysMode::Windowed, _) => {
            "Your main display in a window you can resize; the picture fits it."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_inherited_value_reads_as_the_app_default_it_is() {
        let app = AppDefaults {
            typing_mode: TypingMode::Keys,
            ..AppDefaults::default()
        };
        let inherit = ConnectionOverrides::default();
        let field = typing_field(app, &inherit);
        assert_eq!(field.selected, "App default (Keys only)");
        assert_eq!(
            field.choices[0],
            (Override::Inherit, field.selected.clone())
        );
        assert_eq!(
            field.choices[1..]
                .iter()
                .map(|(_, label)| label.as_str())
                .collect::<Vec<_>>(),
            ["Characters", "Keys only"]
        );
        let own = ConnectionOverrides {
            typing_mode: Override::Own(TypingMode::Auto),
            ..ConnectionOverrides::default()
        };
        assert_eq!(typing_field(app, &own).selected, "Characters");
        assert_eq!(
            typing_field(AppDefaults::default(), &inherit).selected,
            "App default (Characters)"
        );
        assert_eq!(
            displays_field(DeckPlatform::Mac, AppDefaults::default(), &inherit).selected,
            format!(
                "App default ({})",
                AppDefaults::default().displays_mode.label()
            )
        );
        assert_eq!(
            command_key_field(DeckPlatform::Mac, &inherit).selected,
            "App default (Automatic)"
        );
        assert_eq!(
            option_key_field(AppDefaults::default(), &inherit).selected,
            "App default (Automatic)"
        );
    }

    #[test]
    fn a_value_only_a_profile_holds_stays_visible() {
        // Own(Automatic) is not offered (it is the default) but a profile
        // file may hold it.
        let own = ConnectionOverrides {
            keyboard_shortcuts: Override::Own(ShortcutPreference::Automatic),
            ..ConnectionOverrides::default()
        };
        let field = command_key_field(DeckPlatform::Mac, &own);
        assert_eq!(field.selected, "Automatic");
        assert!(field.choices.contains(&(
            Override::Own(ShortcutPreference::Automatic),
            "Automatic".to_owned()
        )));
    }

    #[test]
    fn a_pc_offers_the_command_key_on_a_mac_host_and_nothing_else() {
        use arcen_input::keyboard::PcKeyboardSettings;
        let field = command_key_field(DeckPlatform::Pc, &ConnectionOverrides::default());
        assert_eq!(
            field
                .choices
                .iter()
                .map(|(_, label)| label.as_str())
                .collect::<Vec<_>>(),
            [
                "App default (Ctrl, as on Windows)",
                "Windows key, keys as labelled"
            ]
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
            assert_eq!(
                shortcut_label(choice, DeckPlatform::Pc),
                "Ctrl, as on Windows"
            );
        }
    }

    #[test]
    fn the_count_of_connections_that_keep_their_own_reads_naturally() {
        assert_eq!(
            default_for_all_connections(0),
            "Default for all connections"
        );
        assert_eq!(
            default_for_all_connections(1),
            "Default for all connections. 1 connection uses its own."
        );
        assert_eq!(
            default_for_all_connections(3),
            "Default for all connections. 3 connections use their own."
        );
        assert_eq!(clipboard_line(0), None);
        assert_eq!(
            clipboard_line(2).as_deref(),
            Some("2 connections use their own")
        );
    }

    #[test]
    fn the_intro_names_the_keyboard_switch_each_deck_spells() {
        let switch = KeyboardSwitchChord::DEFAULT;
        let mac = input_intro(DeckPlatform::Mac, switch);
        assert_eq!(
            mac[1],
            format!(
                "Press {} to move the keyboard between this Mac and the remote computer.",
                switch.mac_text()
            )
        );
        let pc = input_intro(DeckPlatform::Pc, switch);
        assert!(pc[1].starts_with(&format!("Press {} ", switch.pc_text())));
        assert!(pc[1].contains("this PC"));
    }

    #[test]
    fn the_permission_line_shows_only_when_it_is_needed() {
        assert_eq!(
            system_shortcuts_permission(true, Some(false)),
            Some("Needs Accessibility permission.")
        );
        assert_eq!(system_shortcuts_permission(false, Some(false)), None);
        assert_eq!(system_shortcuts_permission(true, Some(true)), None);
        assert_eq!(system_shortcuts_permission(true, None), None);
    }

    #[test]
    fn learn_more_leads_to_the_user_guide_in_the_public_source() {
        assert_eq!(
            user_guide_url(KEYBOARD_GUIDE_SECTION),
            format!(
                "{}/blob/main/docs/user-guide.md#6-keyboard",
                crate::about::SOURCE_URL
            )
        );
        let guide = include_str!("../../../../docs/user-guide.md");
        assert!(guide.contains("\n## 6. Keyboard\n"));
        assert!(guide.contains("\n## 5. Pen and tablet\n"));
    }

    #[test]
    fn each_deck_explains_displays_in_its_own_terms_one_line_each() {
        for mode in DisplaysMode::ALL {
            let pc = displays_line(mode, DeckPlatform::Pc);
            for mac_term in ["Spaces", "Notch", "notch", "Retina", "macOS"] {
                assert!(!pc.contains(mac_term), "{pc}");
            }
            for platform in [DeckPlatform::Mac, DeckPlatform::Pc] {
                let line = displays_line(mode, platform);
                assert!(line.chars().count() <= 80 && !line.contains('\n'), "{line}");
                assert_eq!(line.matches(". ").count(), 0, "one sentence: {line}");
            }
        }
        assert!(displays_line(DisplaysMode::MatchLayout, DeckPlatform::Mac).contains("Spaces"));
        let field = displays_field(
            DeckPlatform::Pc,
            AppDefaults::default(),
            &ConnectionOverrides::default(),
        );
        assert_eq!(
            field.line,
            displays_line(AppDefaults::default().displays_mode, DeckPlatform::Pc)
        );
    }

    #[test]
    fn the_command_key_says_what_it_does_in_one_sentence() {
        for platform in [DeckPlatform::Mac, DeckPlatform::Pc] {
            for preference in ShortcutPreference::ALL {
                let line = shortcut_description(preference, platform);
                assert!(line.chars().count() <= 100, "{line}");
                assert!(!line.contains('/') && !line.contains(". "), "{line}");
            }
        }
        // A Mac host keeps ⌘ whatever is chosen (the keyboard resolver), so
        // every Mac line names the hosts it changes.
        for preference in ShortcutPreference::ALL {
            let line = shortcut_description(preference, DeckPlatform::Mac);
            assert!(line.contains("Windows and Linux hosts"), "{line}");
        }
    }

    #[test]
    fn microphone_and_certificate_wording_is_plain() {
        for platform in [DeckPlatform::Mac, DeckPlatform::Pc] {
            let line = microphone_line(platform);
            for jargon in ["microphone-v1", "negotiated", "capabilit", "capture"] {
                assert!(!line.contains(jargon), "{line}");
            }
        }
        assert!(microphone_line(DeckPlatform::Pc).contains("Windows lets Arcen Deck"));
        assert!(microphone_line(DeckPlatform::Mac).contains("macOS lets Arcen Deck"));
        assert_eq!(certificate_choices(false).len(), 2);
        let choices = certificate_choices(true);
        assert_eq!(
            choices
                .iter()
                .map(|choice| choice.title.as_str())
                .collect::<Vec<_>>(),
            ["High", "Medium", "Low"]
        );
        for choice in &choices {
            for text in [choice.title.as_str(), choice.line.as_str()] {
                assert!(
                    !text.contains(" - ") && !text.contains("client") && !text.contains("server"),
                    "{text}"
                );
            }
            assert!(choice.line.chars().count() <= 100 && !choice.line.contains(". "));
        }
        assert_eq!(choices[1].badge, Some("(Default)"));
        assert_eq!(
            crate::settings::ClientSettings::default().security_mode,
            choices[1].value
        );
    }

    #[test]
    fn the_system_shortcuts_field_follows_this_decks_switch_until_set() {
        let inherit = ConnectionOverrides::default();
        for (platform, app, default) in [
            (DeckPlatform::Mac, false, "App default (Off)"),
            (DeckPlatform::Pc, true, "App default (On)"),
        ] {
            let app = AppDefaults {
                system_shortcuts: app,
                ..AppDefaults::default()
            };
            let field = system_shortcuts_field(platform, app, &inherit);
            assert_eq!(field.title, SYSTEM_SHORTCUTS_TITLE);
            assert_eq!(
                field.line,
                if app.system_shortcuts {
                    system_shortcuts_line(platform)
                } else {
                    system_shortcuts_off_line(platform)
                }
            );
            assert_eq!(field.selected, default);
            assert_eq!(
                field
                    .choices
                    .iter()
                    .map(|(value, label)| (*value, label.as_str()))
                    .collect::<Vec<_>>(),
                [
                    (Override::Inherit, default),
                    (Override::Own(true), "On"),
                    (Override::Own(false), "Off"),
                ]
            );
            let off = ConnectionOverrides {
                system_shortcuts: Override::Own(false),
                ..ConnectionOverrides::default()
            };
            let field = system_shortcuts_field(platform, app, &off);
            assert_eq!(field.selected, "Off");
            assert_eq!(field.line, system_shortcuts_off_line(platform));
        }
    }

    #[test]
    fn home_sums_up_only_the_own_settings_that_differ_from_the_app_default() {
        let app = AppDefaults::default();
        assert_eq!(
            own_settings_summary(DeckPlatform::Mac, app, &ConnectionOverrides::default()),
            None
        );
        // Own values equal to the defaults say nothing.
        let same = ConnectionOverrides {
            typing_mode: Override::Own(app.typing_mode),
            displays_mode: Override::Own(app.displays_mode),
            keyboard_shortcuts: Override::Own(ShortcutPreference::Automatic),
            system_shortcuts: Override::Own(app.system_shortcuts),
            clipboard_enabled: Override::Own(true),
            ..ConnectionOverrides::default()
        };
        assert_eq!(own_settings_summary(DeckPlatform::Mac, app, &same), None);
        let own = ConnectionOverrides {
            typing_mode: Override::Own(TypingMode::Keys),
            option_keys: Override::Own(OptionKeyPreference::Chosen(
                OptionKeyMode::LeftAltRightCharacters,
            )),
            ..ConnectionOverrides::default()
        };
        assert_eq!(
            own_settings_summary(DeckPlatform::Mac, app, &own).as_deref(),
            Some("This connection: Left Option is Alt, keys only typing.")
        );
        let every = ConnectionOverrides {
            keyboard_shortcuts: Override::Own(ShortcutPreference::WindowsApp),
            displays_mode: Override::Own(DisplaysMode::Windowed),
            system_shortcuts: Override::Own(!app.system_shortcuts),
            clipboard_enabled: Override::Own(false),
            ..own
        };
        assert_eq!(
            own_settings_summary(DeckPlatform::Mac, app, &every).as_deref(),
            Some(
                "This connection: ⌘ is the Windows key, left Option is Alt, keys only typing, \
                 opens in a window, system shortcuts go to the remote computer, clipboard off."
            )
        );
        let pc = AppDefaults {
            system_shortcuts: true,
            ..app
        };
        let off = ConnectionOverrides {
            system_shortcuts: Override::Own(false),
            ..ConnectionOverrides::default()
        };
        assert_eq!(
            own_settings_summary(DeckPlatform::Pc, pc, &off).as_deref(),
            Some("This connection: System shortcuts stay on this PC.")
        );
        // A clipboard a connection "allows" while Settings shares none is
        // not in force, so it says nothing.
        let allows = ConnectionOverrides {
            clipboard_enabled: Override::Own(true),
            ..ConnectionOverrides::default()
        };
        let not_sharing = AppDefaults {
            clipboard_enabled: false,
            ..app
        };
        assert_eq!(
            own_settings_summary(DeckPlatform::Mac, not_sharing, &allows),
            None
        );
        // Plain words: no middle dots, no setting keys.
        for platform in [DeckPlatform::Mac, DeckPlatform::Pc] {
            let text = own_settings_summary(platform, app, &every).unwrap();
            assert!(!text.contains('\u{b7}') && !text.contains('_'), "{text}");
        }
    }
}
