//! Proof that a socket in a headless Wayland session's runtime directory is
//! that session's, and that the process on the other end is its agent.
//!
//! The Pier (root) proves `arcen-clipboard.sock` with it; the path check
//! comes first, the peer check after connecting is the authority. The same
//! rule as `input::wayland` applies to `arcen-input.sock` (that module keeps
//! its own copy for now, so it stays an OS adapter rather than portable host
//! code; folding the two together is a follow-up).

use std::path::Path;

/// Why a session socket was not proven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketProofError {
    /// The socket is not directly inside the runtime directory.
    OutsideRuntimeDir,
    /// The directory or socket is not the session user's alone.
    NotPrivate,
    /// The peer is not the session user's agent.
    ForeignPeer {
        /// Peer uid, when the kernel reported one.
        uid: Option<u32>,
        /// Peer pid, when the kernel reported one.
        pid: Option<i32>,
    },
}

impl std::fmt::Display for SocketProofError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutsideRuntimeDir => {
                formatter.write_str("socket is outside the session runtime directory")
            }
            Self::NotPrivate => formatter
                .write_str("socket or runtime directory is not private to the session user"),
            Self::ForeignPeer { uid, pid } => write!(
                formatter,
                "socket peer is not the session's agent (uid {uid:?}, pid {pid:?})"
            ),
        }
    }
}

impl std::error::Error for SocketProofError {}

/// The socket and its directory belong to `uid` alone (mode `0?00`), and the
/// socket is directly inside the directory.
///
/// # Errors
///
/// [`SocketProofError`] naming what is wrong.
pub fn verify_private_socket(
    runtime_dir: &Path,
    socket: &Path,
    uid: u32,
) -> Result<(), SocketProofError> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    if socket.parent() != Some(runtime_dir) {
        return Err(SocketProofError::OutsideRuntimeDir);
    }
    let private = |path: &Path, is_kind: fn(&std::fs::Metadata) -> bool| {
        std::fs::symlink_metadata(path).is_ok_and(|metadata| {
            is_kind(&metadata) && metadata.uid() == uid && metadata.mode() & 0o077 == 0
        })
    };
    if private(runtime_dir, std::fs::Metadata::is_dir)
        && private(socket, |metadata| metadata.file_type().is_socket())
    {
        Ok(())
    } else {
        Err(SocketProofError::NotPrivate)
    }
}

/// The connected peer runs as `uid` and, when `agent_pid` is given, is that
/// process.
///
/// # Errors
///
/// [`SocketProofError::ForeignPeer`] otherwise, or when the kernel reports no
/// credentials.
pub fn verify_peer(
    stream: &tokio::net::UnixStream,
    uid: u32,
    agent_pid: Option<u32>,
) -> Result<(), SocketProofError> {
    let credentials = stream
        .peer_cred()
        .map_err(|_| SocketProofError::ForeignPeer {
            uid: None,
            pid: None,
        })?;
    let pid = credentials.pid();
    let pid_matches =
        agent_pid.is_none_or(|agent| pid.and_then(|pid| u32::try_from(pid).ok()) == Some(agent));
    if credentials.uid() == uid && pid_matches {
        Ok(())
    } else {
        Err(SocketProofError::ForeignPeer {
            uid: Some(credentials.uid()),
            pid,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::current_exe()
            .unwrap()
            .with_file_name(format!("arcen-socket-proof-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[tokio::test]
    async fn a_private_socket_of_this_user_and_process_is_proven() {
        let dir = scratch("ok");
        let socket = dir.join("s.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let uid = nix::unistd::Uid::effective().as_raw();
        assert_eq!(verify_private_socket(&dir, &socket, uid), Ok(()));
        assert_eq!(
            verify_private_socket(Path::new("/"), &socket, uid),
            Err(SocketProofError::OutsideRuntimeDir)
        );
        assert_eq!(
            verify_private_socket(&dir, &socket, uid.wrapping_add(1)),
            Err(SocketProofError::NotPrivate)
        );
        let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let _accepted = listener.accept().await.unwrap();
        assert_eq!(verify_peer(&stream, uid, Some(std::process::id())), Ok(()));
        assert!(matches!(
            verify_peer(&stream, uid, Some(std::process::id().wrapping_add(1))),
            Err(SocketProofError::ForeignPeer { .. })
        ));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            verify_private_socket(&dir, &socket, uid),
            Err(SocketProofError::NotPrivate)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
