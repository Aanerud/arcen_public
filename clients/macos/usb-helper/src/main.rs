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

use capture::{speed_code, PhysicalDevice};
use frame::{
    encode_hello, read_frame, write_frame, TAG_CANCEL, TAG_COMPLETE, TAG_ERROR, TAG_HELLO,
    TAG_SUBMIT,
};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Default rendezvous path. Root-owned directory, so an unprivileged process
/// cannot pre-create or replace the socket.
const DEFAULT_SOCKET: &str = "/var/run/arcen-usb-helper.sock";

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
/// the tablet alone: `read_interrupt_until_report` keeps a submitted interrupt
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
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut socket_path = DEFAULT_SOCKET.to_owned();
    let mut allowed_uid: Option<u32> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--socket" => {
                index += 1;
                match args.get(index) {
                    Some(value) => socket_path = value.clone(),
                    None => fail("--socket requires a path"),
                }
            }
            "--allow-uid" => {
                index += 1;
                match args.get(index).and_then(|value| value.parse().ok()) {
                    Some(value) => allowed_uid = Some(value),
                    None => fail("--allow-uid requires a numeric uid"),
                }
            }
            "--probe" => {
                probe();
                return;
            }
            "-h" | "--help" => {
                println!(
                    "usage: arcen-usb-helper [--socket PATH] [--allow-uid UID] [--probe]\n\n\
                     Runs the privileged Arcen USB capture helper. Must run as root.\n\
                     --probe captures and immediately releases the device, then exits."
                );
                return;
            }
            other => fail(&format!("unknown argument {other}")),
        }
        index += 1;
    }

    // SAFETY: `geteuid` reads process credentials and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if euid != 0 {
        fail("arcen-usb-helper must run as root; it exists so Deck does not have to");
    }

    let allowed_uid = allowed_uid.unwrap_or_else(console_owner_uid);
    if allowed_uid == 0 {
        fail("refusing to authorize root as the client uid; pass --allow-uid");
    }

    if let Err(error) = serve(Path::new(&socket_path), allowed_uid) {
        fail(&error);
    }
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
fn probe() {
    match PhysicalDevice::capture() {
        Ok(device) => {
            let id = device.identity();
            println!(
                "arcen-usb-helper probe device={:04x}:{:04x} bcd={:04x} capture=ok",
                id.vendor_id, id.product_id, id.bcd_device
            );
            device.shutdown();
            drop(device);
            println!("arcen-usb-helper probe release=ok");
        }
        Err(error) => {
            eprintln!("arcen-usb-helper probe failed: {error}");
            std::process::exit(1);
        }
    }
}

fn serve(socket_path: &Path, allowed_uid: u32) -> Result<(), String> {
    // Remove a stale socket, but only if it is genuinely a socket, so a
    // mistyped path cannot make this root process delete an unrelated file.
    if let Ok(metadata) = std::fs::symlink_metadata(socket_path) {
        use std::os::unix::fs::FileTypeExt;
        if metadata.file_type().is_socket() {
            let _ = std::fs::remove_file(socket_path);
        } else {
            return Err(format!(
                "{} exists and is not a socket; refusing to replace it",
                socket_path.display()
            ));
        }
    }

    let listener = UnixListener::bind(socket_path)
        .map_err(|error| format!("bind {}: {error}", socket_path.display()))?;
    // The peer-uid check is the real gate; the mode and owner simply avoid
    // advertising the endpoint to every local process.
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("chmod socket: {error}"))?;
    let c_path = std::ffi::CString::new(socket_path.as_os_str().as_encoded_bytes())
        .map_err(|error| format!("socket path: {error}"))?;
    // SAFETY: `chown` on a path this process created moments ago as root.
    if unsafe { libc::chown(c_path.as_ptr(), allowed_uid, 0) } != 0 {
        return Err(format!(
            "chown socket to uid {allowed_uid}: {}",
            std::io::Error::last_os_error()
        ));
    }

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
        if let Err(error) = handle_client(&stream) {
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
fn handle_client(stream: &UnixStream) -> Result<(), String> {
    eprintln!("arcen-usb-helper: client connected; capturing device");
    let device = Arc::new(PhysicalDevice::capture()?);
    let identity = device.identity();
    eprintln!(
        "arcen-usb-helper: captured {:04x}:{:04x}",
        identity.vendor_id, identity.product_id
    );

    let mut writer = stream
        .try_clone()
        .map_err(|error| format!("clone socket: {error}"))?;
    let mut reader = std::io::BufReader::new(
        stream
            .try_clone()
            .map_err(|error| format!("clone socket: {error}"))?,
    );

    write_frame(
        &mut writer,
        TAG_HELLO,
        &encode_hello(
            identity.vendor_id,
            identity.product_id,
            identity.bcd_device,
            identity.device_class,
            speed_code(identity.speed),
        ),
    )
    .map_err(|error| error.to_string())?;

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
    // releases every interface and restores the macOS driver stack through
    // `CapturedPhysicalUsb::drop`.
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
