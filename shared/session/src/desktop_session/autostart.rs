//! XDG autostart for an Arcen-assembled GNOME session.
//!
//! Implements the [Desktop Application Autostart Specification] over the
//! [Desktop Entry Specification], plus the GNOME extensions `gnome-session`
//! honours: `X-GNOME-Autostart-enabled`, `X-GNOME-Autostart-Phase`,
//! `X-GNOME-Autostart-Delay`, `X-GNOME-Autostart-Notify`,
//! `X-GNOME-AutoRestart` and `AutostartCondition` (also spelled
//! `X-GNOME-Autostart-condition`).
//!
//! The caller reads the autostart directories and passes their files in,
//! highest-priority directory first; this module never touches the file
//! system. Commands come out as an argument vector, never as shell text.
//!
//! `X-GNOME-HiddenUnderSystemd` and `X-systemd-skip` are recorded and
//! ignored: they hide an entry because systemd starts it instead, and an
//! Arcen session is not run by the user's systemd manager (which is exactly
//! what `gnome-session`'s own non-systemd mode did).
//!
//! [Desktop Application Autostart Specification]: https://specifications.freedesktop.org/autostart-spec/latest/
//! [Desktop Entry Specification]: https://specifications.freedesktop.org/desktop-entry-spec/latest/

use std::collections::BTreeSet;
use std::fmt::{Display, Formatter};

/// Largest autostart file read.
pub const MAX_ENTRY_BYTES: usize = 64 * 1024;
/// Most autostart files considered across all directories.
pub const MAX_ENTRIES: usize = 256;
/// Longest desktop-file id accepted.
pub const MAX_ID_BYTES: usize = 255;
/// Most arguments in one command.
pub const MAX_ARGUMENTS: usize = 64;
/// Longest `X-GNOME-Autostart-Delay` honoured, in seconds.
pub const MAX_DELAY_SECS: u32 = 300;

/// When in the session an entry starts (`X-GNOME-Autostart-Phase`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Phase {
    /// Before anything else.
    EarlyInitialization,
    /// Before the display server (the keyring).
    PreDisplayServer,
    /// The display server itself.
    DisplayServer,
    /// Session infrastructure (settings daemons, accessibility).
    Initialization,
    /// The window manager.
    WindowManager,
    /// The panel.
    Panel,
    /// The desktop.
    Desktop,
    /// Ordinary applications (the default).
    Application,
}

impl Phase {
    fn parse(value: &str) -> Self {
        match value.trim() {
            "EarlyInitialization" => Self::EarlyInitialization,
            "PreDisplayServer" => Self::PreDisplayServer,
            "DisplayServer" => Self::DisplayServer,
            "Initialization" => Self::Initialization,
            "WindowManager" => Self::WindowManager,
            "Panel" => Self::Panel,
            "Desktop" => Self::Desktop,
            _ => Self::Application,
        }
    }

    /// The Arcen start stage of this phase: session infrastructure starts
    /// once the compositor runs, everything from the window-manager phase on
    /// once the desktop is ready.
    #[must_use]
    pub const fn stage(self) -> Stage {
        match self {
            Self::EarlyInitialization
            | Self::PreDisplayServer
            | Self::DisplayServer
            | Self::Initialization => Stage::Initialization,
            Self::WindowManager | Self::Panel | Self::Desktop | Self::Application => {
                Stage::Application
            }
        }
    }

    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::EarlyInitialization => "EarlyInitialization",
            Self::PreDisplayServer => "PreDisplayServer",
            Self::DisplayServer => "DisplayServer",
            Self::Initialization => "Initialization",
            Self::WindowManager => "WindowManager",
            Self::Panel => "Panel",
            Self::Desktop => "Desktop",
            Self::Application => "Application",
        }
    }
}

/// When an Arcen session starts an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Stage {
    /// As soon as the compositor's Wayland socket exists.
    Initialization,
    /// Once the desktop announced itself ready.
    Application,
}

