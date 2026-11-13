// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Arcen privileged USB helper.
//!
//! This is the only Arcen macOS component that runs with root privilege, and it
//! exists so that Arcen Deck does not have to. It captures one exact, policy
//! approved USB input device and executes the control and interrupt transfers
//! Deck forwards from the host.
//!
//! See `docs/adr/0011-macos-privileged-usb-helper.md`. Tranche 1 (this file)
//! uses a root-owned Unix socket with peer-uid authentication and is started
//! deliberately by an administrator. Tranche 2 replaces the transport with an
//! `SMAppService` daemon publishing an XPC Mach service pinned by a
//! code-signing requirement, which is what removes the administrator step.
//!
//! Deliberate non-goals for this process: no async runtime, no network
//! transport, no UI, no media, no serialization framework. Every dependency
//! here is part of the root attack surface.

mod capture;
mod frame;
#[cfg(feature = "iousbhost")]
mod iousbhost;
mod registry;

use arcen_secure_dir::{DirPolicy, TrustedDir};
use capture::{speed_code, Backend, PhysicalDevice};
use frame::{
    encode_hello, encode_info, read_frame, write_frame, TAG_CANCEL, TAG_COMPLETE, TAG_ERROR,
    TAG_HELLO, TAG_INFO, TAG_SUBMIT,
};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Default rendezvous path; the Deck and the launchd plist name the same one.
///
/// Not under `/var/run`: on macOS that is `root:daemon` 0775 without the sticky bit,
/// so a member of group `daemon` could replace entries there. Every ancestor
/// of this directory is `root:wheel` 0755 and the directory itself is root's
/// 0755 (the Deck must be able to reach the socket), so only root can create,
/// replace or remove anything on the path. `/Library/Application Support`
/// would also do (`root:admin` 0755), but `/private/var/db` is where macOS
/// keeps root daemons' state, next to the IOUSBHost capture marker
/// (`/private/var/db/arcen/state`). The helper creates and verifies the
/// directory at start ([`TrustedDir`]).
const DEFAULT_SOCKET: &str = "/private/var/db/arcen/run/usb-helper.sock";
/// Mode of the socket directory and of `/private/var/db/arcen` if the helper
/// creates it.
const SOCKET_DIRECTORY_MODE: u32 = 0o755;

/// Outbound completion frames buffered before the producer is made to wait.
///
/// Generous enough that ordinary bursts never touch it, small enough that a
/// client which stops reading cannot cost this root process real memory.
const OUTBOUND_QUEUE_LIMIT: usize = 256;

/// How long one socket write may block before the session is abandoned.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the helper waits for a client's **first** request before abandoning
/// the session.
///
/// Deliberately not a liveness deadline for an established session. A healthy
/// Hard USB session can be completely silent for as long as the operator leaves
/// the tablet alone: the shared exporter's interrupt hold
/// (`arcen_usb_bridge::hold_until_report`) keeps a submitted interrupt
/// transfer pending inside the helper until the device actually reports, so no
/// completion goes back to Deck and Deck sends nothing further. Applying this
/// deadline to an established session would disconnect an idle operator.
///
/// It exists only to bound the one case that has no legitimate form — a client
/// that connects, takes the device, and never asks for anything. Once a first
/// valid request arrives the deadline is removed, and liveness afterwards is
/// the socket's own business: a peer that goes away fails the pump's write,
/// which ends the session.
const FIRST_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

