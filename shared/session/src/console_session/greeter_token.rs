// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The one-time token that completes a remote-first sign-in through the
//! login screen (ADR 0025, "Remote-first sign-in").
//!
//! The display manager resumes only sessions it started itself, so a Deck
//! that signs in while nobody is at the workstation must have its session
//! started by the login screen. GNOME's login screen has a pre-authenticated
//! credential hook for exactly this: when the credential service it watches
//! on the system bus sends it a token, it starts its `gdm-ovirtcred` PAM
//! conversation and answers the conversation's secret prompt with that
//! token. Arcen owns that bus name only while it completes a sign-in, and
//! its PAM module in that stack turns the token into the user.
//!
//! The token is the authorisation, so it is made worthless outside its one
//! use: 32 random bytes, sent only to the login screen's own bus
//! connections; on disk only a root-only record named by the token's SHA-256
//! (never the token), naming one user and one deadline 30 seconds out;
//! consumed (unlinked) by the first conversation that presents it, whether
//! it is then admitted or not. The Pier creates a record only after the
//! user's own password passed the Pier's PAM service and account checks.

use sha2::{Digest, Sha256};

/// The login screen's PAM service for the pre-authenticated hook.
pub const PAM_SERVICE: &str = "gdm-ovirtcred";
/// The system-bus name the login screen watches for that service.
pub const BUS_NAME: &str = "org.ovirt.vdsm.Credentials";
/// Object path of the credential signal.
pub const OBJECT_PATH: &str = "/org/ovirt/vdsm/Credentials";
/// Interface of the credential signal.
pub const INTERFACE: &str = "org.ovirt.vdsm.Credentials";
/// The signal that carries the token (one string argument).
pub const SIGNAL: &str = "UserAuthenticated";
/// Where token records live: inside GDM's own root-owned runtime
/// directory, so the display manager's session worker (`SELinux` `xdm_t`)
/// can read and remove them without an extra policy rule.
pub const RECORD_DIR: &str = "/run/gdm/arcen-greeter-login";
/// Random bytes in a token.
pub const TOKEN_BYTES: usize = 32;
/// Characters in a token (lowercase hexadecimal).
pub const TOKEN_HEX_LEN: usize = TOKEN_BYTES * 2;
/// How long a record admits its token.
pub const LIFETIME_SECS: u64 = 30;
/// Largest record a module reads.
pub const MAX_RECORD_BYTES: usize = 256;
/// First line of the PAM stack Arcen installs for [`PAM_SERVICE`]; a stack
/// without it belongs to someone else and is never replaced or used.
pub const PAM_STACK_MARKER: &str = "# arcen-greeter-login v1";
const RECORD_HEADER: &str = "arcen-greeter-login v1";

/// A token's text form.
#[must_use]
pub fn token_text(bytes: &[u8; TOKEN_BYTES]) -> String {
    hex(bytes)
}

/// The record file name for `token`, or `None` when it is not a token
/// (wrong length or not lowercase hexadecimal): nothing else is ever looked
/// up.
#[must_use]
pub fn record_name(token: &str) -> Option<String> {
    let valid = token.len() == TOKEN_HEX_LEN
        && token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    valid.then(|| hex(&Sha256::digest(token.as_bytes())))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    text
}

/// A user name a record may carry: what `useradd` and directory services
/// produce for login names, and nothing a path, a PAM item or a log line
/// could misread.
#[must_use]
pub fn username_valid(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'@'))
}

/// Whom a token signs in, and until when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GreeterTokenRecord {
    pub username: String,
    pub uid: u32,
    /// Unix time after which the token admits nobody.
    pub not_after: u64,
}

impl GreeterTokenRecord {
    /// A record for `username`/`uid` issued at `now`; `None` for root or a
    /// name no record may carry.
    #[must_use]
    pub fn issue(username: &str, uid: u32, now: u64) -> Option<Self> {
        (uid != 0 && username_valid(username)).then(|| Self {
            username: username.to_owned(),
            uid,
            not_after: now.saturating_add(LIFETIME_SECS),
        })
    }

