// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The desktop a remote-first sign-in starts for an account that never
//! chose one (`platform.session.default_desktop`).
//!
//! A display manager starts the desktop the account's settings name
//! (`AccountsService` `Session`/`XSession`). An account that never chose one
//! carries its distribution's template value (`Session=gnome` on Enterprise
//! Linux), so a sign-in from a Deck while nobody is at the desk starts that
//! desktop, which may not be the one the workstation's users know (GNOME
//! Classic, say). An administrator may name the desktop such accounts get.
//! Arcen then records it in the account's settings before the login screen
//! takes the sign-in — as the person would by choosing it on the login
//! screen — and only when the account has no choice of its own:
//!
//! - no `AccountsService` record, no session in it, or the template's value:
//!   no choice, the default is applied;
//! - the value Arcen applied before (kept in [`APPLIED_DIR`]): applied
//!   again only when the administrator changed the setting since;
//! - anything else, including the template's value chosen again after
//!   Arcen applied the default, is the account's own choice and is never
//!   changed.
//!
//! The same decision gives the desktop of Arcen's own login session on a
//! host without a display manager. This module holds what needs no
//! operating system: the setting, the record and template readers and the
//! decision.

/// `AccountsService`'s per-account records (`[User]` with `Session=` and
/// `XSession=`).
pub const ACCOUNTS_USER_DIR: &str = "/var/lib/AccountsService/users";

/// `AccountsService`'s account templates, the administrator's first.
pub const ACCOUNTS_TEMPLATE_DIRS: [&str; 2] = [
    "/etc/accountsservice/user-templates",
    "/usr/share/accountsservice/user-templates",
];

/// The templates `AccountsService` gives new accounts (`administrator` for
/// accounts in the administrators' group).
pub const ACCOUNTS_TEMPLATES: [&str; 2] = ["standard", "administrator"];

/// Where Arcen keeps, per uid, the default desktop it applied (root-only).
pub const APPLIED_DIR: &str = "/var/lib/arcen/default-desktop";

/// Where X session entries are installed; a desktop id is the file name of
/// an entry without `.desktop`.
pub const XSESSIONS_DIR: &str = "/usr/share/xsessions";

/// Longest desktop id accepted.
pub const MAX_DESKTOP_ID_LEN: usize = 64;

/// A desktop id (`gnome-classic`, `gnome-xorg`): the name of an X session
/// entry without `.desktop`. ASCII letters, digits, `.`, `_` and `-`, not
/// starting with `.` or `-`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopId(String);

impl DesktopId {
    /// `None` for anything but a plain id.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let valid = !text.is_empty()
            && text.len() <= MAX_DESKTOP_ID_LEN
            && !text.starts_with(['.', '-'])
            && text
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
        valid.then(|| Self(text.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for DesktopId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The desktop ids installed, from the file names in [`XSESSIONS_DIR`],
/// sorted.
#[must_use]
pub fn installed_desktops<S: AsRef<str>>(file_names: &[S]) -> Vec<String> {
    let mut ids: Vec<String> = file_names
        .iter()
        .filter_map(|name| name.as_ref().strip_suffix(".desktop"))
        .filter_map(DesktopId::parse)
        .map(|id| id.0)
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// The session an `AccountsService` record (or template) names: `Session=`,
/// else `XSession=`, in its `[User]` group; `None` when it names none.
#[must_use]
pub fn record_session(text: &str) -> Option<String> {
    let mut in_user = false;
    let mut session = None;
    let mut x_session = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_user = line == "[User]";
            continue;
        }
        if !in_user {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            match key.trim() {
                "Session" => session = Some(value.to_owned()),
                "XSession" => x_session = Some(value.to_owned()),
                _ => {}
            }
        }
    }
    session.or(x_session)
}

/// What the account's settings say about its desktop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountChoice {
    /// No record, no session in it, or a template's value: no choice.
    None,
    /// The default Arcen applied, unchanged since.
    AppliedByArcen(String),
    /// The account's own choice.
    Chosen(String),
}

/// Classifies an account from its `AccountsService` record, the sessions the
/// templates give, and the default Arcen applied to it before.
#[must_use]
pub fn account_choice(
    record: Option<&str>,
    template_sessions: &[String],
    applied: Option<&str>,
) -> AccountChoice {
    let Some(current) = record.and_then(record_session) else {
        return AccountChoice::None;
    };
    match applied {
        Some(applied) if applied == current => AccountChoice::AppliedByArcen(current),
        None if template_sessions.contains(&current) => AccountChoice::None,
        // Anything else, including the template's value chosen again after
        // Arcen applied the default, the account chose.
        _ => AccountChoice::Chosen(current),
    }
}

/// Why the account's desktop is left as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepReason {
    /// `platform.session.default_desktop` is not set.
    NoSetting,
    /// The account chose its desktop.
    AccountChoice,
    /// The account already has the default.
    AlreadyDefault,
    /// The configured desktop is not installed.
    NotInstalled,
}

