// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Unlocking a running GNOME keyring daemon with the login password.
//!
//! A display manager's login unlocks the user's login keyring through
//! `pam_gnome_keyring`, which hands the password it saw in the PAM auth
//! phase to the daemon over the daemon's control socket
//! (`$XDG_RUNTIME_DIR/keyring/control`). A sign-in Arcen completes without
//! that phase — GDM's login screen taking a one-time token, or Arcen's own
//! login session on a host without a display manager — starts the daemon
//! locked. The Pier proved the same password with its own PAM service just
//! before, so it does what `pam_gnome_keyring` would have done: one
//! `UNLOCK` request on the same socket, as the session user (the daemon
//! accepts only its own user), which opens the login keyring or creates it
//! with that password when there is none.
//!
//! The control protocol (gnome-keyring 3.x–46, `gkr-pam-client.c`): the
//! client sends one credentials byte (0), then a packet of big-endian
//! `u32` length (the whole packet), `u32` operation, and each argument as
//! `u32` length and bytes; the daemon answers 8 bytes: length 8 and a
//! `u32` result code. The client checks the daemon's peer credentials
//! first, as `pam_gnome_keyring` does.

/// The socket's name in the control directory.
pub const CONTROL_SOCKET: &str = "control";
/// The control directory's name in the user's runtime directory.
pub const CONTROL_DIRECTORY: &str = "keyring";
/// Longest password handed on (the daemon reads at most 8 KiB).
pub const MAX_PASSWORD_BYTES: usize = 8192;
/// `GKD_CONTROL_OP_UNLOCK`.
const OP_UNLOCK: u32 = 1;

/// The credentials byte a client sends first.
pub const CREDENTIALS_BYTE: u8 = 0;

/// The `UNLOCK` request for `password`, or `None` when it is empty, too
/// long, or holds a NUL (the daemon reads a C string). It holds the
/// password: built at its final size (never reallocated), and the caller
/// wipes it once sent.
#[must_use]
pub fn unlock_request(password: &[u8]) -> Option<Vec<u8>> {
    if password.is_empty() || password.len() > MAX_PASSWORD_BYTES || password.contains(&0) {
        return None;
    }
    let argument = u32::try_from(password.len()).ok()?;
    let total = 12 + argument;
    let mut packet = Vec::with_capacity(usize::try_from(total).ok()?);
    packet.extend_from_slice(&total.to_be_bytes());
    packet.extend_from_slice(&OP_UNLOCK.to_be_bytes());
    packet.extend_from_slice(&argument.to_be_bytes());
    packet.extend_from_slice(password);
    Some(packet)
}

/// What the daemon answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnlockResult {
    /// The login keyring is unlocked (or was created with the password).
    Unlocked,
    /// The password does not open the login keyring.
    Denied,
    /// The daemon could not do it.
    Failed,
    /// No daemon answers on that socket.
    NoDaemon,
    /// The answer is not one the protocol knows.
    Malformed,
}

impl UnlockResult {
    /// Stable log token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Unlocked => "unlocked",
            Self::Denied => "denied",
            Self::Failed => "failed",
            Self::NoDaemon => "no_daemon",
            Self::Malformed => "malformed",
        }
    }
}

/// Reads the daemon's 8-byte answer.
#[must_use]
pub fn unlock_result(answer: [u8; 8]) -> UnlockResult {
    let [l0, l1, l2, l3, r0, r1, r2, r3] = answer;
    if u32::from_be_bytes([l0, l1, l2, l3]) != 8 {
        return UnlockResult::Malformed;
    }
    match u32::from_be_bytes([r0, r1, r2, r3]) {
        0 => UnlockResult::Unlocked,
        1 => UnlockResult::Denied,
        2 => UnlockResult::Failed,
        3 => UnlockResult::NoDaemon,
        _ => UnlockResult::Malformed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unlock_request_is_the_packet_pam_gnome_keyring_sends() {
        let packet = unlock_request(b"pw!").expect("request");
        assert_eq!(
            packet.as_slice(),
            &[0, 0, 0, 15, 0, 0, 0, 1, 0, 0, 0, 3, b'p', b'w', b'!']
        );
    }

    #[test]
    fn passwords_the_daemon_cannot_read_are_not_sent() {
        assert!(unlock_request(b"").is_none());
        assert!(unlock_request(b"a\0b").is_none());
        assert!(unlock_request(&vec![b'a'; MAX_PASSWORD_BYTES + 1]).is_none());
        assert!(unlock_request(&vec![b'a'; MAX_PASSWORD_BYTES]).is_some());
    }

    #[test]
    fn the_answer_is_read_strictly() {
        assert_eq!(
            unlock_result([0, 0, 0, 8, 0, 0, 0, 0]),
            UnlockResult::Unlocked
        );
        assert_eq!(
            unlock_result([0, 0, 0, 8, 0, 0, 0, 1]),
            UnlockResult::Denied
        );
        assert_eq!(
            unlock_result([0, 0, 0, 8, 0, 0, 0, 2]),
            UnlockResult::Failed
        );
        assert_eq!(
            unlock_result([0, 0, 0, 8, 0, 0, 0, 3]),
            UnlockResult::NoDaemon
        );
        assert_eq!(
            unlock_result([0, 0, 0, 8, 0, 0, 0, 9]),
            UnlockResult::Malformed
        );
        assert_eq!(
            unlock_result([0, 0, 0, 9, 0, 0, 0, 0]),
            UnlockResult::Malformed
        );
    }
}