/// An `AutostartCondition`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    /// `if-exists PATH`: start only if `$XDG_CONFIG_HOME/PATH` exists.
    IfExists(String),
    /// `unless-exists PATH`: start only if it does not.
    UnlessExists(String),
    /// `GSettings SCHEMA KEY`: start only if that boolean key is true.
    GSettings {
        /// Schema id.
        schema: String,
        /// Key name.
        key: String,
    },
    /// `GNOME3 if-session NAME`: start only in that session.
    IfSession(String),
    /// `GNOME3 unless-session NAME`: start except in that session.
    UnlessSession(String),
    /// A condition this implementation does not understand: the entry is
    /// skipped, as `gnome-session` skips it.
    Unknown(String),
}

impl Condition {
    /// Parses a condition value.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        let words = value.split_whitespace().collect::<Vec<_>>();
        match words.as_slice() {
            ["if-exists", path] if valid_relative_path(path) => Self::IfExists((*path).to_owned()),
            ["unless-exists", path] if valid_relative_path(path) => {
                Self::UnlessExists((*path).to_owned())
            }
            ["GSettings" | "GNOME3" | "GNOME", schema, key]
                if *schema != "if-session"
                    && *schema != "unless-session"
                    && valid_settings_word(schema, true)
                    && valid_settings_word(key, false) =>
            {
                Self::GSettings {
                    schema: (*schema).to_owned(),
                    key: (*key).to_owned(),
                }
            }
            ["GNOME3", "if-session", name] if valid_settings_word(name, false) => {
                Self::IfSession((*name).to_owned())
            }
            ["GNOME3", "unless-session", name] if valid_settings_word(name, false) => {
                Self::UnlessSession((*name).to_owned())
            }
            _ => Self::Unknown(value.chars().take(128).collect()),
        }
    }
}

/// What the caller knows about the session for evaluating conditions and
/// `TryExec`. Implemented by the platform adapter.
pub trait EntryFacts {
    /// Whether `$XDG_CONFIG_HOME/<relative>` exists. `relative` is already
    /// validated: relative, no `..`, no NUL.
    fn config_path_exists(&self, relative: &str) -> bool;
    /// The boolean value of `schema key`, or `None` when it cannot be read.
    fn settings_boolean(&self, schema: &str, key: &str) -> Option<bool>;
    /// Whether `program` (absolute, or found on `PATH`) is executable.
    fn program_exists(&self, program: &str) -> bool;
    /// The session name (`gnome`).
    fn session_name(&self) -> &str;
}

/// One parsed autostart entry.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // One field per Desktop Entry boolean key.
pub struct AutostartEntry {
    /// Desktop-file id: the file name without `.desktop`.
    pub id: String,
    /// The command as an argument vector.
    pub argv: Vec<String>,
    /// `TryExec`, when present.
    pub try_exec: Option<String>,
    /// `Hidden=true`: the entry is deleted.
    pub hidden: bool,
    /// `X-GNOME-Autostart-enabled=false` disables it.
    pub enabled: bool,
    /// `OnlyShowIn`.
    pub only_show_in: Option<Vec<String>>,
    /// `NotShowIn`.
    pub not_show_in: Option<Vec<String>>,
    /// The condition, when present.
    pub condition: Option<Condition>,
    /// The phase.
    pub phase: Phase,
    /// `X-GNOME-AutoRestart`.
    pub auto_restart: bool,
    /// `X-GNOME-Autostart-Delay`, bounded.
    pub delay_secs: u32,
    /// `X-GNOME-Autostart-Notify`: the program registers with the session
    /// manager and needs `DESKTOP_AUTOSTART_ID`.
    pub notify: bool,
    /// `X-GNOME-HiddenUnderSystemd` (recorded, ignored).
    pub hidden_under_systemd: bool,
}

/// Why an autostart file was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryError {
    /// The file name is not a valid desktop-file id.
    InvalidId,
    /// The file is larger than [`MAX_ENTRY_BYTES`].
    TooLarge,
    /// The file is not UTF-8.
    NotUtf8,
    /// No `[Desktop Entry]` group.
    NoDesktopEntryGroup,
    /// `Type` is not `Application`.
    NotAnApplication,
    /// `Exec` is missing, empty or malformed.
    InvalidExec,
}