impl KeepReason {
    /// Stable token for logs and session truth.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::NoSetting => "no_default_desktop",
            Self::AccountChoice => "account_choice",
            Self::AlreadyDefault => "already_default",
            Self::NotInstalled => "default_desktop_not_installed",
        }
    }
}

/// What to do with the account's desktop setting before a sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefaultDesktopDecision {
    /// Record this desktop in the account's settings.
    Apply(DesktopId),
    Keep(KeepReason),
}

impl DefaultDesktopDecision {
    /// Stable token for logs and session truth.
    #[must_use]
    pub const fn token(&self) -> &'static str {
        match self {
            Self::Apply(_) => "default_applied",
            Self::Keep(reason) => reason.token(),
        }
    }
}

/// The decision for one account. `installed` are the desktop ids
/// [`installed_desktops`] found.
#[must_use]
pub fn decide(
    setting: Option<&DesktopId>,
    choice: &AccountChoice,
    current: Option<&str>,
    installed: &[String],
) -> DefaultDesktopDecision {
    use DefaultDesktopDecision::{Apply, Keep};
    let Some(setting) = setting else {
        return Keep(KeepReason::NoSetting);
    };
    match choice {
        AccountChoice::Chosen(_) => return Keep(KeepReason::AccountChoice),
        AccountChoice::AppliedByArcen(applied) if applied == setting.as_str() => {
            return Keep(KeepReason::AlreadyDefault);
        }
        AccountChoice::AppliedByArcen(_) | AccountChoice::None => {}
    }
    if !installed.iter().any(|id| id == setting.as_str()) {
        return Keep(KeepReason::NotInstalled);
    }
    if *choice == AccountChoice::None && current == Some(setting.as_str()) {
        return Keep(KeepReason::AlreadyDefault);
    }
    Apply(setting.clone())
}

/// The desktop Arcen's own login session starts (a host without a display
/// manager): the account's setting as GDM would start it (its own choice,
/// or a default Arcen applied before), else the default the decision
/// applies or finds, else `configured` (`platform.session.desktop`).
#[must_use]
pub fn login_session_desktop(
    choice: &AccountChoice,
    decision: &DefaultDesktopDecision,
    setting: Option<&DesktopId>,
    configured: &str,
) -> String {
    match (choice, decision) {
        (AccountChoice::Chosen(desktop), _)
        | (AccountChoice::AppliedByArcen(desktop), DefaultDesktopDecision::Keep(_)) => {
            desktop.clone()
        }
        (_, DefaultDesktopDecision::Apply(desktop)) => desktop.as_str().to_owned(),
        (_, DefaultDesktopDecision::Keep(KeepReason::AlreadyDefault)) => {
            setting.map_or_else(|| configured.to_owned(), |id| id.as_str().to_owned())
        }
        _ => configured.to_owned(),
    }
}

