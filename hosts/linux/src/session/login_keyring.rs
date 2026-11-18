// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The login keyring of a console sign-in Arcen completed without the
//! password reaching `pam_gnome_keyring` (GDM's login screen taking the
//! one-time token, or Arcen's own login session on a host without a
//! display manager): unlocked with the password the Pier just proved.
//!
//! The launcher keeps a copy of the password only for this, and hands it
//! to a short-lived helper (`session-agent --login-keyring-unlock`) running
//! as the session user on its standard input: never an argument, an
//! environment variable or a file. The helper waits (bounded) for the
//! user's keyring daemon, checks it runs as the same user, sends the one
//! `UNLOCK` request of [`keyring_control`] and exits. Every failure is
//! logged and changes nothing else: applications then ask for the keyring
//! password as before.

use std::process::Stdio;
use std::time::Duration;

use arcen_session::desktop_session::keyring_control;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use zeroize::Zeroizing;

use crate::logging::target;

/// How long the helper waits for the user's keyring daemon (started by
/// `pam_gnome_keyring` with the session, or by the desktop's autostart).
const DAEMON_WAIT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(250);
const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the launcher waits for the helper.
const HELPER_TIMEOUT: Duration = Duration::from_secs(45);

/// The helper, as the session user.
#[cfg(target_os = "linux")]
pub async fn run_unlock() -> Result<(), String> {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
    let uid = nix::unistd::Uid::effective();
    if uid.is_root() {
        return Err("the keyring unlock refuses to run as root".to_owned());
    }
    let mut password = Zeroizing::new(Vec::with_capacity(keyring_control::MAX_PASSWORD_BYTES + 1));
    tokio::io::stdin()
        .take(keyring_control::MAX_PASSWORD_BYTES as u64 + 1)
        .read_to_end(&mut password)
        .await
        .map_err(|_| "no password reached the keyring unlock".to_owned())?;
    let request = keyring_control::unlock_request(&password)
        .map(Zeroizing::new)
        .ok_or_else(|| "the password cannot be handed to the keyring".to_owned())?;
    drop(password);
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| "the keyring unlock requires XDG_RUNTIME_DIR".to_owned())?;
    let socket = runtime
        .join(keyring_control::CONTROL_DIRECTORY)
        .join(keyring_control::CONTROL_SOCKET);
    let deadline = tokio::time::Instant::now() + DAEMON_WAIT;
    let stream = loop {
        let owned_socket = std::fs::symlink_metadata(&socket).is_ok_and(|metadata| {
            metadata.file_type().is_socket() && metadata.uid() == uid.as_raw()
        });
        if owned_socket {
            if let Ok(stream) = tokio::net::UnixStream::connect(&socket).await {
                break stream;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("no keyring daemon of the user answered".to_owned());
        }
        tokio::time::sleep(POLL).await;
    };
    // The daemon must be the user's own, as pam_gnome_keyring checks.
    let peer = stream
        .peer_cred()
        .map_err(|_| "the keyring daemon's credentials are unknown".to_owned())?;
    if peer.uid() != uid.as_raw() {
        return Err("the keyring socket is served by another user".to_owned());
    }
    let mut stream = stream;
    let answer = tokio::time::timeout(ANSWER_TIMEOUT, async {
        stream
            .write_all(&[keyring_control::CREDENTIALS_BYTE])
            .await?;
        stream.write_all(&request).await?;
        let mut answer = [0_u8; 8];
        stream.read_exact(&mut answer).await?;
        Ok::<_, std::io::Error>(answer)
    })
    .await;
    drop(request);
    let result = match answer {
        Ok(Ok(answer)) => keyring_control::unlock_result(answer),
        _ => keyring_control::UnlockResult::Failed,
    };
    tracing::info!(
        target: target::SESSION,
        result = result.token(),
        "login keyring unlock answered"
    );
    match result {
        keyring_control::UnlockResult::Unlocked => Ok(()),
        other => Err(format!("the keyring answered {}", other.token())),
    }
}

/// Starts the helper as the session user and hands it `password`; returns
/// at once and logs the outcome when the helper ends (bounded).
#[cfg(target_os = "linux")]
pub(crate) fn unlock_in_background(
    agent_binary: &std::path::Path,
    identity: &super::identity::UserIdentity,
    environment: &super::identity::SessionEnvironment,
    session_log_id: &str,
    session_id: &str,
    password: Zeroizing<Vec<u8>>,
) {
    let mut command = crate::command_for_helper(agent_binary, "session-agent");
    command
        .arg(super::agent::LOGIN_KEYRING_UNLOCK_FLAG)
        .arg("--session-log-id")
        .arg(session_log_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    if super::identity::configure_user_command(&mut command, identity, environment).is_err() {
        tracing::warn!(target: target::SESSION, session_id, "the login keyring is not unlocked: the helper could not be set up");
        return;
    }
    command.env("XDG_RUNTIME_DIR", format!("/run/user/{}", identity.uid));
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!(target: target::SESSION, session_id, %error, "the login keyring is not unlocked: the helper did not start");
            return;
        }
    };
    let session_id = session_id.to_owned();
    tokio::spawn(async move {
        if let Some(mut stdin) = child.stdin.take() {
            let written = stdin.write_all(&password).await;
            drop(stdin);
            drop(password);
            if written.is_err() {
                let _ = child.start_kill();
            }
        }
        let status = tokio::time::timeout(HELPER_TIMEOUT, child.wait()).await;
        let unlocked = matches!(status, Ok(Ok(status)) if status.success());
        if unlocked {
            tracing::info!(target: target::SESSION, session_id = %session_id, "keyring unlocked: the login keyring opened with the sign-in password");
        } else {
            let _ = child.start_kill();
            tracing::warn!(target: target::SESSION, session_id = %session_id, "the login keyring stayed locked (no keyring daemon, or the password does not open it); applications will ask for its password");
        }
    });
}
