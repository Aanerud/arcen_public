//! The user's login-shell environment for a desktop session.
//!
//! A GDM login starts GNOME through the user's login shell, so `~/.profile`
//! (a `PATH` addition, a licence-server variable) reaches every
//! application. An Arcen desktop has no display manager; its session agent,
//! already running as the session user, runs that login shell once, reads
//! `env -0` and keeps what the shared filter
//! ([`arcen_session::desktop_session::login_environment`]) allows. Never as
//! root, bounded in time and size, and never fatal: a shell that fails,
//! hangs or prints nothing leaves the session as it was, with one log line.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use arcen_session::desktop_session::login_environment::{
    filter_login_environment, LoginEnvironment, MAX_DUMP_BYTES,
};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::logging::target;

/// How long the login shell may take in a headless Wayland desktop (whose
/// start budget is 40 s); the X11 agent passes a shorter bound.
pub const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(5);
const ENV: &str = "/usr/bin/env";
const SHELLS: &str = "/etc/shells";

/// Runs the session user's login shell and returns the variables to add.
/// Logs the outcome (names only, never values). Must run as the session
/// user; refuses as root.
pub async fn import_login_environment(timeout: Duration) -> LoginEnvironment {
    match run_login_shell(timeout).await {
        Ok(dump) => {
            let environment = filter_login_environment(&dump, |name| std::env::var(name).ok());
            tracing::info!(
                target: target::SESSION,
                imported = environment.imported.len(),
                names = ?environment.imported_names(),
                denied = environment.dropped.len(),
                "login shell environment imported"
            );
            environment
        }
        Err(reason) => {
            tracing::warn!(
                target: target::SESSION,
                reason,
                "login shell environment not imported; the session keeps its own"
            );
            LoginEnvironment::default()
        }
    }
}

async fn run_login_shell(timeout: Duration) -> Result<Vec<u8>, &'static str> {
    let uid = nix::unistd::Uid::current();
    if uid.is_root() || nix::unistd::Uid::effective().is_root() {
        return Err("refused as root");
    }
    let user = nix::unistd::User::from_uid(uid)
        .ok()
        .flatten()
        .ok_or("no passwd entry")?;
    let shell = user.shell;
    let listed = std::fs::read_to_string(SHELLS).unwrap_or_default();
    if !login_shell_usable(&shell, &listed) {
        return Err("no usable login shell");
    }
    let mut command = Command::new(&shell);
    command
        .args(["-l", "-c", &format!("exec {ENV} -0")])
        .current_dir(&user.dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    {
        use std::os::unix::process::CommandExt;
        // Its own process group, so a profile that starts background jobs
        // can be stopped as a whole on timeout.
        command.as_std_mut().process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|_| "the login shell did not start")?;
    let pgid = child.id().and_then(|pid| i32::try_from(pid).ok());
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let read = async {
        let mut dump = Vec::new();
        let mut limited = (&mut stdout).take(u64::try_from(MAX_DUMP_BYTES + 1).unwrap_or(u64::MAX));
        limited
            .read_to_end(&mut dump)
            .await
            .map_err(|_| "reading the login shell failed")?;
        let status = child
            .wait()
            .await
            .map_err(|_| "waiting for the login shell failed")?;
        if status.success() {
            Ok(dump)
        } else {
            Err("the login shell failed")
        }
    };
    let result = tokio::time::timeout(timeout, read)
        .await
        .unwrap_or(Err("the login shell timed out"));
    if let Some(pgid) = pgid {
        // The shell itself has exited or is killed on drop; anything its
        // profile left behind in the group goes too.
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pgid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    result
}

/// Whether `shell` may run a login: listed in `/etc/shells` and not a
/// no-login shell, the test `gnome-session` applies before it re-execs
/// through the login shell.
fn login_shell_usable(shell: &Path, shells_file: &str) -> bool {
    let text = shell.to_string_lossy();
    shell.is_absolute()
        && !text.contains("nologin")
        && !text.contains("false")
        && shells_file
            .lines()
            .map(str::trim)
            .any(|line| !line.starts_with('#') && line == text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_listed_login_shells_are_run() {
        let shells = "# /etc/shells\n/bin/sh\n/bin/bash\n/usr/bin/zsh\n/sbin/nologin\n";
        assert!(login_shell_usable(Path::new("/bin/bash"), shells));
        assert!(login_shell_usable(Path::new("/usr/bin/zsh"), shells));
        assert!(!login_shell_usable(Path::new("/sbin/nologin"), shells));
        assert!(!login_shell_usable(Path::new("/bin/false"), "/bin/false\n"));
        assert!(!login_shell_usable(Path::new("/opt/fish"), shells));
        assert!(!login_shell_usable(Path::new("bash"), "bash\n"));
    }
}
