//! Which variables a user's login shell may add to a desktop session.
//!
//! A GDM login starts GNOME through the user's login shell, so variables
//! from `~/.profile` (a `PATH` addition, a licence server) reach every
//! application. An Arcen session reproduces that by running the login
//! shell once, as the session user, and reading `env -0`. This module
//! decides which of those variables enter the session: never one that
//! Arcen owns (the display, the runtime directory, the buses, the locale
//! and time zone it resolves itself), never a loader or agent variable, and
//! only within size bounds.

/// Largest `env -0` dump read.
pub const MAX_DUMP_BYTES: usize = 64 * 1024;
/// Most variables imported.
pub const MAX_VARIABLES: usize = 256;
/// Longest variable name.
pub const MAX_NAME_BYTES: usize = 128;
/// Longest variable value.
pub const MAX_VALUE_BYTES: usize = 8 * 1024;

/// Names Arcen owns or that describe the shell process rather than the
/// session.
const DENIED_NAMES: &[&str] = &[
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "WAYLAND_SOCKET",
    "XAUTHORITY",
    "XDG_RUNTIME_DIR",
    "XDG_CURRENT_DESKTOP",
    "XDG_SEAT",
    "XDG_VTNR",
    "XDG_MENU_PREFIX",
    "DESKTOP_SESSION",
    "GDMSESSION",
    "GDK_BACKEND",
    "DCONF_PROFILE",
    "PULSE_RUNTIME_PATH",
    "PULSE_SERVER",
    "PIPEWIRE_REMOTE",
    "PIPEWIRE_RUNTIME_DIR",
    "GNOME_KEYRING_CONTROL",
    "GNOME_DESKTOP_SESSION_ID",
    "DESKTOP_AUTOSTART_ID",
    "SESSION_MANAGER",
    "GVFS_DISABLE_FUSE",
    "TZ",
    "LANG",
    "LANGUAGE",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "PWD",
    "OLDPWD",
    "SHLVL",
    "_",
    "TERM",
    "MAIL",
    "INVOCATION_ID",
    "JOURNAL_STREAM",
    "NOTIFY_SOCKET",
    "MANAGERPID",
    "SYSTEMD_EXEC_PID",
    "MOTD_SHOWN",
    "GPG_AGENT_INFO",
    // A different settings backend would bypass the session's dconf profile
    // and with it the pinned power and lock keys.
    "GSETTINGS_BACKEND",
    "SHELL_DEBUG",
    // GPU and driver selection: the compositor's render node and the
    // qualified GPU import route are proven for the host's own driver.
    "DRI_PRIME",
    "EGL_PLATFORM",
];

/// Prefixes of names Arcen owns, that change how programs load or
/// authenticate, or that change how the compositor composes and on which
/// GPU (Mutter, Clutter and Cogl debug and driver switches such as
/// `MUTTER_DEBUG_FORCE_HDR`, GNOME Shell's session mode, EGL/GLX vendor and
/// Mesa driver overrides).
const DENIED_PREFIXES: &[&str] = &[
    "DBUS_",
    "LD_",
    "SSH_",
    "XDG_SESSION_",
    "LC_",
    "ARCEN_",
    "LISTEN_",
    "GIO_LAUNCHED_",
    "PAM_",
    "BASH_FUNC_",
    "MUTTER_",
    "CLUTTER_",
    "COGL_",
    "GNOME_SHELL_",
    "__EGL_",
    "__GLX_",
    "__NV_",
    "LIBGL_",
    "MESA_",
    "GBM_",
];

/// Why a variable was not imported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// Arcen owns it or it is not a session variable.
    Denied,
    /// The name is not a plain identifier.
    InvalidName,
    /// The name or value is too long, or the dump was truncated.
    TooLarge,
    /// The session already has exactly this value.
    Unchanged,
}

/// The result of [`filter_login_environment`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoginEnvironment {
    /// Variables to set in the session, in dump order.
    pub imported: Vec<(String, String)>,
    /// Names not imported and why (never their values).
    pub dropped: Vec<(String, DropReason)>,
}

impl LoginEnvironment {
    /// The imported names, for logs (values are never logged: they may
    /// hold licence keys or tokens).
    #[must_use]
    pub fn imported_names(&self) -> Vec<&str> {
        self.imported
            .iter()
            .map(|(name, _)| name.as_str())
            .collect()
    }
}