    /// The file contents.
    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "{RECORD_HEADER}\nuser={}\nuid={}\nnot_after={}\n",
            self.username, self.uid, self.not_after
        )
    }

    /// Parses [`Self::encode`] exactly; anything else is no record.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_RECORD_BYTES {
            return None;
        }
        let text = std::str::from_utf8(bytes).ok()?;
        let mut lines = text.strip_suffix('\n')?.split('\n');
        if lines.next()? != RECORD_HEADER {
            return None;
        }
        let username = lines.next()?.strip_prefix("user=")?;
        let uid = lines.next()?.strip_prefix("uid=")?;
        let not_after = lines.next()?.strip_prefix("not_after=")?;
        if lines.next().is_some() || !username_valid(username) {
            return None;
        }
        let digits = |value: &str| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit());
        if !digits(uid) || !digits(not_after) {
            return None;
        }
        Some(Self {
            username: username.to_owned(),
            uid: uid.parse().ok()?,
            not_after: not_after.parse().ok()?,
        })
    }
}

/// Why a presented token admits nobody (logged by the PAM module; the login
/// screen only sees a failed conversation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenRejection {
    /// Not a token, or no record, or an unreadable or malformed one.
    Unknown,
    /// The record's file or directory is not root's alone.
    UntrustedRecord,
    /// The deadline passed.
    Expired,
    /// The deadline is further out than any record the Pier writes.
    ImplausibleDeadline,
    /// The record names root.
    Root,
    /// The account no longer exists or its uid changed since the Pier
    /// checked the password.
    AccountChanged,
}

impl TokenRejection {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Unknown => "unknown_token",
            Self::UntrustedRecord => "untrusted_record",
            Self::Expired => "expired",
            Self::ImplausibleDeadline => "implausible_deadline",
            Self::Root => "root",
            Self::AccountChanged => "account_changed",
        }
    }
}

/// Whether a consumed record admits its user at `now`, given the uid the
/// system resolves the record's user name to.
///
/// # Errors
///
/// The [`TokenRejection`] that refuses it.
pub fn admit(
    record: &GreeterTokenRecord,
    now: u64,
    resolved_uid: Option<u32>,
) -> Result<(), TokenRejection> {
    if record.uid == 0 || resolved_uid == Some(0) {
        return Err(TokenRejection::Root);
    }
    if now > record.not_after {
        return Err(TokenRejection::Expired);
    }
    // A clock that moved back by more than a few seconds still cannot make
    // a record live longer than one lifetime from now.
    if record.not_after > now.saturating_add(LIFETIME_SECS + 5) {
        return Err(TokenRejection::ImplausibleDeadline);
    }
    if resolved_uid != Some(record.uid) {
        return Err(TokenRejection::AccountChanged);
    }
    Ok(())
}

/// Whether the record directory is `trusted_owner`'s alone (`0700`; the
/// owner is root on a host, the test user in tests).
#[must_use]
pub const fn record_dir_trustworthy(
    is_dir: bool,
    owner: u32,
    trusted_owner: u32,
    mode: u32,
) -> bool {
    is_dir && owner == trusted_owner && mode.trailing_zeros() >= 6
}

/// Whether a record file may be believed: a regular file,
/// `trusted_owner`'s alone (root on a host), one link, no larger than
/// [`MAX_RECORD_BYTES`].
#[must_use]
pub const fn record_file_trustworthy(
    regular: bool,
    owner: u32,
    trusted_owner: u32,
    mode: u32,
    links: u64,
    size: u64,
) -> bool {
    regular
        && owner == trusted_owner
        && mode.trailing_zeros() >= 6
        && links == 1
        && size <= MAX_RECORD_BYTES as u64
}

/// The PAM module file name Arcen installs for [`PAM_SERVICE`].
pub const PAM_MODULE: &str = "pam_arcen_greeter.so";

