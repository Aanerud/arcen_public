// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The default desktop of a remote-first sign-in
//! (`platform.session.default_desktop`): the Linux adapter.
//!
//! Every decision is `arcen_session::console_session::account_desktop`'s.
//! This module reads the account's AccountsService record and templates and
//! the record of what Arcen applied before, and carries out an `Apply`
//! through AccountsService's own D-Bus interface (`SetXSession`,
//! `SetSession`), as the login screen's session menu does, so the running
//! `accounts-daemon` and GDM see it. It never edits AccountsService's files.

use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::Path;
use std::time::Duration;

use arcen_session::console_session::account_desktop::{
    self as policy, AccountChoice, DefaultDesktopDecision, DesktopId,
};

use crate::logging::target;

/// AccountsService records and templates are small; anything larger is
/// not one.
const MAX_RECORD_BYTES: u64 = 64 * 1024;
const DBUS_TIMEOUT: Duration = Duration::from_secs(5);

/// What [`prepare`] found and did for one sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Prepared {
    pub(crate) choice: AccountChoice,
    pub(crate) decision: DefaultDesktopDecision,
    /// An `Apply` that AccountsService refused or that could not be asked.
    pub(crate) failed: bool,
}

impl Prepared {
    /// Stable token for logs and the launcher's response.
    pub(crate) fn token(&self) -> &'static str {
        if self.failed {
            "default_not_applied"
        } else {
            self.decision.token()
        }
    }
}

fn read_small(path: &Path) -> Option<String> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_RECORD_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// A user name that is one path component.
fn plain_name(username: &str) -> bool {
    !username.is_empty()
        && username.len() <= 256
        && username != "."
        && username != ".."
        && !username.contains(['/', '\0'])
}

/// The desktop ids installed in `/usr/share/xsessions`.
pub(crate) fn installed() -> Vec<String> {
    let names: Vec<String> = std::fs::read_dir(policy::XSESSIONS_DIR)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|entry| entry.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    policy::installed_desktops(&names)
}

fn template_sessions() -> Vec<String> {
    let mut sessions = Vec::new();
    for dir in policy::ACCOUNTS_TEMPLATE_DIRS {
        for template in policy::ACCOUNTS_TEMPLATES {
            if let Some(session) = read_small(&Path::new(dir).join(template))
                .as_deref()
                .and_then(policy::record_session)
            {
                if !sessions.contains(&session) {
                    sessions.push(session);
                }
            }
        }
    }
    sessions
}

fn applied_path(uid: u32) -> std::path::PathBuf {
    Path::new(policy::APPLIED_DIR).join(uid.to_string())
}

fn applied_for(uid: u32) -> Option<String> {
    read_small(&applied_path(uid))
        .as_deref()
        .and_then(policy::applied_record)
}

/// Records, root-only, the default applied to `uid`.
fn record_applied(uid: u32, desktop: &DesktopId) -> Result<(), String> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(policy::APPLIED_DIR)
        .map_err(|error| format!("{}: {error}", policy::APPLIED_DIR))?;
    let path = applied_path(uid);
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&temporary);
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&temporary)?;
        writeln!(file, "{desktop}")?;
        file.sync_all()?;
        std::fs::rename(&temporary, &path)
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("{}: {error}", path.display()));
    }
    Ok(())
}

/// Sets the account's desktop through AccountsService, as the login
/// screen's session menu does.
async fn set_account_desktop(username: &str, desktop: &DesktopId) -> Result<(), String> {
    let connection = zbus::Connection::system()
        .await
        .map_err(|error| format!("system bus: {error}"))?;
    let reply = connection
        .call_method(
            Some("org.freedesktop.Accounts"),
            "/org/freedesktop/Accounts",
            Some("org.freedesktop.Accounts"),
            "FindUserByName",
            &(username,),
        )
        .await
        .map_err(|error| format!("FindUserByName: {error}"))?;
    let user: zbus::zvariant::OwnedObjectPath = reply
        .body()
        .deserialize()
        .map_err(|error| format!("FindUserByName: {error}"))?;
    connection
        .call_method(
            Some("org.freedesktop.Accounts"),
            &user,
            Some("org.freedesktop.Accounts.User"),
            "SetXSession",
            &(desktop.as_str(),),
        )
        .await
        .map_err(|error| format!("SetXSession: {error}"))?;
    // `Session` (AccountsService 0.6.50 and later) is what GDM reads first.
    match connection
        .call_method(
            Some("org.freedesktop.Accounts"),
            &user,
            Some("org.freedesktop.Accounts.User"),
            "SetSession",
            &(desktop.as_str(),),
        )
        .await
    {
        Ok(_) => Ok(()),
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod" =>
        {
            Ok(())
        }
        Err(error) => Err(format!("SetSession: {error}")),
    }
}