impl Display for EntryError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidId => "invalid desktop-file id",
            Self::TooLarge => "file too large",
            Self::NotUtf8 => "not UTF-8",
            Self::NoDesktopEntryGroup => "no [Desktop Entry] group",
            Self::NotAnApplication => "Type is not Application",
            Self::InvalidExec => "Exec missing or malformed",
        })
    }
}

impl std::error::Error for EntryError {}

/// The desktop-file id of `file_name`, when it is a `.desktop` file with a
/// plausible id.
#[must_use]
pub fn desktop_file_id(file_name: &str) -> Option<&str> {
    let id = file_name.strip_suffix(".desktop")?;
    (!id.is_empty()
        && file_name.len() <= MAX_ID_BYTES
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+')))
    .then_some(id)
}

/// Parses one autostart file.
///
/// # Errors
///
/// [`EntryError`] when the file cannot describe a startable application.
/// A `Hidden=true` file parses even without `Exec`: it deletes the entry of
/// the same id in lower-priority directories.
pub fn parse_entry(file_name: &str, bytes: &[u8]) -> Result<AutostartEntry, EntryError> {
    let id = desktop_file_id(file_name).ok_or(EntryError::InvalidId)?;
    if bytes.len() > MAX_ENTRY_BYTES {
        return Err(EntryError::TooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| EntryError::NotUtf8)?;
    let fields = desktop_entry_fields(text).ok_or(EntryError::NoDesktopEntryGroup)?;
    let get = |key: &str| {
        fields
            .iter()
            .rev()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.as_str())
    };
    let boolean =
        |key: &str, default: bool| get(key).map_or(default, |value| value.trim() == "true");
    let hidden = boolean("Hidden", false);
    let mut entry = AutostartEntry {
        id: id.to_owned(),
        argv: Vec::new(),
        try_exec: get("TryExec")
            .map(unescape_string)
            .filter(|value| !value.is_empty()),
        hidden,
        enabled: boolean("X-GNOME-Autostart-enabled", true),
        only_show_in: get("OnlyShowIn").map(parse_list),
        not_show_in: get("NotShowIn").map(parse_list),
        condition: get("AutostartCondition")
            .or_else(|| get("X-GNOME-Autostart-condition"))
            .map(|value| Condition::parse(&unescape_string(value))),
        phase: get("X-GNOME-Autostart-Phase").map_or(Phase::Application, Phase::parse),
        auto_restart: boolean("X-GNOME-AutoRestart", false),
        delay_secs: get("X-GNOME-Autostart-Delay")
            .and_then(|value| value.trim().parse::<u32>().ok())
            .map_or(0, |delay| delay.min(MAX_DELAY_SECS)),
        notify: boolean("X-GNOME-Autostart-Notify", false),
        hidden_under_systemd: boolean("X-GNOME-HiddenUnderSystemd", false),
    };
    if hidden {
        return Ok(entry);
    }
    if get("Type").map(str::trim) != Some("Application") {
        return Err(EntryError::NotAnApplication);
    }
    let exec = get("Exec").ok_or(EntryError::InvalidExec)?;
    entry.argv = parse_exec(&unescape_string(exec)).ok_or(EntryError::InvalidExec)?;
    Ok(entry)
}

/// Merges entries by id across directories given highest priority first:
/// the first directory that has an id decides it, a `Hidden` entry
/// included (it deletes the id). Returns the surviving ids' entries in id
/// order, the deleted ids, and the files that failed to parse.
#[must_use]
pub fn merge(directories: &[Vec<(String, Vec<u8>)>]) -> Merged {
    let mut seen = BTreeSet::new();
    let mut merged = Merged::default();
    let mut considered = 0_usize;
    for directory in directories {
        for (file_name, bytes) in directory {
            let Some(id) = desktop_file_id(file_name) else {
                continue;
            };
            if seen.contains(id) {
                continue;
            }
            considered += 1;
            if considered > MAX_ENTRIES {
                merged.truncated = true;
                break;
            }
            seen.insert(id.to_owned());
            match parse_entry(file_name, bytes) {
                Ok(entry) if entry.hidden => merged.deleted.push(entry.id),
                Ok(entry) => merged.entries.push(entry),
                Err(error) => merged.invalid.push((id.to_owned(), error)),
            }
        }
    }
    merged.entries.sort_by(|left, right| left.id.cmp(&right.id));
    merged
}

/// The result of [`merge`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Merged {
    /// Entries that survive, by id.
    pub entries: Vec<AutostartEntry>,
    /// Ids a higher-priority `Hidden=true` file deleted.
    pub deleted: Vec<String>,
    /// Files that could not be parsed.
    pub invalid: Vec<(String, EntryError)>,
    /// More than [`MAX_ENTRIES`] files were offered; the rest were ignored.
    pub truncated: bool,
}

