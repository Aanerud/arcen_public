// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Deck-side client for the privileged USB helper.
//!
//! Deck never captures the physical device itself. It connects to the
//! root-owned helper socket, proves the peer is root, and hands the stream to
//! the shared component client (`arcen_deck_core::runtime::usb_component`),
//! which forwards the *same* `arcen-protocol` URB frames the Deck exchanges
//! with the host, so this boundary adds no second wire format. See
//! `docs/adr/0011-macos-privileged-usb-helper.md`.

use arcen_deck_core::runtime::usb_component::ComponentClient;
use tokio::net::UnixStream;

/// One connected privileged-helper session.
pub type HelperClient = ComponentClient<UnixStream>;

/// How messages name the helper.
const PEER: &str = "privileged USB helper";

/// Default rendezvous path, matching `arcen-usb-helper`'s default and the
/// launchd plist. Every directory on it is root's and writable by nobody
/// else (not `/var/run`, which group `daemon` can write on macOS).
pub const DEFAULT_SOCKET: &str = "/private/var/db/arcen/run/usb-helper.sock";

/// The effective uid of the process at the other end of a Unix socket.
///
/// `getpeereid` reads the credentials the kernel recorded at connect time,
/// never a pid that could be reused.
fn peer_uid(fd: std::os::fd::BorrowedFd<'_>) -> std::io::Result<u32> {
    use std::os::fd::AsRawFd;
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: both out-params are initialized locals, and the descriptor is
    // borrowed, so it stays open for the duration of the call.
    let result = unsafe { libc::getpeereid(fd.as_raw_fd(), &raw mut uid, &raw mut gid) };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(uid)
}

/// Only root may answer as the privileged helper: anything else listening on
/// the helper's path would receive the host's URB traffic and could feed the
/// Deck a forged device.
fn check_helper_peer(uid: std::io::Result<u32>, path: &str) -> Result<(), String> {
    match uid {
        Ok(0) => Ok(()),
        Ok(uid) => Err(format!(
            "refusing the USB helper at {path}: it runs as uid {uid}, not root; \
             another process may be impersonating the helper"
        )),
        Err(error) => Err(format!(
            "refusing the USB helper at {path}: its credentials cannot be read: {error}"
        )),
    }
}

/// Connects to the helper, proves it runs as root, and completes the
/// identity handshake.
///
/// # Errors
///
/// Returns a message when the helper is not running, the socket is not
/// reachable by this user, the peer is not root, or the handshake frame is
/// malformed.
pub async fn connect(path: &str) -> Result<HelperClient, String> {
    let stream = UnixStream::connect(path).await.map_err(|error| {
        format!(
            "connect privileged USB helper at {path}: {error}; \
             start it with `sudo arcen-usb-helper`"
        )
    })?;
    {
        use std::os::fd::AsFd;
        if let Err(refusal) = check_helper_peer(peer_uid(stream.as_fd()), path) {
            tracing::error!(target: crate::logging::target::USB, "{refusal}");
            return Err(refusal);
        }
    }
    ComponentClient::handshake(stream, PEER).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_socket_matches_the_helpers_launchd_plist() {
        let plist = include_str!("../../../packaging/macos/tech.arcen.deck.usbhelper.plist");
        assert!(
            plist.contains(&format!("<string>{DEFAULT_SOCKET}</string>")),
            "the plist must start the helper on {DEFAULT_SOCKET}"
        );
        assert!(!DEFAULT_SOCKET.starts_with("/var/"));
        assert!(!DEFAULT_SOCKET.starts_with("/private/var/run"));
    }

    #[test]
    fn only_a_root_peer_is_accepted_as_the_helper() {
        assert_eq!(check_helper_peer(Ok(0), DEFAULT_SOCKET), Ok(()));
        let refusal = check_helper_peer(Ok(501), DEFAULT_SOCKET).unwrap_err();
        assert!(refusal.contains("uid 501, not root"), "{refusal}");
        let unreadable = check_helper_peer(
            Err(std::io::Error::from_raw_os_error(libc::ENOTCONN)),
            DEFAULT_SOCKET,
        )
        .unwrap_err();
        assert!(unreadable.contains("cannot be read"), "{unreadable}");
    }

    /// This test process's own uid, as a socket peer sees it.
    fn own_uid() -> u32 {
        use std::os::fd::AsFd;
        let (deck, _impostor) = std::os::unix::net::UnixStream::pair().expect("socket pair");
        peer_uid(deck.as_fd()).expect("peer credentials")
    }

    #[test]
    fn a_helper_peer_that_is_not_root_is_refused() {
        let uid = own_uid();
        if uid != 0 {
            assert!(check_helper_peer(Ok(uid), DEFAULT_SOCKET).is_err());
        }
    }

    #[tokio::test]
    async fn connect_refuses_a_listener_that_is_not_root_before_reading_anything() {
        // A short path: `sun_path` holds 104 bytes.
        let home = std::fs::canonicalize(std::env::var_os("HOME").expect("HOME")).expect("home");
        let directory = home.join(format!(".arcen-hc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir(&directory).expect("scratch directory");
        let path = directory.join("h.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        // An impostor that would answer with a valid hello.
        let impostor = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = std::io::Write::write_all(
                &mut stream,
                &[1, 0, 0, 0, arcen_usb_bridge::component_ipc::TAG_HELLO],
            );
        });
        let outcome = connect(path.to_str().expect("utf-8")).await;
        let _ = impostor.join();
        let _ = std::fs::remove_dir_all(&directory);
        if own_uid() != 0 {
            let refusal = outcome.expect_err("a non-root helper is refused");
            assert!(refusal.contains("not root"), "{refusal}");
        }
    }
}