/// The record Arcen keeps in [`APPLIED_DIR`] for an account: the desktop
/// it applied.
#[must_use]
pub fn applied_record(text: &str) -> Option<String> {
    let line = text.lines().next()?.trim();
    DesktopId::parse(line).map(|id| id.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECORD: &str = "[com.redhat.AccountsServiceUser.System]\nid='rocky'\n\n[User]\nSession=gnome\nIcon=/home/u/.face\nSystemAccount=false\n";
    const TEMPLATE: &str = "# defaults\n[Template]\nEnvironmentFiles=/etc/os-release;\n\n[User]\nSession=gnome\nIcon=${HOME}/.face\n";

    fn id(text: &str) -> DesktopId {
        DesktopId::parse(text).expect("id")
    }

    #[test]
    fn desktop_ids_are_plain_names() {
        assert_eq!(id("gnome-classic").as_str(), "gnome-classic");
        for bad in ["", ".hidden", "-x", "a/b", "a b", "x;y", &"a".repeat(65)] {
            assert_eq!(DesktopId::parse(bad), None, "{bad:?}");
        }
        assert_eq!(
            installed_desktops(&[
                "gnome.desktop",
                "gnome-classic.desktop",
                "README",
                ".x.desktop",
                "gnome.desktop"
            ]),
            ["gnome", "gnome-classic"]
        );
    }

    #[test]
    fn a_record_names_its_session_in_the_user_group() {
        assert_eq!(record_session(RECORD).as_deref(), Some("gnome"));
        assert_eq!(record_session(TEMPLATE).as_deref(), Some("gnome"));
        assert_eq!(
            record_session("[User]\nXSession=gnome-classic\n").as_deref(),
            Some("gnome-classic")
        );
        assert_eq!(
            record_session("[User]\nSession=\nXSession=x\n").as_deref(),
            Some("x")
        );
        assert_eq!(record_session("[Other]\nSession=gnome\n"), None);
        assert_eq!(record_session("[User]\nSystemAccount=false\n"), None);
    }

    #[test]
    fn only_an_account_without_a_choice_of_its_own_gets_the_default() {
        let templates = ["gnome".to_owned()];
        let installed = ["gnome".to_owned(), "gnome-classic".to_owned()];
        let classic = id("gnome-classic");
        let decide_for = |record: Option<&str>, applied: Option<&str>| {
            let choice = account_choice(record, &templates, applied);
            let current = record.and_then(record_session);
            decide(Some(&classic), &choice, current.as_deref(), &installed)
        };
        // No record, the template's value: the default is applied.
        assert_eq!(
            decide_for(None, None),
            DefaultDesktopDecision::Apply(classic.clone())
        );
        assert_eq!(
            decide_for(Some(RECORD), None),
            DefaultDesktopDecision::Apply(classic.clone())
        );
        assert_eq!(
            decide_for(Some("[User]\nIcon=x\n"), None),
            DefaultDesktopDecision::Apply(classic.clone())
        );
        // An explicit choice is never changed.
        assert_eq!(
            decide_for(Some("[User]\nSession=gnome-xorg\n"), None),
            DefaultDesktopDecision::Keep(KeepReason::AccountChoice)
        );
        // Applied before and unchanged: nothing to do.
        assert_eq!(
            decide_for(
                Some("[User]\nSession=gnome-classic\n"),
                Some("gnome-classic")
            ),
            DefaultDesktopDecision::Keep(KeepReason::AlreadyDefault)
        );
        // The template's value chosen again after Arcen applied the
        // default is the account's choice.
        assert_eq!(
            decide_for(Some(RECORD), Some("gnome-classic")),
            DefaultDesktopDecision::Keep(KeepReason::AccountChoice)
        );
        // The administrator changed the setting since Arcen applied the old
        // one, which the account kept.
        let choice = account_choice(
            Some("[User]\nSession=gnome-xorg\n"),
            &templates,
            Some("gnome-xorg"),
        );
        assert_eq!(choice, AccountChoice::AppliedByArcen("gnome-xorg".into()));
        assert_eq!(
            decide(Some(&classic), &choice, Some("gnome-xorg"), &installed),
            DefaultDesktopDecision::Apply(classic.clone())
        );
    }

    #[test]
    fn nothing_is_applied_without_a_setting_an_installed_desktop_or_a_difference() {
        let installed = ["gnome".to_owned()];
        assert_eq!(
            decide(None, &AccountChoice::None, None, &installed),
            DefaultDesktopDecision::Keep(KeepReason::NoSetting)
        );
        assert_eq!(
            decide(
                Some(&id("gnome-classic")),
                &AccountChoice::None,
                None,
                &installed
            ),
            DefaultDesktopDecision::Keep(KeepReason::NotInstalled)
        );
        assert_eq!(
            decide(
                Some(&id("gnome")),
                &AccountChoice::None,
                Some("gnome"),
                &installed
            ),
            DefaultDesktopDecision::Keep(KeepReason::AlreadyDefault)
        );
        assert_eq!(
            DefaultDesktopDecision::Apply(id("gnome")).token(),
            "default_applied"
        );
        assert_eq!(KeepReason::AccountChoice.token(), "account_choice");
    }

    #[test]
    fn the_login_session_starts_the_choice_then_the_default_then_the_configured_desktop() {
        let classic = id("gnome-classic");
        let keep = |reason| DefaultDesktopDecision::Keep(reason);
        assert_eq!(
            login_session_desktop(
                &AccountChoice::Chosen("gnome-xorg".into()),
                &keep(KeepReason::AccountChoice),
                Some(&classic),
                "gnome"
            ),
            "gnome-xorg"
        );
        assert_eq!(
            login_session_desktop(
                &AccountChoice::None,
                &DefaultDesktopDecision::Apply(classic.clone()),
                Some(&classic),
                "gnome"
            ),
            "gnome-classic"
        );
        assert_eq!(
            login_session_desktop(
                &AccountChoice::AppliedByArcen("gnome-classic".into()),
                &keep(KeepReason::AlreadyDefault),
                Some(&classic),
                "gnome"
            ),
            "gnome-classic"
        );
        assert_eq!(
            login_session_desktop(
                &AccountChoice::AppliedByArcen("gnome-classic".into()),
                &keep(KeepReason::NoSetting),
                None,
                "gnome"
            ),
            "gnome-classic",
            "a default applied before stays the account's setting after the key is removed"
        );
        for reason in [KeepReason::NoSetting, KeepReason::NotInstalled] {
            assert_eq!(
                login_session_desktop(&AccountChoice::None, &keep(reason), Some(&classic), "gnome"),
                "gnome"
            );
        }
    }

    #[test]
    fn the_applied_record_holds_one_desktop_id() {
        assert_eq!(
            applied_record("gnome-classic\n").as_deref(),
            Some("gnome-classic")
        );
        assert_eq!(applied_record("../etc\n"), None);
        assert_eq!(applied_record(""), None);
    }
}