fn main() {
    // Nothing this root process creates is for anyone else. Set before any
    // thread exists, so the socket is unconnectable by others even for the
    // moment between its bind and its explicit mode.
    let _ = rustix::process::umask(arcen_secure_dir::mode(0o077));
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match parse_args(&args) {
        Ok(options) => options,
        Err(error) => fail(&error),
    };
    let Options {
        socket_path,
        allowed_uid,
        backend,
        action,
    } = options;
    match action {
        Action::Serve => {}
        Action::Probe => {
            probe(backend);
            return;
        }
        #[cfg(feature = "iousbhost")]
        Action::ProbeIoUsbHost => {
            match iousbhost::probe() {
                Ok(lines) => lines.iter().for_each(|line| println!("{line}")),
                Err(error) => fail(&error),
            }
            return;
        }
        Action::Help => {
            println!(
                "usage: arcen-usb-helper [--socket PATH] [--allow-uid UID] [--backend NAME] [--probe]\n\n\
                 Runs the privileged Arcen USB capture helper. Must run as root.\n\
                 --probe captures and immediately releases the device, then exits.\n\
                 --backend selects the capture backend (this build: {}; default {}).",
                Backend::available().join(", "),
                Backend::COMPILED_DEFAULT.name()
            );
            return;
        }
    }

    // SAFETY: `geteuid` reads process credentials and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if euid != 0 {
        fail("arcen-usb-helper must run as root; it exists so Deck does not have to");
    }

    // A helper that stopped while holding an IOUSBHost capture left a marker;
    // launchd restarts the daemon, and this gives the device back first.
    #[cfg(feature = "iousbhost")]
    iousbhost::recover_at_start();

    let allowed_uid = allowed_uid.unwrap_or_else(console_owner_uid);
    if allowed_uid == 0 {
        fail("refusing to authorize root as the client uid; pass --allow-uid");
    }

    eprintln!(
        "arcen-usb-helper: capture backend: {} (version {})",
        backend.name(),
        env!("CARGO_PKG_VERSION")
    );
    if let Err(error) = serve(Path::new(&socket_path), allowed_uid, backend) {
        fail(&error);
    }
}

/// What one invocation does, decided only after every argument was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Serve,
    Probe,
    #[cfg(feature = "iousbhost")]
    ProbeIoUsbHost,
    Help,
}

#[derive(Debug, PartialEq, Eq)]
struct Options {
    socket_path: String,
    allowed_uid: Option<u32>,
    backend: Backend,
    action: Action,
}

/// Parses the whole command line before anything runs, so an option after
/// `--probe` (`--probe --backend libusb`) still applies to the probe and a
/// bad argument anywhere refuses the invocation. `--help` answers at once.
///
/// # Errors
///
/// Returns a message naming the bad or incomplete argument.
fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        socket_path: DEFAULT_SOCKET.to_owned(),
        allowed_uid: None,
        backend: Backend::COMPILED_DEFAULT,
        action: Action::Serve,
    };
    let mut values = args.iter();
    while let Some(argument) = values.next() {
        match argument.as_str() {
            "--socket" => {
                options.socket_path = values.next().ok_or("--socket requires a path")?.clone();
            }
            "--allow-uid" => {
                options.allowed_uid = Some(
                    values
                        .next()
                        .and_then(|value| value.parse().ok())
                        .ok_or("--allow-uid requires a numeric uid")?,
                );
            }
            "--backend" => {
                options.backend =
                    Backend::parse(values.next().ok_or("--backend requires a name")?)?;
            }
            "--probe" => options.action = Action::Probe,
            #[cfg(feature = "iousbhost")]
            "--probe-iousbhost" => options.action = Action::ProbeIoUsbHost,
            "-h" | "--help" => {
                options.action = Action::Help;
                return Ok(options);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(options)
}

fn fail(message: &str) -> ! {
    eprintln!("arcen-usb-helper: {message}");
    std::process::exit(1);
}

/// Best-effort owner of the current console session, used as the default
/// authorized client uid when one is not supplied explicitly.
fn console_owner_uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/dev/console").map_or(0, |metadata| metadata.uid())
}

/// Captures and immediately releases, proving privilege and profile match
/// without opening a socket.
fn probe(backend: Backend) {
    match PhysicalDevice::capture(backend) {
        Ok(device) => {
            let id = device.identity();
            println!(
                "arcen-usb-helper probe backend={} device={:04x}:{:04x} bcd={:04x} capture=ok",
                backend.name(),
                id.vendor_id,
                id.product_id,
                id.bcd_device
            );
            device.shutdown();
            drop(device);
            println!("arcen-usb-helper probe release=ok");
        }
        Err(error) => {
            eprintln!(
                "arcen-usb-helper probe backend={} failed: {error}",
                backend.name()
            );
            std::process::exit(1);
        }
    }
}

/// Splits a socket path into its directory and its entry name.
///
/// # Errors
///
/// Refuses a relative path, a path ending in `..`, or a name that is not
/// valid UTF-8.
fn socket_location(socket_path: &Path) -> Result<(PathBuf, String), String> {
    let invalid = || format!("{} is not a valid socket path", socket_path.display());
    if !socket_path.is_absolute() {
        return Err(format!("{} is not an absolute path", socket_path.display()));
    }
    let directory = socket_path.parent().ok_or_else(invalid)?;
    let name = socket_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(invalid)?;
    arcen_secure_dir::check_entry_name(name)?;
    Ok((directory.to_path_buf(), name.to_owned()))
}

fn serve(socket_path: &Path, allowed_uid: u32, backend: Backend) -> Result<(), String> {
    // The directory is verified from `/` down and pinned, and the socket is
    // bound, given its mode and handed to the client uid relative to it: no
    // `chmod` or `chown` follows a path anyone but root could change, and a
    // stale entry is removed only if it is genuinely a socket.
    let (directory, name) = socket_location(socket_path)?;
    let directory = TrustedDir::create(
        &directory,
        DirPolicy {
            owner_uid: 0,
            mode: SOCKET_DIRECTORY_MODE,
        },
        SOCKET_DIRECTORY_MODE,
    )
    .map_err(|error| format!("socket directory {}: {error}", directory.display()))?;
    // The peer-uid check is the real gate; the mode and owner simply avoid
    // advertising the endpoint to every local process.
    let listener = directory
        .bind_socket(&name, allowed_uid)
        .map_err(|error| format!("bind {}: {error}", socket_path.display()))?;

    eprintln!(
        "arcen-usb-helper: listening on {} for uid {allowed_uid}",
        socket_path.display()
    );

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("arcen-usb-helper: accept failed: {error}");
                continue;
            }
        };
        match peer_uid(&stream) {
            Ok(uid) if uid == allowed_uid => {}
            Ok(uid) => {
                eprintln!("arcen-usb-helper: rejected connection from uid {uid}");
                continue;
            }
            Err(error) => {
                eprintln!("arcen-usb-helper: could not read peer credentials: {error}");
                continue;
            }
        }
        if let Err(error) = handle_client(&stream, backend) {
            eprintln!("arcen-usb-helper: session ended: {error}");
            let _ = write_frame(&mut &stream, TAG_ERROR, error.as_bytes());
        }
        eprintln!("arcen-usb-helper: device released, awaiting next client");
    }
    Ok(())
}

