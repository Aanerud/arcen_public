//! The session's keyring, unlocked with the login password.
//!
//! A GDM login unlocks the user's login keyring through
//! `pam_gnome_keyring`. That module finds the daemon under the PAM
//! environment's `XDG_RUNTIME_DIR` (`/run/user/<uid>`), which is the user's
//! systemd bus, not the private bus a headless Arcen desktop's applications
//! use. Instead, the agent starts `gnome-keyring-daemon --unlock` itself on
//! the private bus, before the compositor, and writes the password the
//! launcher handed it to the daemon's standard input: never an argument,
//! an environment variable or a file. The daemon unlocks the login keyring
//! (creating it with that password if there is none), exactly as it does
//! for `pam_gnome_keyring`. Every failure is logged and the desktop keeps
//! running: applications then ask for the keyring password as before.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;
use zeroize::Zeroizing;

use super::stack::MemberEnvironment;
use crate::logging::target;

/// The keyring daemon.
pub const KEYRING_DAEMON: &str = "/usr/bin/gnome-keyring-daemon";
/// Longest password handed on (the daemon reads at most 8 KiB).
pub const MAX_PASSWORD_BYTES: usize = 8192;
const READY_TIMEOUT: Duration = Duration::from_secs(5);
const LOCKED_TIMEOUT: Duration = Duration::from_secs(5);
const SECRETS_NAME: &str = "org.freedesktop.secrets";
const LOGIN_COLLECTION: &str = "/org/freedesktop/secrets/collection/login";

/// Variables the daemon announces, accepted only as paths inside the
/// private runtime directory.
const ANNOUNCED: [&str; 2] = ["GNOME_KEYRING_CONTROL", "SSH_AUTH_SOCK"];

/// A started keyring daemon and the variables applications need.
#[derive(Debug)]
pub struct StartedKeyring {
    /// The daemon (an optional stack member).
    pub child: Child,
    /// `GNOME_KEYRING_CONTROL`, `SSH_AUTH_SOCK`.
    pub environment: Vec<(String, String)>,
}

/// Starts the keyring daemon with `components`, unlocking the login
/// keyring with `password` when one is given.
///
/// # Errors
///
/// A reason token when the daemon is missing or did not start; the caller
/// logs it and runs without a keyring of its own.
pub async fn start(
    members: &MemberEnvironment,
    components: &[String],
    password: Option<Zeroizing<Vec<u8>>>,
) -> Result<StartedKeyring, &'static str> {
    if components.is_empty() {
        return Err("no keyring component is enabled");
    }
    if !Path::new(KEYRING_DAEMON).is_file() {
        return Err("gnome-keyring-daemon is not installed");
    }
    let runtime_dir = members.runtime_dir().to_path_buf();
    let control = runtime_dir.join("keyring");
    let mut command = members.command(KEYRING_DAEMON);
    command
        .arg("--foreground")
        .arg(format!("--components={}", components.join(",")))
        .arg("--control-directory")
        .arg(&control);
    if password.is_some() {
        command.arg("--unlock");
    }
    command
        .stdin(if password.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|_| "the keyring daemon did not start")?;
    if let Some(password) = password {
        let mut stdin = child.stdin.take().ok_or("no keyring stdin")?;
        let written = tokio::time::timeout(READY_TIMEOUT, async {
            stdin.write_all(&password).await?;
            stdin.shutdown().await
        })
        .await;
        drop(stdin);
        drop(password);
        if !matches!(written, Ok(Ok(()))) {
            let _ = child.start_kill();
            return Err("the password could not be handed to the keyring daemon");
        }
    }
    let stdout = child.stdout.take().ok_or("no keyring stdout")?;
    // The daemon prints its variables first and closes its standard output
    // only once it is initialised (its bus names owned), so reading to end
    // of file keeps any application from activating a second daemon.
    let mut announced = Vec::new();
    let initialised = tokio::time::timeout(READY_TIMEOUT, async {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(pair) = accepted_variable(&line, &runtime_dir) {
                if announced.len() < ANNOUNCED.len() {
                    announced.push(pair);
                }
            }
        }
    })
    .await
    .is_ok();
    if !initialised {
        tracing::warn!(target: target::SESSION, "the keyring daemon did not finish initialising in time");
    }
    if !matches!(child.try_wait(), Ok(None)) {
        return Err("the keyring daemon exited during startup");
    }
    Ok(StartedKeyring {
        child,
        environment: announced,
    })
}

/// `NAME=value` from the daemon's output, when `NAME` is one it announces
/// and the value is a path inside `runtime_dir`.
fn accepted_variable(line: &str, runtime_dir: &Path) -> Option<(String, String)> {
    let (name, value) = line.trim().split_once('=')?;
    let path = Path::new(value);
    (ANNOUNCED.contains(&name)
        && path.is_absolute()
        && path.starts_with(runtime_dir)
        && !value.contains("..")
        && !value.chars().any(char::is_control))
    .then(|| (name.to_owned(), value.to_owned()))
}

/// Logs whether the login keyring ended up unlocked (bounded; never fatal).
pub async fn report_login_keyring(bus_address: String, unlock_attempted: bool) {
    let locked = tokio::time::timeout(LOCKED_TIMEOUT, async {
        let connection = zbus::connection::Builder::address(bus_address.as_str())
            .ok()?
            .build()
            .await
            .ok()?;
        for _ in 0..50 {
            let proxy = zbus::Proxy::new(
                &connection,
                SECRETS_NAME,
                LOGIN_COLLECTION,
                "org.freedesktop.Secret.Collection",
            )
            .await
            .ok()?;
            if let Ok(locked) = proxy.get_property::<bool>("Locked").await {
                return Some(locked);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        None
    })
    .await
    .ok()
    .flatten();
    match (locked, unlock_attempted) {
        (Some(false), _) => {
            tracing::info!(target: target::SESSION, "keyring unlocked: the login keyring opened with the login password")
        }
        (Some(true), true) => {
            tracing::warn!(target: target::SESSION, "keyring locked: the login password does not open the login keyring; applications will ask for the keyring password")
        }
        (Some(true), false) => {
            tracing::info!(target: target::SESSION, "keyring locked: no login password was handed over")
        }
        (None, _) => {
            tracing::info!(target: target::SESSION, "keyring state unknown: the login collection did not answer")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_announced_paths_inside_the_runtime_directory_are_taken() {
        let runtime = Path::new("/run/user/1000/arcen-wayland-7");
        assert_eq!(
            accepted_variable(
                "GNOME_KEYRING_CONTROL=/run/user/1000/arcen-wayland-7/keyring",
                runtime
            ),
            Some((
                "GNOME_KEYRING_CONTROL".to_owned(),
                "/run/user/1000/arcen-wayland-7/keyring".to_owned()
            ))
        );
        assert!(accepted_variable(
            "SSH_AUTH_SOCK=/run/user/1000/arcen-wayland-7/keyring/ssh",
            runtime
        )
        .is_some());
        assert_eq!(accepted_variable("SSH_AUTH_SOCK=/tmp/agent", runtime), None);
        assert_eq!(
            accepted_variable("SSH_AUTH_SOCK=/run/user/1000/arcen-wayland-7/../x", runtime),
            None
        );
        assert_eq!(
            accepted_variable("LD_PRELOAD=/run/user/1000/arcen-wayland-7/x.so", runtime),
            None
        );
    }
}
