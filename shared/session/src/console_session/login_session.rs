// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The real login session Arcen starts on a host without a display manager
//! (ADR 0025 §10).
//!
//! A host with no display manager has no login screen to complete a
//! remote-first sign-in, so Arcen does what a display manager does: it
//! opens a PAM session for the account (its own service, [`PAM_SERVICE`],
//! which never authenticates: the Pier proved the password with its own
//! service first), on `seat0` and a virtual terminal of its own, starts an
//! X server there and runs the account's desktop session from its
//! `/usr/share/xsessions` entry through the system's `Xsession`, with the
//! session manager, settings daemons and autostart a sign-in at a desk
//! gets. The session belongs to logind and lives on its own: a Deck joins
//! it as a console session, leaving locks it, and only the user's logout
//! (or an administrator) ends it. One per account; another account's
//! sign-in is refused while it exists.
//!
//! This module holds what needs no operating system: the PAM stack, the
//! choice of virtual terminal, the session entry's command and the
//! environment the session starts with.

/// Arcen's PAM service for the sessions it starts.
pub const PAM_SERVICE: &str = "arcen-login";
/// First line of the stack Arcen installs for [`PAM_SERVICE`]; a stack
/// without it is not Arcen's and is never replaced or used.
pub const PAM_STACK_MARKER: &str = "# arcen-login v1";
/// Where the system keeps its X session entries.
pub const XSESSIONS_DIR: &str = "/usr/share/xsessions";
/// The system's X session wrappers, preferred first (Enterprise Linux and
/// Fedora; Debian and Ubuntu): xinitrc.d or Xsession.d set up the message
/// bus, the user manager's environment and `xhost` for the user.
pub const XSESSION_WRAPPERS: [&str; 2] = ["/etc/X11/xinit/Xsession", "/etc/X11/Xsession"];

/// The stack for [`PAM_SERVICE`]: authentication always refused (the
/// service only opens sessions for an account the Pier authenticated), the
/// host's own `login` account and session lines, password changes refused.
/// `None` when `login` has no account or session lines to take.
///
/// Opening a session establishes credentials first (`pam_setcred`), which
/// runs the `auth` lines too, and `pam_deny` refuses that as well. So
/// `auth` is a `requisite` condition that is false for this very service
/// (`pam_succeed_if service notin arcen-login`: authentication stops there,
/// refused) and has no say in credentials (`pam_succeed_if` ignores
/// `setcred`), followed by `pam_permit` for credentials only, which
/// authentication never reaches.
#[must_use]
pub fn pam_stack(login: &str) -> Option<String> {
    // Debian's stacks bring theirs in with `@include common-account` and
    // `@include common-session`.
    let kind_of = |line: &str| -> Option<&'static str> {
        let mut words = line.split_whitespace();
        match words.next()?.trim_start_matches('-') {
            "account" => Some("account"),
            "session" => Some("session"),
            "@include" => match words.next()? {
                "common-account" => Some("account"),
                "common-session" | "common-session-noninteractive" => Some("session"),
                _ => None,
            },
            _ => None,
        }
    };
    let kept: Vec<&str> = login
        .lines()
        .map(str::trim)
        .filter(|line| kind_of(line).is_some())
        .collect();
    let has = |kind: &str| kept.iter().any(|line| kind_of(line) == Some(kind));
    if !has("account") || !has("session") {
        return None;
    }
    let mut stack = format!(
        "{PAM_STACK_MARKER}\n\
         # Written by the Arcen Pier installer (ADR 0025). The Pier opens a session\n\
         # with this service only for an account whose password its own service has\n\
         # just checked, on a host without a display manager. It authenticates\n\
         # nobody. account and session lines are the host's login ones. Removed by\n\
         # the uninstaller.\n\
         auth     requisite   pam_succeed_if.so quiet service notin {PAM_SERVICE}\n\
         auth     optional    pam_permit.so\n"
    );
    for line in kept {
        stack.push_str(line);
        stack.push('\n');
    }
    stack.push_str("password required    pam_deny.so\n");
    Some(stack)
}