/// Why an entry does not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// `X-GNOME-Autostart-enabled=false`.
    Disabled,
    /// `OnlyShowIn`/`NotShowIn` excludes the current desktop.
    NotForThisDesktop,
    /// `TryExec` or the command's program is not installed.
    NotInstalled,
    /// The condition is false.
    ConditionFalse,
    /// The condition could not be evaluated (unknown form, or the setting
    /// could not be read).
    ConditionUnknown,
}

impl SkipReason {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::NotForThisDesktop => "not_for_this_desktop",
            Self::NotInstalled => "not_installed",
            Self::ConditionFalse => "condition_false",
            Self::ConditionUnknown => "condition_unknown",
        }
    }
}

/// Whether `entry` starts in a session whose `XDG_CURRENT_DESKTOP` is
/// `current_desktops` (colon-separated), given `facts`.
///
/// # Errors
///
/// The [`SkipReason`] when it does not.
pub fn should_start(
    entry: &AutostartEntry,
    current_desktops: &str,
    facts: &dyn EntryFacts,
) -> Result<(), SkipReason> {
    if !entry.enabled {
        return Err(SkipReason::Disabled);
    }
    let desktops = current_desktops
        .split(':')
        .filter(|desktop| !desktop.is_empty())
        .collect::<Vec<_>>();
    let listed = |list: &Option<Vec<String>>| {
        list.as_ref()
            .map(|list| list.iter().any(|name| desktops.contains(&name.as_str())))
    };
    if listed(&entry.only_show_in) == Some(false) || listed(&entry.not_show_in) == Some(true) {
        return Err(SkipReason::NotForThisDesktop);
    }
    if entry
        .try_exec
        .as_deref()
        .is_some_and(|program| !facts.program_exists(program))
    {
        return Err(SkipReason::NotInstalled);
    }
    if entry
        .argv
        .first()
        .is_none_or(|program| !facts.program_exists(program))
    {
        return Err(SkipReason::NotInstalled);
    }
    match &entry.condition {
        None => Ok(()),
        Some(Condition::IfExists(path)) => facts
            .config_path_exists(path)
            .then_some(())
            .ok_or(SkipReason::ConditionFalse),
        Some(Condition::UnlessExists(path)) => (!facts.config_path_exists(path))
            .then_some(())
            .ok_or(SkipReason::ConditionFalse),
        Some(Condition::GSettings { schema, key }) => match facts.settings_boolean(schema, key) {
            Some(true) => Ok(()),
            Some(false) => Err(SkipReason::ConditionFalse),
            None => Err(SkipReason::ConditionUnknown),
        },
        Some(Condition::IfSession(name)) => (facts.session_name() == name)
            .then_some(())
            .ok_or(SkipReason::ConditionFalse),
        Some(Condition::UnlessSession(name)) => (facts.session_name() != name)
            .then_some(())
            .ok_or(SkipReason::ConditionFalse),
        Some(Condition::Unknown(_)) => Err(SkipReason::ConditionUnknown),
    }
}

/// The `[Desktop Entry]` group's unlocalised `key=value` pairs, raw.
fn desktop_entry_fields(text: &str) -> Option<Vec<(&str, String)>> {
    let mut in_group = false;
    let mut found = false;
    let mut fields = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_group = line.trim_end() == "[Desktop Entry]";
            found |= in_group;
            continue;
        }
        if !in_group {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim_end();
        if key.contains('[') {
            continue;
        }
        fields.push((key, value.trim_start().to_owned()));
    }
    found.then_some(fields)
}