/// Arcen's stack for [`PAM_SERVICE`], derived from the host's own password
/// sign-in stack (`/etc/pam.d/gdm-password`): its `account` and `session`
/// lines exactly as they are, so a remote-first session is checked and set
/// up like a password one (account policy, `pam_systemd`, `SELinux`,
/// namespaces); Arcen's token module as the only `auth`; no password
/// changes. `None` when the host stack has no `account` or no `session`
/// line to copy.
#[must_use]
pub fn pam_stack(gdm_password: &str) -> Option<String> {
    let kept: Vec<&str> = gdm_password
        .lines()
        .map(str::trim)
        .filter(|line| {
            let kind = line.split_whitespace().next().unwrap_or_default();
            matches!(kind.trim_start_matches('-'), "account" | "session")
        })
        .collect();
    let has = |kind: &str| {
        kept.iter()
            .any(|line| line.trim_start_matches('-').starts_with(kind))
    };
    if !has("account") || !has("session") {
        return None;
    }
    let mut stack = format!(
        "{PAM_STACK_MARKER}\n\
         # Written by the Arcen Pier installer (ADR 0025). GNOME's login screen runs\n\
         # this stack only when the Pier hands it a one-time sign-in token for a user\n\
         # whose password the Pier has just checked. account and session lines are\n\
         # the host's gdm-password ones. Removed by the uninstaller.\n\
         auth     required    {PAM_MODULE}\n"
    );
    for line in kept {
        stack.push_str(line);
        stack.push('\n');
    }
    stack.push_str("password required    pam_deny.so\n");
    Some(stack)
}

/// The system-bus policy Arcen installs so root may own [`BUS_NAME`]
/// (and nobody else gains anything).
#[must_use]
pub fn bus_policy() -> String {
    format!(
        "<!-- {PAM_STACK_MARKER}: written by the Arcen Pier installer (ADR 0025). -->\n\
         <!DOCTYPE busconfig PUBLIC \"-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN\"\n \
         \"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd\">\n\
         <busconfig>\n  <policy user=\"root\">\n    <allow own=\"{BUS_NAME}\"/>\n  </policy>\n</busconfig>\n"
    )
}

/// Where the installer writes [`bus_policy`].
pub const BUS_POLICY_PATH: &str = "/etc/dbus-1/system.d/arcen-greeter-login.conf";

/// Whether a bus policy file is Arcen's own.
#[must_use]
pub fn bus_policy_is_arcen(text: &str) -> bool {
    text.lines()
        .next()
        .is_some_and(|line| line.contains(PAM_STACK_MARKER))
}