/// Whether a stack is Arcen's.
#[must_use]
pub fn pam_stack_is_arcen(text: &str) -> bool {
    text.lines().next() == Some(PAM_STACK_MARKER)
}

/// The virtual terminal a new session takes: the first of 7–12, then 2–6,
/// that no session on the seat uses (VT 1 is the text console's and a
/// display manager's). `None` when every one is taken.
#[must_use]
pub fn choose_vt(used: &[u32]) -> Option<u32> {
    (7..=12).chain(2..=6).find(|vt| !used.contains(vt))
}

/// The session entries a desktop id may use, preferred first: its X11
/// variant (`gnome-xorg` for `gnome`), then the entry of that name. An id
/// that is not a plain name has none.
#[must_use]
pub fn xsession_names(desktop: &str) -> Vec<String> {
    if super::account_desktop::DesktopId::parse(desktop).is_none() {
        return Vec::new();
    }
    vec![
        format!("{desktop}-xorg.desktop"),
        format!("{desktop}.desktop"),
    ]
}

/// The desktop names an X session entry declares (`DesktopNames=`), as
/// `XDG_CURRENT_DESKTOP` wants them (`GNOME-Classic:GNOME`); `None` when
/// it declares none or a name that is not plain.
#[must_use]
pub fn xsession_desktop_names(entry: &str) -> Option<String> {
    let mut in_entry = false;
    let mut names = None;
    for line in entry.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if in_entry {
            if let Some(value) = line.strip_prefix("DesktopNames=") {
                names = Some(value.trim());
            }
        }
    }
    let parts: Vec<&str> = names?.split(';').filter(|part| !part.is_empty()).collect();
    let plain = !parts.is_empty()
        && parts.iter().all(|part| {
            part.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        });
    plain.then(|| parts.join(":"))
}

/// The command of an X session entry (`[Desktop Entry] Exec=`), as plain
/// words: an entry whose command needs a shell (quotes, variables,
/// redirection, field codes) or is empty is refused.
#[must_use]
pub fn xsession_exec(entry: &str) -> Option<Vec<String>> {
    let mut in_entry = false;
    let mut exec = None;
    for line in entry.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if in_entry {
            if let Some(value) = line.strip_prefix("Exec=") {
                exec = Some(value.trim());
            }
        }
    }
    let exec = exec?;
    if exec.is_empty()
        || exec.chars().any(|c| {
            c.is_control()
                || matches!(
                    c,
                    ';' | '|'
                        | '&'
                        | '$'
                        | '`'
                        | '<'
                        | '>'
                        | '('
                        | ')'
                        | '{'
                        | '}'
                        | '*'
                        | '?'
                        | '"'
                        | '\''
                        | '\\'
                        | '%'
                        | '~'
                        | '#'
                )
        })
    {
        return None;
    }
    let words: Vec<String> = exec.split_whitespace().map(str::to_owned).collect();
    (!words.is_empty()).then_some(words)
}