/// Unescapes a Desktop Entry `string` value (`\s \n \t \r \\`).
fn unescape_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(other) if other != '\\' => {
                out.push('\\');
                out.push(other);
            }
            Some(_) | None => out.push('\\'),
        }
    }
    out
}

/// A `;`-separated list (`\;` escapes a separator).
fn parse_list(value: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut chars = value.trim().chars();
    while let Some(character) = chars.next() {
        match character {
            '\\' => match chars.next() {
                Some(';') => current.push(';'),
                Some(other) => {
                    current.push('\\');
                    current.push(other);
                }
                None => {}
            },
            ';' => {
                if !current.is_empty() {
                    items.push(std::mem::take(&mut current));
                }
            }
            other => current.push(other),
        }
    }
    if !current.is_empty() {
        items.push(current);
    }
    items
}

/// Splits an `Exec` value into arguments (Desktop Entry quoting), dropping
/// field codes. `None` when it is empty, unterminated or too long.
fn parse_exec(exec: &str) -> Option<Vec<String>> {
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quoted = false;
    let mut chars = exec.chars().peekable();
    while let Some(character) = chars.next() {
        if quoted {
            match character {
                '"' => quoted = false,
                '\\' => match chars.next() {
                    Some(escaped @ ('"' | '`' | '$' | '\\')) => current.push(escaped),
                    Some(other) => {
                        current.push('\\');
                        current.push(other);
                    }
                    None => return None,
                },
                other => current.push(other),
            }
            continue;
        }
        match character {
            '"' => {
                quoted = true;
                started = true;
            }
            ' ' | '\t' => {
                if started {
                    arguments.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            // Field codes expand to nothing for an autostart launch.
            '%' => {
                if chars.next() == Some('%') {
                    current.push('%');
                    started = true;
                }
            }
            other => {
                current.push(other);
                started = true;
            }
        }
    }
    if quoted {
        return None;
    }
    if started {
        arguments.push(current);
    }
    (!arguments.is_empty()
        && arguments.len() <= MAX_ARGUMENTS
        && !arguments[0].is_empty()
        && arguments.iter().all(|argument| !argument.contains('\0')))
    .then_some(arguments)
}

fn valid_relative_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 255
        && !path.starts_with('/')
        && !path.contains('\0')
        && path.split('/').all(|part| part != "..")
}

fn valid_settings_word(word: &str, dotted: bool) -> bool {
    !word.is_empty()
        && word.len() <= 128
        && word.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || (dotted && byte == b'.')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Facts {
        files: Vec<&'static str>,
        settings: Vec<(&'static str, &'static str, bool)>,
        programs: Vec<&'static str>,
    }

    impl EntryFacts for Facts {
        fn config_path_exists(&self, relative: &str) -> bool {
            self.files.contains(&relative)
        }
        fn settings_boolean(&self, schema: &str, key: &str) -> Option<bool> {
            self.settings
                .iter()
                .find(|(s, k, _)| *s == schema && *k == key)
                .map(|(_, _, value)| *value)
        }
        fn program_exists(&self, program: &str) -> bool {
            self.programs.contains(&program)
        }
        fn session_name(&self) -> &'static str {
            "gnome"
        }
    }

    fn facts() -> Facts {
        Facts {
            files: vec![],
            settings: vec![],
            programs: vec![
                "/usr/libexec/gsd-power",
                "orca",
                "/usr/libexec/gnome-initial-setup",
                "true",
            ],
        }
    }

    const POWER: &str = "[Desktop Entry]\nType=Application\nName=GNOME power management\n\
Name[de]=Energie\nExec=/usr/libexec/gsd-power\nOnlyShowIn=GNOME;\nNoDisplay=true\n\
X-GNOME-Autostart-Phase=Initialization\nX-GNOME-Autostart-Notify=true\n\
X-GNOME-AutoRestart=true\nX-GNOME-HiddenUnderSystemd=true\n";

    #[test]
    fn a_settings_daemon_entry_parses_with_its_gnome_extensions() {
        let entry = parse_entry("org.gnome.SettingsDaemon.Power.desktop", POWER.as_bytes())
            .expect("parses");
        assert_eq!(entry.id, "org.gnome.SettingsDaemon.Power");
        assert_eq!(entry.argv, ["/usr/libexec/gsd-power"]);
        assert_eq!(entry.phase, Phase::Initialization);
        assert_eq!(entry.phase.stage(), Stage::Initialization);
        assert!(entry.auto_restart && entry.notify && entry.hidden_under_systemd);
        assert_eq!(entry.only_show_in, Some(vec!["GNOME".to_owned()]));
        assert_eq!(should_start(&entry, "GNOME", &facts()), Ok(()));
        assert_eq!(
            should_start(&entry, "KDE", &facts()),
            Err(SkipReason::NotForThisDesktop)
        );
        assert_eq!(
            should_start(&entry, "GNOME-Classic:GNOME", &facts()),
            Ok(())
        );
    }

    #[test]
    fn exec_quoting_and_field_codes_follow_the_desktop_entry_spec() {
        assert_eq!(
            parse_exec(r#"/bin/app --name "two words" %U --x=%% "a\"b" "\$HOME""#),
            Some(vec![
                "/bin/app".to_owned(),
                "--name".to_owned(),
                "two words".to_owned(),
                "--x=%".to_owned(),
                "a\"b".to_owned(),
                "$HOME".to_owned(),
            ])
        );
        assert_eq!(parse_exec("  "), None);
        assert_eq!(parse_exec("\"unterminated"), None);
        assert_eq!(parse_exec("%U"), None);
        assert_eq!(parse_exec("\"\" x"), None);
        assert_eq!(
            parse_exec(&unescape_string(r"sh\s-c\s'echo'")),
            Some(vec!["sh".into(), "-c".into(), "'echo'".into()])
        );
        assert_eq!(parse_exec(&"a ".repeat(MAX_ARGUMENTS + 1)), None);
    }

    #[test]
    fn a_user_file_overrides_or_deletes_the_system_entry_of_the_same_id() {
        let system = vec![
            (
                "org.gnome.SettingsDaemon.Power.desktop".to_owned(),
                POWER.as_bytes().to_vec(),
            ),
            (
                "orca-autostart.desktop".to_owned(),
                b"[Desktop Entry]\nType=Application\nExec=orca\n\
AutostartCondition=GSettings org.gnome.desktop.a11y.applications screen-reader-enabled\n"
                    .to_vec(),
            ),
            ("broken.desktop".to_owned(), b"no group here".to_vec()),
            ("README".to_owned(), b"ignored".to_vec()),
        ];
        let user = vec![(
            "org.gnome.SettingsDaemon.Power.desktop".to_owned(),
            b"[Desktop Entry]\nHidden=true\n".to_vec(),
        )];
        let merged = merge(&[user, system]);
        assert_eq!(merged.deleted, ["org.gnome.SettingsDaemon.Power"]);
        assert_eq!(merged.entries.len(), 1);
        assert_eq!(merged.entries[0].id, "orca-autostart");
        assert_eq!(
            merged.invalid,
            [("broken".to_owned(), EntryError::NoDesktopEntryGroup)]
        );
        let mut facts = facts();
        assert_eq!(
            should_start(&merged.entries[0], "GNOME", &facts),
            Err(SkipReason::ConditionUnknown)
        );
        facts.settings.push((
            "org.gnome.desktop.a11y.applications",
            "screen-reader-enabled",
            false,
        ));
        assert_eq!(
            should_start(&merged.entries[0], "GNOME", &facts),
            Err(SkipReason::ConditionFalse)
        );
        facts.settings[0].2 = true;
        assert_eq!(should_start(&merged.entries[0], "GNOME", &facts), Ok(()));
    }

    #[test]
    fn conditions_parse_strictly() {
        assert_eq!(
            Condition::parse("unless-exists gnome-initial-setup-done"),
            Condition::UnlessExists("gnome-initial-setup-done".into())
        );
        assert!(matches!(
            Condition::parse("if-exists ../../etc/passwd"),
            Condition::Unknown(_)
        ));
        assert!(matches!(
            Condition::parse("if-exists /etc/passwd"),
            Condition::Unknown(_)
        ));
        assert_eq!(
            Condition::parse("GNOME3 if-session gnome"),
            Condition::IfSession("gnome".into())
        );
        assert_eq!(
            Condition::parse("GNOME3 unless-session gnome-classic"),
            Condition::UnlessSession("gnome-classic".into())
        );
        assert_eq!(
            Condition::parse("GNOME3 org.gnome.desktop.interface toolkit-accessibility"),
            Condition::GSettings {
                schema: "org.gnome.desktop.interface".into(),
                key: "toolkit-accessibility".into()
            }
        );
        assert!(matches!(
            Condition::parse("GSettings org.gnome;rm key"),
            Condition::Unknown(_)
        ));
        assert!(matches!(
            Condition::parse("KDE whatever"),
            Condition::Unknown(_)
        ));
    }

    #[test]
    fn first_login_condition_and_try_exec_are_honoured() {
        let entry = parse_entry(
            "gnome-initial-setup-first-login.desktop",
            b"[Desktop Entry]\nType=Application\n\
Exec=/usr/libexec/gnome-initial-setup --existing-user\nOnlyShowIn=GNOME;\n\
AutostartCondition=unless-exists gnome-initial-setup-done\n",
        )
        .expect("parses");
        let mut facts = facts();
        assert_eq!(should_start(&entry, "GNOME", &facts), Ok(()));
        facts.files.push("gnome-initial-setup-done");
        assert_eq!(
            should_start(&entry, "GNOME", &facts),
            Err(SkipReason::ConditionFalse)
        );
        let missing = parse_entry(
            "x.desktop",
            b"[Desktop Entry]\nType=Application\nExec=true\nTryExec=/opt/none\n",
        )
        .expect("parses");
        assert_eq!(
            should_start(&missing, "GNOME", &facts),
            Err(SkipReason::NotInstalled)
        );
        let disabled = parse_entry(
            "y.desktop",
            b"[Desktop Entry]\nType=Application\nExec=true\nX-GNOME-Autostart-enabled=false\n",
        )
        .expect("parses");
        assert_eq!(
            should_start(&disabled, "GNOME", &facts),
            Err(SkipReason::Disabled)
        );
    }

    #[test]
    fn invalid_files_and_ids_are_refused() {
        assert_eq!(desktop_file_id("../x.desktop"), None);
        assert_eq!(desktop_file_id(".hidden.desktop"), None);
        assert_eq!(desktop_file_id("x.txt"), None);
        assert_eq!(desktop_file_id("a b.desktop"), None);
        assert_eq!(
            parse_entry("x.desktop", &vec![b'a'; MAX_ENTRY_BYTES + 1]),
            Err(EntryError::TooLarge)
        );
        assert_eq!(
            parse_entry("x.desktop", b"[Desktop Entry]\nType=Link\nURL=x\n"),
            Err(EntryError::NotAnApplication)
        );
        assert_eq!(
            parse_entry("x.desktop", b"[Desktop Entry]\nType=Application\n"),
            Err(EntryError::InvalidExec)
        );
        assert_eq!(
            parse_entry("x.desktop", &[0xff, 0xfe]),
            Err(EntryError::NotUtf8)
        );
        let delay = parse_entry(
            "d.desktop",
            b"[Desktop Entry]\nType=Application\nExec=true\nX-GNOME-Autostart-Delay=99999\n\
[Other Group]\nExec=false\n",
        )
        .expect("parses");
        assert_eq!(delay.delay_secs, MAX_DELAY_SECS);
        assert_eq!(delay.argv, ["true"]);
    }

    #[test]
    fn merge_is_bounded() {
        let many = (0..MAX_ENTRIES + 5)
            .map(|index| {
                (
                    format!("e{index}.desktop"),
                    b"[Desktop Entry]\nType=Application\nExec=true\n".to_vec(),
                )
            })
            .collect::<Vec<_>>();
        let merged = merge(&[many]);
        assert!(merged.truncated);
        assert_eq!(merged.entries.len(), MAX_ENTRIES);
    }
}