/// Before a remote-first sign-in (or Arcen's own login session): gives an
/// account without a desktop choice of its own the configured default.
pub(crate) async fn prepare(username: &str, uid: u32, setting: Option<&DesktopId>) -> Prepared {
    if !plain_name(username) {
        return Prepared {
            choice: AccountChoice::None,
            decision: DefaultDesktopDecision::Keep(policy::KeepReason::NoSetting),
            failed: false,
        };
    }
    let record = read_small(&Path::new(policy::ACCOUNTS_USER_DIR).join(username));
    let current = record.as_deref().and_then(policy::record_session);
    let applied = applied_for(uid);
    let choice =
        policy::account_choice(record.as_deref(), &template_sessions(), applied.as_deref());
    let decision = match setting {
        None => DefaultDesktopDecision::Keep(policy::KeepReason::NoSetting),
        Some(_) => policy::decide(setting, &choice, current.as_deref(), &installed()),
    };
    let mut failed = false;
    if let DefaultDesktopDecision::Apply(desktop) = &decision {
        match tokio::time::timeout(DBUS_TIMEOUT, set_account_desktop(username, desktop)).await {
            Ok(Ok(())) => {
                if let Err(error) = record_applied(uid, desktop) {
                    tracing::warn!(target: target::SESSION, %error, "the applied default desktop could not be recorded; a later sign-in may apply it again");
                }
            }
            Ok(Err(error)) => {
                failed = true;
                tracing::warn!(target: target::SESSION, %error, desktop = %desktop, "AccountsService did not take the default desktop; the account's own setting stays");
            }
            Err(_) => {
                failed = true;
                tracing::warn!(target: target::SESSION, desktop = %desktop, "AccountsService did not answer; the account's own setting stays");
            }
        }
    }
    let prepared = Prepared {
        choice,
        decision,
        failed,
    };
    tracing::info!(
        target: target::SESSION,
        username,
        default_desktop = %setting.map(DesktopId::as_str).unwrap_or(""),
        account_desktop = current.as_deref().unwrap_or(""),
        decision = prepared.token(),
        "default desktop for this sign-in"
    );
    prepared
}

/// The desktop a session runs: what logind recorded (`DESKTOP=`), else
/// `XDG_SESSION_DESKTOP` of the user's processes in its scope (GDM 40
/// records none). Only a plain desktop id is reported.
pub(crate) fn session_desktop(session_id: &str, uid: u32) -> Option<String> {
    if session_id.is_empty() || !session_id.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return None;
    }
    read_small(&Path::new("/run/systemd/sessions").join(session_id))
        .and_then(|record| {
            record
                .lines()
                .find_map(|line| line.strip_prefix("DESKTOP=").map(str::to_owned))
        })
        .or_else(|| super::console::session_environ_desktop(session_id, uid))
        .and_then(|desktop| DesktopId::parse(&desktop))
        .map(|desktop| desktop.as_str().to_owned())
}

/// At Pier start: a configured default desktop that is not installed is
/// never applied; say so once.
pub fn check_at_start(setting: Option<&DesktopId>) {
    let Some(setting) = setting else {
        return;
    };
    let installed = installed();
    if installed.iter().any(|id| id == setting.as_str()) {
        tracing::info!(target: target::SESSION, default_desktop = %setting, "remote-first sign-ins give accounts without a desktop choice this desktop");
    } else {
        tracing::warn!(
            target: target::SESSION,
            default_desktop = %setting,
            installed = %installed.join(","),
            "platform.session.default_desktop names no X session entry in /usr/share/xsessions; accounts keep their own desktop"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_user_names_name_a_record() {
        assert!(plain_name("someone"));
        assert!(plain_name("user@example.internal"));
        for bad in ["", ".", "..", "a/b", "../etc/passwd", "a\0b"] {
            assert!(!plain_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_session_desktop_is_read_only_for_a_plain_session_id() {
        assert_eq!(session_desktop("../x", 4_000_000_001), None);
        assert_eq!(session_desktop("", 4_000_000_001), None);
    }

    #[tokio::test]
    async fn without_a_setting_nothing_is_changed() {
        let prepared = prepare("arcen-test-no-such-account", 4_000_000_001, None).await;
        assert_eq!(prepared.token(), "no_default_desktop");
        assert_eq!(prepared.choice, AccountChoice::None);
        assert!(!prepared.failed);
    }
}