/// Whether the PAM stack text for [`PAM_SERVICE`] is Arcen's own.
#[must_use]
pub fn pam_stack_is_arcen(text: &str) -> bool {
    text.lines().next() == Some(PAM_STACK_MARKER)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;
    const TOKEN: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";

    #[test]
    fn a_token_is_found_by_its_hash_and_nothing_else_is_looked_up() {
        let name = record_name(TOKEN).expect("token");
        assert_eq!(name.len(), 64);
        assert_ne!(name, TOKEN, "the token itself never names a file");
        assert_eq!(record_name(TOKEN), Some(name));
        for bad in [
            "",
            &TOKEN[1..],
            &TOKEN.to_uppercase(),
            &format!("{}/", &TOKEN[..63]),
            "../../etc/shadow",
        ] {
            assert_eq!(record_name(bad), None, "{bad}");
        }
        assert_eq!(token_text(&[0xab; TOKEN_BYTES]).len(), TOKEN_HEX_LEN);
        assert!(record_name(&token_text(&[0xab; TOKEN_BYTES])).is_some());
    }

    #[test]
    fn records_round_trip_and_anything_else_is_no_record() {
        let record = GreeterTokenRecord::issue("tim", 1001, NOW).expect("record");
        assert_eq!(record.not_after, NOW + LIFETIME_SECS);
        assert_eq!(
            GreeterTokenRecord::parse(record.encode().as_bytes()),
            Some(record.clone())
        );
        let encoded = record.encode();
        for bad in [
            encoded.replace("v1", "v2"),
            encoded.replace("user=tim", "user=tim\nuser=root"),
            encoded.replace("uid=1001", "uid=-1"),
            encoded.trim_end().to_owned(),
            format!("{encoded}extra\n"),
            encoded.replace("user=tim", "user=a b"),
        ] {
            assert_eq!(GreeterTokenRecord::parse(bad.as_bytes()), None, "{bad:?}");
        }
        assert_eq!(
            GreeterTokenRecord::parse(&[b'x'; MAX_RECORD_BYTES + 1]),
            None
        );
        assert_eq!(GreeterTokenRecord::issue("root", 0, NOW), None);
        assert_eq!(GreeterTokenRecord::issue("../x", 1001, NOW), None);
        assert!(GreeterTokenRecord::issue("tim.d@example", 88_601_122, NOW).is_some());
    }

    #[test]
    fn a_record_admits_its_user_once_within_its_lifetime_only() {
        let record = GreeterTokenRecord::issue("tim", 1001, NOW).expect("record");
        assert_eq!(admit(&record, NOW + 5, Some(1001)), Ok(()));
        assert_eq!(
            admit(&record, NOW + LIFETIME_SECS + 1, Some(1001)),
            Err(TokenRejection::Expired)
        );
        assert_eq!(
            admit(&record, NOW, Some(1002)),
            Err(TokenRejection::AccountChanged)
        );
        assert_eq!(
            admit(&record, NOW, None),
            Err(TokenRejection::AccountChanged)
        );
        assert_eq!(admit(&record, NOW, Some(0)), Err(TokenRejection::Root));
        let far = GreeterTokenRecord {
            not_after: NOW + 3600,
            ..record
        };
        assert_eq!(
            admit(&far, NOW, Some(1001)),
            Err(TokenRejection::ImplausibleDeadline)
        );
    }

    #[test]
    fn only_root_owned_private_records_are_believed() {
        assert!(record_dir_trustworthy(true, 0, 0, 0o700));
        assert!(!record_dir_trustworthy(true, 0, 0, 0o711));
        assert!(!record_dir_trustworthy(true, 42, 0, 0o700));
        assert!(!record_dir_trustworthy(false, 0, 0, 0o700));
        assert!(record_file_trustworthy(true, 0, 0, 0o600, 1, 80));
        assert!(!record_file_trustworthy(true, 0, 0, 0o640, 1, 80));
        assert!(!record_file_trustworthy(true, 1001, 0, 0o600, 1, 80));
        assert!(!record_file_trustworthy(true, 0, 0, 0o600, 2, 80));
        assert!(!record_file_trustworthy(true, 0, 0, 0o600, 1, 4096));
        assert!(!record_file_trustworthy(false, 0, 0, 0o600, 1, 80));
    }

    #[test]
    fn the_stack_keeps_the_hosts_account_and_session_and_only_arcens_auth() {
        let gdm_password = "auth     [success=done ignore=ignore default=bad] pam_selinux_permit.so\n\
            auth        substack      password-auth\n\
            auth        optional      pam_gnome_keyring.so\n\
            \n\
            account     required      pam_nologin.so\n\
            account     include       password-auth\n\
            password    substack       password-auth\n\
            -password   optional       pam_gnome_keyring.so use_authtok\n\
            session     required      pam_selinux.so close\n\
            session     include       password-auth\n\
            -session    optional      pam_ck_connector.so\n";
        let stack = pam_stack(gdm_password).expect("stack");
        assert!(pam_stack_is_arcen(&stack));
        let auth: Vec<_> = stack.lines().filter(|l| l.starts_with("auth")).collect();
        assert_eq!(auth, ["auth     required    pam_arcen_greeter.so"]);
        assert!(stack.contains("account     include       password-auth"));
        assert!(stack.contains("session     include       password-auth"));
        assert!(stack.contains("-session    optional      pam_ck_connector.so"));
        assert!(!stack.contains("substack      password-auth\nauth"));
        assert!(!stack.contains("use_authtok"));
        assert!(stack.ends_with("password required    pam_deny.so\n"));
        assert_eq!(pam_stack("auth required pam_unix.so\n"), None);
        assert!(bus_policy_is_arcen(&bus_policy()));
        assert!(bus_policy().contains(BUS_NAME));
        assert!(!bus_policy_is_arcen("<busconfig/>"));
    }

    #[test]
    fn only_arcens_own_pam_stack_is_used() {
        assert!(pam_stack_is_arcen(&format!(
            "{PAM_STACK_MARKER}\nauth required x\n"
        )));
        assert!(!pam_stack_is_arcen("auth required pam_ovirt_cred.so\n"));
    }
}
