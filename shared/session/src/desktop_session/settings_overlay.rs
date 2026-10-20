//! The settings an Arcen session pins, for a per-session dconf database.
//!
//! dconf reads a profile (`DCONF_PROFILE`) that lists databases; a key
//! locked in a database overrides every database after it, the user's own
//! included. An Arcen session lists its own compiled database right after
//! the user's, so the handful of keys below win for processes of that
//! session only: nothing under `/etc` changes, the user's own settings stay
//! untouched (and apply again in any other session), and Settings shows the
//! pinned keys as locked.
//!
//! Every value comes from a closed set here; nothing user-controlled is
//! ever rendered.

use std::fmt::Write as _;

use super::lock::LockPolicy;

/// One pinned key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedKey {
    /// dconf directory, without leading or trailing slash.
    pub directory: &'static str,
    /// Key name.
    pub key: &'static str,
    /// The value in `GVariant` text format.
    pub value: &'static str,
}

impl PinnedKey {
    /// The key's dconf path.
    #[must_use]
    pub fn path(&self) -> String {
        format!("/{}/{}", self.directory, self.key)
    }
}

const POWER: &str = "org/gnome/settings-daemon/plugins/power";

/// Keys every session pins: a remote session never suspends the host, on
/// idle or on a power key.
pub const ALWAYS_PINNED: [PinnedKey; 3] = [
    PinnedKey {
        directory: POWER,
        key: "sleep-inactive-ac-type",
        value: "'nothing'",
    },
    PinnedKey {
        directory: POWER,
        key: "sleep-inactive-battery-type",
        value: "'nothing'",
    },
    PinnedKey {
        directory: POWER,
        key: "power-button-action",
        value: "'nothing'",
    },
];

/// Pinned when [`LockPolicy`] refuses locking.
pub const LOCK_DISABLED: PinnedKey = PinnedKey {
    directory: "org/gnome/desktop/lockdown",
    key: "disable-lock-screen",
    value: "true",
};

/// The keys a session with `lock` pins.
#[must_use]
pub fn pinned_keys(lock: LockPolicy) -> Vec<PinnedKey> {
    let mut keys = ALWAYS_PINNED.to_vec();
    if !lock.allows_lock() {
        keys.push(LOCK_DISABLED);
    }
    keys
}

/// The keyfile `dconf compile` reads.
#[must_use]
pub fn render_keyfile(keys: &[PinnedKey]) -> String {
    let mut directories = keys.iter().map(|key| key.directory).collect::<Vec<_>>();
    directories.sort_unstable();
    directories.dedup();
    let mut out = String::from("# Arcen session settings: generated, per session.\n");
    for directory in directories {
        let _ = write!(out, "\n[{directory}]\n");
        for key in keys.iter().filter(|key| key.directory == directory) {
            let _ = writeln!(out, "{}={}", key.key, key.value);
        }
    }
    out
}

/// The locks file (`locks/` of the keyfile directory).
#[must_use]
pub fn render_locks(keys: &[PinnedKey]) -> String {
    keys.iter().fold(String::new(), |mut out, key| {
        let _ = writeln!(out, "{}", key.path());
        out
    })
}

/// Longest base profile considered, in bytes.
pub const MAX_PROFILE_BYTES: usize = 8 * 1024;

/// The session's dconf profile: the user's writable database, then the
/// session's database, then every system or file database the profile the
/// user would otherwise get lists (`base`, when there is one). `None` when
/// `database` is not a plain absolute path.
#[must_use]
pub fn compose_profile(database: &str, base: Option<&str>) -> Option<String> {
    if !database.starts_with('/')
        || database
            .chars()
            .any(|character| character.is_control() || character == ' ')
    {
        return None;
    }
    let mut lines = vec!["user-db:user".to_owned(), format!("file-db:{database}")];
    if let Some(base) = base.filter(|base| base.len() <= MAX_PROFILE_BYTES) {
        for line in base.lines().map(str::trim) {
            let keep = (line.starts_with("system-db:") || line.starts_with("file-db:"))
                && !line.chars().any(char::is_control);
            if keep && !lines.iter().any(|existing| existing == line) {
                lines.push(line.to_owned());
            }
        }
    }
    Some(lines.join("\n") + "\n")
}

#[cfg(test)]
mod tests {
    use super::super::lock::{LockDisabledReason, LockPolicy};
    use super::*;

    #[test]
    fn power_keys_are_always_pinned_and_lock_only_without_an_unlock_path() {
        let honour = pinned_keys(LockPolicy::HonourUserSettings);
        assert_eq!(honour.len(), 3);
        let disabled = pinned_keys(LockPolicy::Disabled(
            LockDisabledReason::NoInSessionUnlockPath,
        ));
        assert_eq!(disabled.last(), Some(&LOCK_DISABLED));
        assert_eq!(
            render_keyfile(&disabled),
            "# Arcen session settings: generated, per session.\n\n\
[org/gnome/desktop/lockdown]\ndisable-lock-screen=true\n\n\
[org/gnome/settings-daemon/plugins/power]\nsleep-inactive-ac-type='nothing'\n\
sleep-inactive-battery-type='nothing'\npower-button-action='nothing'\n"
        );
        assert_eq!(
            render_locks(&honour),
            "/org/gnome/settings-daemon/plugins/power/sleep-inactive-ac-type\n\
/org/gnome/settings-daemon/plugins/power/sleep-inactive-battery-type\n\
/org/gnome/settings-daemon/plugins/power/power-button-action\n"
        );
    }

    #[test]
    fn the_profile_keeps_the_administrators_system_databases_after_ours() {
        assert_eq!(
            compose_profile(
                "/run/user/1000/arcen-wayland-7/dconf/arcen.db",
                Some("user-db:user\nsystem-db:local\n# c\nsystem-db:site\nservice-db:x\nsystem-db:local\n")
            )
            .as_deref(),
            Some(
                "user-db:user\nfile-db:/run/user/1000/arcen-wayland-7/dconf/arcen.db\n\
system-db:local\nsystem-db:site\n"
            )
        );
        assert_eq!(
            compose_profile("/r/a.db", None).as_deref(),
            Some("user-db:user\nfile-db:/r/a.db\n")
        );
        assert_eq!(compose_profile("relative.db", None), None);
        assert_eq!(compose_profile("/a b.db", None), None);
        assert_eq!(compose_profile("/a\nsystem-db:x", None), None);
    }
}