/// A free X display number (`:N`) for a new session: the first from 1 not
/// in `taken` (sockets and locks that exist) and not `reserved` (the
/// Pier's own desktops).
#[must_use]
pub fn choose_display(taken: &[u32], reserved: &[u32]) -> Option<u32> {
    (1..=63).find(|n| !taken.contains(n) && !reserved.contains(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOGIN: &str = "#%PAM-1.0\n\
        auth       substack     system-auth\n\
        auth       include      postlogin\n\
        account    required     pam_nologin.so\n\
        account    include      system-auth\n\
        password   include      system-auth\n\
        session    required     pam_selinux.so close\n\
        session    required     pam_loginuid.so\n\
        session    required     pam_selinux.so open\n\
        session    optional     pam_keyinit.so force revoke\n\
        session    include      system-auth\n\
        -session   optional     pam_ck_connector.so\n";

    #[test]
    fn the_stack_never_authenticates_and_keeps_the_hosts_account_and_session_lines() {
        let stack = pam_stack(LOGIN).expect("stack");
        assert!(pam_stack_is_arcen(&stack));
        let auth: Vec<&str> = stack.lines().filter(|l| l.starts_with("auth")).collect();
        assert_eq!(
            auth,
            [
                "auth     requisite   pam_succeed_if.so quiet service notin arcen-login",
                "auth     optional    pam_permit.so"
            ],
            "authentication stops at a condition false for this service; credentials pass"
        );
        assert!(stack.contains("account    required     pam_nologin.so\n"));
        assert!(stack.contains("session    include      system-auth\n"));
        assert!(stack.contains("-session   optional     pam_ck_connector.so\n"));
        assert!(!stack.contains("substack"));
        assert!(stack.ends_with("password required    pam_deny.so\n"));
        assert_eq!(pam_stack("auth include system-auth\n"), None);
        assert!(!pam_stack_is_arcen(LOGIN));
    }

    #[test]
    fn debian_includes_are_kept_as_account_and_session() {
        let debian = "@include common-auth\n@include common-account\n\
                      session optional pam_motd.so\n@include common-session\n@include common-password\n";
        let stack = pam_stack(debian).expect("stack");
        assert!(stack.contains("@include common-account\n"));
        assert!(stack.contains("@include common-session\n"));
        assert!(!stack.contains("common-auth") && !stack.contains("common-password"));
    }

    #[test]
    fn a_free_vt_and_display_are_chosen() {
        assert_eq!(choose_vt(&[]), Some(7));
        assert_eq!(choose_vt(&[1, 7]), Some(8));
        assert_eq!(choose_vt(&(7..=12).collect::<Vec<_>>()), Some(2));
        assert_eq!(choose_vt(&(1..=12).collect::<Vec<_>>()), None);
        assert_eq!(choose_display(&[0, 1], &[10]), Some(2));
        assert_eq!(choose_display(&(1..=63).collect::<Vec<_>>(), &[]), None);
    }

    #[test]
    fn only_a_plain_session_command_is_run() {
        let entry = "[Desktop Entry]\nName=Standard (X11 display server)\nExec=gnome-session\n\
                     TryExec=gnome-session\nType=Application\n[Desktop Action x]\nExec=evil\n";
        assert_eq!(xsession_exec(entry), Some(vec!["gnome-session".to_owned()]));
        assert_eq!(
            xsession_exec(
                "[Desktop Entry]\nExec=env GNOME_SHELL_SESSION_MODE=classic gnome-session\n"
            ),
            Some(
                ["env", "GNOME_SHELL_SESSION_MODE=classic", "gnome-session"]
                    .map(str::to_owned)
                    .to_vec()
            )
        );
        for bad in [
            "[Desktop Entry]\nExec=sh -c 'x; y'\n",
            "[Desktop Entry]\nExec=$HOME/x\n",
            "[Desktop Entry]\nExec=\n",
            "[Desktop Entry]\nExec=a > b\n",
            "[Desktop Entry]\nExec=prog %U\n",
            "[Other]\nExec=gnome-session\n",
        ] {
            assert_eq!(xsession_exec(bad), None, "{bad}");
        }
        assert_eq!(
            xsession_names("gnome"),
            ["gnome-xorg.desktop", "gnome.desktop"]
        );
        assert_eq!(
            xsession_names("gnome-classic"),
            ["gnome-classic-xorg.desktop", "gnome-classic.desktop"]
        );
        assert_eq!(
            xsession_names("gnome-custom-session"),
            [
                "gnome-custom-session-xorg.desktop",
                "gnome-custom-session.desktop"
            ]
        );
        assert!(xsession_names("../x").is_empty());
    }

    #[test]
    fn an_entrys_desktop_names_become_the_current_desktop() {
        assert_eq!(
            xsession_desktop_names(
                "[Desktop Entry]\nName=Classic\nDesktopNames=GNOME-Classic;GNOME;\n"
            )
            .as_deref(),
            Some("GNOME-Classic:GNOME")
        );
        assert_eq!(xsession_desktop_names("[Desktop Entry]\nExec=x\n"), None);
        assert_eq!(
            xsession_desktop_names("[Desktop Entry]\nDesktopNames=a b;\n"),
            None
        );
        assert_eq!(
            xsession_desktop_names("[Other]\nDesktopNames=GNOME\n"),
            None
        );
    }
}
