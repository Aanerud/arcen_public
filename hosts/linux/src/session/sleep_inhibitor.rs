//! A logind `sleep:idle` block inhibitor held for the life of a desktop.
//!
//! A host that serves a remote desktop must never suspend under it: not on
//! logind's `IdleAction`, not because a settings daemon of some other
//! session (the GDM greeter on the console seat) decides the machine is
//! idle, and not because an application asks. The privileged launcher, which
//! owns the desktop's PAM and logind session, therefore holds a logind
//! `block` inhibitor for `sleep:idle` from the moment the desktop's session
//! exists until it is torn down, on both desktop providers.
//!
//! It is `systemd-inhibit … /usr/bin/cat` with `cat` reading a pipe only the
//! launcher holds: dropping the guard closes the pipe, `cat` sees end of
//! file and exits, and `systemd-inhibit` releases the inhibitor. A launcher
//! that dies closes the pipe the same way, so the inhibitor can never
//! outlive the desktop. A physical lid close still suspends (logind's
//! default `LidSwitchIgnoreInhibited=yes`).

use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, ChildStdin, Command};

use crate::logging::target;

const SYSTEMD_INHIBIT: &str = "/usr/bin/systemd-inhibit";
const CAT: &str = "/usr/bin/cat";
const EARLY_EXIT_CHECK: Duration = Duration::from_millis(300);

/// The held inhibitor; released on drop.
#[derive(Debug)]
pub struct SleepInhibitor {
    child: Child,
    stdin: Option<ChildStdin>,
    session_id: String,
}

/// The `systemd-inhibit` arguments for `session_id`.
#[must_use]
pub fn inhibit_arguments(session_id: &str) -> Vec<String> {
    vec![
        "--what=sleep:idle".to_owned(),
        "--who=Arcen".to_owned(),
        format!("--why=Arcen remote desktop session {session_id} is open"),
        "--mode=block".to_owned(),
        CAT.to_owned(),
    ]
}

impl SleepInhibitor {
    /// Takes the inhibitor for the desktop of logind session `session_id`.
    /// `None` (logged) when it could not be taken; never fatal.
    pub async fn acquire(session_id: &str) -> Option<Self> {
        let mut command = Command::new(SYSTEMD_INHIBIT);
        command
            .args(inhibit_arguments(session_id))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                tracing::warn!(target: target::SESSION, %error, session_id, "the host-sleep inhibitor could not be taken");
                return None;
            }
        };
        let stdin = child.stdin.take();
        tokio::time::sleep(EARLY_EXIT_CHECK).await;
        if let Ok(Some(status)) = child.try_wait() {
            tracing::warn!(target: target::SESSION, %status, session_id, "the host-sleep inhibitor was refused");
            return None;
        }
        tracing::info!(
            target: target::SESSION,
            session_id,
            what = "sleep:idle",
            mode = "block",
            "host sleep inhibited while the desktop exists"
        );
        Some(Self {
            child,
            stdin,
            session_id: session_id.to_owned(),
        })
    }
}

impl Drop for SleepInhibitor {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.start_kill();
        tracing::info!(target: target::SESSION, session_id = %self.session_id, "host-sleep inhibitor released");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_inhibitor_blocks_sleep_and_idle_for_the_named_session() {
        assert_eq!(
            inhibit_arguments("42"),
            [
                "--what=sleep:idle",
                "--who=Arcen",
                "--why=Arcen remote desktop session 42 is open",
                "--mode=block",
                "/usr/bin/cat"
            ]
        );
    }
}