/// Whether `name` may be imported at all.
#[must_use]
pub fn name_allowed(name: &str) -> bool {
    !DENIED_NAMES.contains(&name)
        && !DENIED_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// Filters a NUL-separated `env -0` dump. `current` gives the session's
/// value of a name, so unchanged variables are not re-imported.
#[must_use]
pub fn filter_login_environment(
    dump: &[u8],
    current: impl Fn(&str) -> Option<String>,
) -> LoginEnvironment {
    let truncated = dump.len() > MAX_DUMP_BYTES;
    let dump = &dump[..dump.len().min(MAX_DUMP_BYTES)];
    let mut result = LoginEnvironment::default();
    let records = dump.split(|byte| *byte == 0).collect::<Vec<_>>();
    let last = records.len().saturating_sub(1);
    for (index, record) in records.into_iter().enumerate() {
        if record.is_empty() {
            continue;
        }
        let Ok(record) = std::str::from_utf8(record) else {
            continue;
        };
        let Some((name, value)) = record.split_once('=') else {
            continue;
        };
        let reason = if truncated && index == last {
            Some(DropReason::TooLarge)
        } else if !valid_name(name) {
            Some(DropReason::InvalidName)
        } else if !name_allowed(name) {
            Some(DropReason::Denied)
        } else if name.len() > MAX_NAME_BYTES || value.len() > MAX_VALUE_BYTES {
            Some(DropReason::TooLarge)
        } else if current(name).as_deref() == Some(value) {
            Some(DropReason::Unchanged)
        } else if result.imported.len() >= MAX_VARIABLES {
            Some(DropReason::TooLarge)
        } else {
            None
        };
        match reason {
            None => {
                result.imported.retain(|(existing, _)| existing != name);
                result.imported.push((name.to_owned(), value.to_owned()));
            }
            Some(reason) => {
                if name.len() <= MAX_NAME_BYTES && valid_name(name) {
                    result.dropped.push((name.to_owned(), reason));
                }
            }
        }
    }
    result
}

fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_session_neutral_variables_are_imported() {
        let dump = b"PATH=/home/u/bin:/usr/bin\0LM_LICENSE_FILE=27000@licence.example.internal\0\
DISPLAY=:9\0DBUS_SESSION_BUS_ADDRESS=unix:path=/x\0LD_PRELOAD=/x.so\0SSH_AUTH_SOCK=/a\0\
XDG_RUNTIME_DIR=/run/user/1\0LC_ALL=C\0TZ=UTC\0HOME=/home/u\0bad-name=1\0\
EDITOR=vim\0QT_QPA_PLATFORM=xcb\0BASH_FUNC_x%%=() { :; }\0EDITOR=nano\0";
        let env =
            filter_login_environment(dump, |name| (name == "EDITOR").then(|| "nano".to_owned()));
        assert_eq!(
            env.imported,
            [
                ("PATH".to_owned(), "/home/u/bin:/usr/bin".to_owned()),
                (
                    "LM_LICENSE_FILE".to_owned(),
                    "27000@licence.example.internal".to_owned()
                ),
                ("EDITOR".to_owned(), "vim".to_owned()),
                ("QT_QPA_PLATFORM".to_owned(), "xcb".to_owned()),
            ]
        );
        assert_eq!(
            env.imported_names(),
            ["PATH", "LM_LICENSE_FILE", "EDITOR", "QT_QPA_PLATFORM"]
        );
        let denied = env
            .dropped
            .iter()
            .filter(|(_, reason)| *reason == DropReason::Denied)
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            denied,
            [
                "DISPLAY",
                "DBUS_SESSION_BUS_ADDRESS",
                "LD_PRELOAD",
                "SSH_AUTH_SOCK",
                "XDG_RUNTIME_DIR",
                "LC_ALL",
                "TZ",
                "HOME"
            ]
        );
        assert!(
            env.dropped
                .contains(&("EDITOR".to_owned(), DropReason::Unchanged))
        );
    }

    #[test]
    fn compositor_switches_and_settings_backends_never_enter() {
        for name in [
            "MUTTER_DEBUG_FORCE_HDR",
            "MUTTER_DEBUG_DUMMY_MODE_SPECS",
            "CLUTTER_PAINT",
            "COGL_DRIVER",
            "GNOME_SHELL_SESSION_MODE",
            "GSETTINGS_BACKEND",
            "__EGL_VENDOR_LIBRARY_FILENAMES",
            "__GLX_VENDOR_LIBRARY_NAME",
            "__NV_PRIME_RENDER_OFFLOAD",
            "LIBGL_ALWAYS_SOFTWARE",
            "MESA_LOADER_DRIVER_OVERRIDE",
            "GBM_BACKEND",
            "DRI_PRIME",
        ] {
            assert!(!name_allowed(name), "{name} must be denied");
        }
        for name in [
            "XDG_CONFIG_HOME",
            "XDG_CONFIG_DIRS",
            "GSK_RENDERER",
            "QT_QPA_PLATFORM",
        ] {
            assert!(name_allowed(name), "{name} is the user's");
        }
    }

    #[test]
    fn dumps_and_values_are_bounded() {
        let long = format!("BIG={}\0OK=1\0", "x".repeat(MAX_VALUE_BYTES + 1));
        let env = filter_login_environment(long.as_bytes(), |_| None);
        assert_eq!(env.imported, [("OK".to_owned(), "1".to_owned())]);
        assert_eq!(env.dropped, [("BIG".to_owned(), DropReason::TooLarge)]);

        let mut huge = Vec::new();
        for index in 0..10_000 {
            huge.extend_from_slice(format!("V{index}=value\0").as_bytes());
        }
        let env = filter_login_environment(&huge, |_| None);
        assert_eq!(env.imported.len(), MAX_VARIABLES);
        assert!(
            env.dropped
                .iter()
                .all(|(_, reason)| *reason == DropReason::TooLarge)
        );
        assert_eq!(
            filter_login_environment(&[0xff, b'=', 0], |_| None),
            LoginEnvironment::default()
        );
    }
}
