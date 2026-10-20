//! The operator's first-login policy and GNOME's own completion markers.
//!
//! On a user's first GNOME login, GNOME runs its initial-setup assistant
//! (`gnome-initial-setup --existing-user`, an autostart entry conditioned on
//! `$XDG_CONFIG_HOME/gnome-initial-setup-done`) and GNOME Shell offers its
//! welcome tour (when `org.gnome.shell welcome-dialog-last-shown-version`
//! is older than the running shell). An Arcen session behaves the same by
//! default. A fleet operator may choose `skip`: Arcen then writes exactly
//! those two markers for the user before the desktop starts, as completing
//! the assistant and the tour would have; nothing else is changed and no
//! input is faked.

use serde::Deserialize;

use super::version;

/// `wayland.first_login_setup`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FirstLoginSetup {
    /// GNOME's first-login assistant and welcome tour run as on a GDM login.
    #[default]
    Run,
    /// GNOME's completion markers are set for the user, so neither runs.
    Skip,
}

impl FirstLoginSetup {
    /// Stable token (also the launcher and agent argument value).
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Skip => "skip",
        }
    }

    /// The policy for a token.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "run" => Some(Self::Run),
            "skip" => Some(Self::Skip),
            _ => None,
        }
    }
}

/// The initial-setup completion marker, relative to `$XDG_CONFIG_HOME`.
pub const INITIAL_SETUP_DONE_MARKER: &str = "gnome-initial-setup-done";
/// What `gnome-initial-setup` writes into it.
pub const INITIAL_SETUP_DONE_CONTENT: &str = "yes";
/// The shell's welcome-tour schema and key.
pub const WELCOME_DIALOG_SCHEMA: &str = "org.gnome.shell";
/// See [`WELCOME_DIALOG_SCHEMA`].
pub const WELCOME_DIALOG_KEY: &str = "welcome-dialog-last-shown-version";

/// Whether the welcome-tour marker must be raised to `shell_version` so
/// the tour does not open: only when the recorded version is older (the
/// marker is never lowered).
#[must_use]
pub fn welcome_marker_needs_update(shell_version: &str, last_shown: &str) -> bool {
    valid_version(shell_version) && version::compare(shell_version, last_shown).is_gt()
}

/// A shell version as `gnome-shell --version` prints it after "GNOME Shell ".
#[must_use]
pub fn shell_version_from_banner(banner: &str) -> Option<&str> {
    let version = banner.trim().strip_prefix("GNOME Shell ")?.trim();
    valid_version(version).then_some(version)
}

fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 32
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_policy_parses_from_configuration() {
        #[derive(Deserialize)]
        struct Holder {
            #[serde(default)]
            first_login_setup: FirstLoginSetup,
        }
        let parse = |text: &str| serde_json::from_str::<Holder>(text).map(|h| h.first_login_setup);
        assert_eq!(parse("{}").expect("default"), FirstLoginSetup::Run);
        assert_eq!(
            parse(r#"{"first_login_setup":"skip"}"#).expect("skip"),
            FirstLoginSetup::Skip
        );
        assert!(parse(r#"{"first_login_setup":"Skip"}"#).is_err());
        assert_eq!(
            FirstLoginSetup::from_token("skip"),
            Some(FirstLoginSetup::Skip)
        );
        assert_eq!(FirstLoginSetup::from_token("never"), None);
    }

    #[test]
    fn the_welcome_marker_is_only_ever_raised() {
        assert!(welcome_marker_needs_update("49.4", ""));
        assert!(welcome_marker_needs_update("49.4", "48.1"));
        assert!(!welcome_marker_needs_update("49.4", "49.4"));
        assert!(!welcome_marker_needs_update("49.4", "50.0"));
        assert!(!welcome_marker_needs_update("4'9", ""));
        assert_eq!(
            shell_version_from_banner("GNOME Shell 49.4\n"),
            Some("49.4")
        );
        assert_eq!(shell_version_from_banner("mutter 49.4"), None);
        assert_eq!(shell_version_from_banner("GNOME Shell 49;rm"), None);
    }
}