/// Reads the connecting process's effective uid from the socket itself.
///
/// Uses `getpeereid`, never the peer's pid: a pid can be reused between the
/// check and its use, which is the classic local privilege-escalation race.
fn peer_uid(stream: &UnixStream) -> Result<u32, String> {
    use std::os::fd::AsRawFd;
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: both out-params are initialized locals, and the fd stays owned by
    // `stream` for the duration of the call.
    let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if result != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(uid)
}

/// Serves one Deck connection: capture, handshake, then pump URBs until the
/// peer disconnects. The device is released on every exit path.
fn handle_client(stream: &UnixStream, backend: Backend) -> Result<(), String> {
    eprintln!(
        "arcen-usb-helper: client connected; capturing device with {}",
        backend.name()
    );
    let device = Arc::new(PhysicalDevice::capture(backend)?);
    let identity = device.identity();
    eprintln!(
        "arcen-usb-helper: captured {:04x}:{:04x} with {}",
        identity.vendor_id,
        identity.product_id,
        backend.name()
    );

    let mut writer = stream
        .try_clone()
        .map_err(|error| format!("clone socket: {error}"))?;
    let mut reader = std::io::BufReader::new(
        stream
            .try_clone()
            .map_err(|error| format!("clone socket: {error}"))?,
    );

    let descriptors: Vec<(u8, &[u8])> = identity
        .report_descriptors
        .iter()
        .map(|entry| (entry.interface, entry.descriptor.as_slice()))
        .collect();
    let hello = encode_hello(
        identity.vendor_id,
        identity.product_id,
        identity.bcd_device,
        identity.device_class,
        speed_code(identity.speed),
        &descriptors,
    )?;
    write_frame(&mut writer, TAG_HELLO, &hello).map_err(|error| {
        format!("send handshake with the tablet's HID report descriptors: {error}")
    })?;
    // Before the pump exists, so it precedes every completion. Older Decks
    // skip the tag.
    write_frame(&mut writer, TAG_INFO, &encode_info(backend.name()))
        .map_err(|error| format!("send helper info: {error}"))?;

    // Every outbound completion funnels through one channel and one writer, so
    // worker completions and synchronous cancellation acknowledgements cannot
    // interleave mid-frame on the socket.
    //
    // Bounded, and this bound is the load-bearing part. This is a root process,
    // and a connected client can make it produce frames faster than it reads
    // them — cancellation acknowledgements are emitted synchronously, one per
    // request, whether or not the URB was ever in flight. An unbounded queue
    // therefore grew without limit for any client that stopped draining its
    // socket. A full queue now blocks the producer instead, which stalls that
    // one misbehaving session and nothing else.
    let (outbound_tx, outbound_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(OUTBOUND_QUEUE_LIMIT);

    // A client that never reads would otherwise leave the writer blocked
    // forever, and the accept loop is serial, so that one session would hold
    // the helper — and the captured tablet — closed to every later client.
    // Bound both directions in time.
    //
    // These must succeed. Continuing without them silently restores exactly
    // the unbounded wait they exist to prevent.
    writer
        .set_write_timeout(Some(WRITE_TIMEOUT))
        .map_err(|error| format!("set socket write timeout: {error}"))?;
    reader
        .get_ref()
        .set_read_timeout(Some(FIRST_REQUEST_TIMEOUT))
        .map_err(|error| format!("set socket read timeout: {error}"))?;

    // The pump owns a socket handle purely so it can end the session. Breaking
    // out of its loop is not enough: dropping this clone leaves the original
    // and the reader's clone open, so `run_requests` would stay blocked in
    // `read_frame` and the session would never reach `device.shutdown()`.
    let pump_socket = stream
        .try_clone()
        .map_err(|error| format!("clone socket: {error}"))?;
    let pump = std::thread::spawn(move || {
        while let Ok(frame) = outbound_rx.recv() {
            if let Err(error) = write_frame(&mut writer, TAG_COMPLETE, &frame) {
                eprintln!("arcen-usb-helper: write failed, ending session: {error}");
                // Whatever went wrong — a timeout, a closed peer, a partial
                // frame — the stream's framing can no longer be trusted, so
                // tear the session down rather than leave a desynchronised
                // client reading garbage.
                let _ = pump_socket.shutdown(std::net::Shutdown::Both);
                break;
            }
        }
    });

    let drain_device = Arc::clone(&device);
    let drain_tx = outbound_tx.clone();
    let drain = std::thread::spawn(move || {
        while let Ok(frame) = drain_device.next_completion() {
            if drain_tx.send(frame).is_err() {
                break;
            }
        }
    });

    let outcome = run_requests(&device, &mut reader, &outbound_tx, stream);
    drop(outbound_tx);

    device.shutdown();
    // Closing the socket unblocks the pump; dropping the last device reference
    // makes the shared exporter release the device exactly once, which
    // releases every interface and restores the macOS driver stack
    // (`RusbDevice::release`, or the last running transfer if one outlives
    // the drain).
    let _ = stream.shutdown(std::net::Shutdown::Both);
    let _ = drain.join();
    let _ = pump.join();
    outcome
}

fn run_requests(
    device: &PhysicalDevice,
    reader: &mut impl std::io::Read,
    outbound: &std::sync::mpsc::SyncSender<Vec<u8>>,
    socket: &UnixStream,
) -> Result<(), String> {
    let mut deadline_cleared = false;
    loop {
        let (tag, payload) = match read_frame(reader) {
            Ok(framed) => framed,
            // End of stream is the ordinary way a session finishes. So is the
            // first-request deadline expiring, which is why that deadline must
            // not still be armed once a session is genuinely under way.
            Err(_) => return Ok(()),
        };
        if !deadline_cleared {
            // A real client has now asked for something, so it is not the
            // silent squatter the deadline exists for. Remove it: an
            // established session may legitimately be silent for as long as
            // the operator leaves the tablet alone, because the interrupt
            // transfer it is waiting on stays pending inside this process.
            socket
                .set_read_timeout(None)
                .map_err(|error| format!("clear socket read timeout: {error}"))?;
            deadline_cleared = true;
        }
        match tag {
            TAG_SUBMIT => {
                let (header, body) = arcen_protocol::decode_usb_urb_submit(&payload)
                    .map_err(|error| format!("invalid submit frame: {error:?}"))?;
                device.submit(header, body)?;
            }
            TAG_CANCEL => {
                let (generation, urb_id) = arcen_protocol::decode_usb_urb_cancel(&payload)
                    .map_err(|error| format!("invalid cancel frame: {error:?}"))?;
                // The terminal cancellation completion must reach Deck; the
                // worker's own late completion is dropped by the tombstone.
                let frame = device.cancel(generation, urb_id)?;
                if outbound.send(frame).is_err() {
                    return Ok(());
                }
            }
            other => return Err(format!("unexpected helper frame tag {other:#04x}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_socket_lives_in_a_directory_only_root_can_change() {
        assert!(!DEFAULT_SOCKET.starts_with("/var/"));
        assert!(!DEFAULT_SOCKET.starts_with("/private/var/run"));
        assert_eq!(
            socket_location(Path::new(DEFAULT_SOCKET)),
            Ok((
                PathBuf::from("/private/var/db/arcen/run"),
                "usb-helper.sock".to_owned()
            ))
        );
        // `sun_path` holds 104 bytes.
        assert!(DEFAULT_SOCKET.len() < 104);
    }

    #[test]
    fn the_launchd_plist_starts_the_helper_on_the_default_socket() {
        let plist = include_str!("../../../../packaging/macos/tech.arcen.deck.usbhelper.plist");
        assert!(
            plist.contains(&format!("<string>{DEFAULT_SOCKET}</string>")),
            "the plist must pass --socket {DEFAULT_SOCKET}"
        );
        assert!(!plist.contains("/var/run/"));
    }

    #[test]
    fn the_backend_is_iousbhost_only_in_an_iousbhost_build() {
        if cfg!(feature = "iousbhost") {
            assert_eq!(Backend::COMPILED_DEFAULT.name(), "iousbhost");
            assert_eq!(Backend::available(), vec!["libusb", "iousbhost"]);
            assert_eq!(
                Backend::parse("iousbhost").map(Backend::name),
                Ok("iousbhost")
            );
        } else {
            assert_eq!(Backend::COMPILED_DEFAULT, Backend::Libusb);
            assert_eq!(Backend::available(), vec!["libusb"]);
            let refusal = Backend::parse("iousbhost").expect_err("not in this build");
            assert!(refusal.contains("this build has: libusb"), "{refusal}");
        }
        assert_eq!(Backend::parse("libusb"), Ok(Backend::Libusb));
        assert!(Backend::parse("rusb").is_err());
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|&argument| argument.to_owned()).collect()
    }

    #[test]
    fn probe_runs_after_every_argument_is_read() {
        let options = parse_args(&args(&["--probe", "--backend", "libusb"])).expect("valid");
        assert_eq!(options.action, Action::Probe);
        assert_eq!(options.backend, Backend::Libusb);
        let options = parse_args(&args(&["--backend", "libusb", "--probe"])).expect("valid");
        assert_eq!(
            (options.action, options.backend),
            (Action::Probe, Backend::Libusb)
        );
        let options = parse_args(&args(&["--probe"])).expect("valid");
        assert_eq!(options.backend, Backend::COMPILED_DEFAULT);
        // A bad argument after --probe refuses the run instead of being ignored.
        assert!(parse_args(&args(&["--probe", "--backend", "nope"])).is_err());
        assert!(parse_args(&args(&["--probe", "--bogus"])).is_err());
        assert!(parse_args(&args(&["--probe", "--backend"])).is_err());
        assert_eq!(
            parse_args(&args(&["--socket", "/x/y.sock", "--allow-uid", "501"])),
            Ok(Options {
                socket_path: "/x/y.sock".to_owned(),
                allowed_uid: Some(501),
                backend: Backend::COMPILED_DEFAULT,
                action: Action::Serve,
            })
        );
    }

    #[test]
    fn socket_paths_must_be_absolute_with_a_plain_name() {
        assert!(socket_location(Path::new("usb-helper.sock")).is_err());
        assert!(socket_location(Path::new("/private/var/db/arcen/run/..")).is_err());
        assert!(socket_location(Path::new("/")).is_err());
    }
}
