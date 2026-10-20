//! The locale a Pier-started desktop runs in, read from the host.
//!
//! The decision is the shared [`SessionLocale::resolve`]; this adapter only
//! reads the system locale file and the user's `AccountsService` record,
//! both root-owned, bounded, as the root launcher does before the desktop
//! starts.

use std::io::Read;
use std::path::Path;

use arcen_session::session_locale::SessionLocale;

const SYSTEM_LOCALE_CONF: &str = "/etc/locale.conf";
const ACCOUNTS_SERVICE_USERS: &str = "/var/lib/AccountsService/users";
const MAX_FILE_BYTES: u64 = 16 * 1024;

/// The session locale for `username` on this host.
#[must_use]
pub fn session_locale(username: &str) -> SessionLocale {
    let system = read_bounded(Path::new(SYSTEM_LOCALE_CONF));
    let user = safe_username(username)
        .then(|| read_bounded(&Path::new(ACCOUNTS_SERVICE_USERS).join(username)))
        .flatten();
    let locale = SessionLocale::resolve(system.as_deref(), user.as_deref());
    tracing::info!(
        target: crate::logging::target::SESSION,
        lang = locale
            .variables()
            .find_map(|(name, value)| (name == "LANG").then_some(value))
            .unwrap_or("<unset>"),
        variables = locale.variables().count(),
        from_user = user.is_some(),
        "desktop session locale resolved"
    );
    locale
}

/// A user name that is one path component (as the system account database
/// returned it), never a path.
fn safe_username(username: &str) -> bool {
    !username.is_empty()
        && username.len() <= 256
        && username != "."
        && username != ".."
        && !username.contains(['/', '\0'])
}

fn read_bounded(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    file.take(MAX_FILE_BYTES).read_to_string(&mut text).ok()?;
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_single_component_user_names_reach_the_accounts_directory() {
        assert!(safe_username("artist"));
        assert!(safe_username("first.last"));
        for bad in ["", ".", "..", "a/b", "../root", "nul\0l"] {
            assert!(!safe_username(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_missing_file_is_absent_not_an_error() {
        assert_eq!(read_bounded(Path::new("/nonexistent/locale.conf")), None);
    }
}
