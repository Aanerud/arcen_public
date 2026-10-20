//! The session's connection to its compositor's EIS server, for committed
//! text that the session's keyboard layout cannot type.
//!
//! libei 1.6 added `ei_text`: a device whose `utf8` request carries a
//! committed string, which a compositor may hand to the focused application
//! through its input method (Mutter MR !4996 does: an input-method commit).
//! It is the only route by which a remote client can type characters the
//! session's layout lacks (emoji, another script) into a GNOME Wayland
//! session. Whether the compositor offers it is established, not assumed:
//! the session connects through `RemoteDesktop.Session.ConnectToEIS`,
//! announces `ei_text`, and binds the seat's text capability only when the
//! seat offers one. Until then committed text the layout lacks is refused
//! and reported, with the evidence (the negotiated interfaces and the seat's
//! capabilities) in the log.
//!
//! This is the session's one long-lived EIS connection: other emulated
//! devices (the pen package's stylus) bind on it rather than opening their
//! own. Pointer and keyboard stay on `RemoteDesktop` D-Bus. The client is
//! `reis`, a pure-Rust implementation of the libei protocol, so what counts
//! is the compositor's libeis, never a libei library on this host.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use reis::{ei, PendingRequestResult};

use super::text::{SessionText, Utf8Committer, Utf8Route};
use crate::logging::target;

/// The libei interface name of a text device.
pub const EI_TEXT: &str = "ei_text";
/// The `ei_text` version this client speaks.
const EI_TEXT_VERSION: u32 = 1;
/// How long the compositor has to complete the handshake and describe its
/// seat.
pub const SEAT_TIMEOUT: Duration = Duration::from_secs(5);
const CLIENT_NAME: &str = "arcen-pier committed text";

/// Why the connection ended.
#[derive(Debug, thiserror::Error)]
pub enum EisTextError {
    /// The socket failed.
    #[error("EIS socket: {0}")]
    Io(#[from] std::io::Error),
    /// The server disconnected.
    #[error("the EIS server disconnected: {0}")]
    Disconnected(String),
    /// The server sent something this client cannot parse.
    #[error("the EIS server sent an unreadable message: {0}")]
    Protocol(String),
    /// The server did not describe a seat in time.
    #[error("the EIS server did not describe a seat within {0:?}")]
    Timeout(Duration),
    /// The connection ended.
    #[error("the EIS connection ended")]
    Ended,
}

/// What the compositor's EIS server offered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EisTextEvidence {
    /// Interfaces negotiated in the handshake, with versions.
    pub negotiated: BTreeMap<String, u32>,
    /// The first seat's capabilities.
    pub seat_interfaces: BTreeSet<String>,
}

impl EisTextEvidence {
    /// Whether the seat offers a text device.
    #[must_use]
    pub fn offers_text(&self) -> bool {
        self.seat_interfaces.contains(EI_TEXT)
    }

    /// Why the seat gives no text device. A compositor describes a seat
    /// only in interfaces both sides negotiated, so the negotiated list is
    /// the evidence: without `ei_text` in it, the compositor lacks it.
    #[must_use]
    pub fn no_text_reason(&self) -> String {
        let negotiated = self
            .negotiated
            .iter()
            .map(|(name, version)| format!("{name} v{version}"))
            .collect::<Vec<_>>()
            .join(", ");
        if self.negotiated.contains_key(EI_TEXT) {
            format!("the EIS seat offers no ei_text device (negotiated: {negotiated})")
        } else {
            format!("the compositor's EIS does not implement ei_text (negotiated: {negotiated})")
        }
    }
}

/// The interfaces announced to the server.
fn announced_interfaces() -> [(&'static str, u32); 6] {
    use reis::Interface;
    [
        (ei::Connection::NAME, ei::Connection::VERSION),
        (ei::Callback::NAME, ei::Callback::VERSION),
        (ei::Pingpong::NAME, ei::Pingpong::VERSION),
        (ei::Seat::NAME, ei::Seat::VERSION),
        (ei::Device::NAME, ei::Device::VERSION),
        (EI_TEXT, EI_TEXT_VERSION),
    ]
}

/// A resumed `ei_text` device, shared with the input service.
struct TextDevice {
    context: ei::Context,
    device: ei::Device,
    text: ei::Text,
    sequence: u32,
}

/// Commits text through the compositor's `ei_text` device.
pub struct EisTextCommitter {
    device: Mutex<Option<TextDevice>>,
    serial: Arc<AtomicU32>,
}

impl Utf8Committer for EisTextCommitter {
    fn commit(&self, chunk: &str) -> Result<(), String> {
        let mut device = self
            .device
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let device = device
            .as_mut()
            .ok_or_else(|| "the EIS text device is paused".to_owned())?;
        let serial = self.serial.load(Ordering::Relaxed);
        device.sequence = device.sequence.wrapping_add(1);
        device.device.start_emulating(serial, device.sequence);
        device.text.utf8(chunk);
        device.device.frame(serial, timestamp_us());
        device.device.stop_emulating(serial);
        device
            .context
            .flush()
            .map_err(|error| format!("EIS flush: {error}"))
    }
}

fn timestamp_us() -> u64 {
    super::monotonic_now_ns() / 1_000
}

#[derive(Default)]
struct DeviceState {
    interfaces: BTreeMap<String, reis::Object>,
}

/// Runs the session's EIS connection on `stream` until it ends, keeping
/// `text`'s input-method route current: offered while the compositor's text
/// device is resumed, not offered otherwise.
///
/// # Errors
///
/// [`EisTextError`] when the connection fails or ends.
pub async fn run(stream: UnixStream, text: Arc<SessionText>) -> Result<(), EisTextError> {
    let context = ei::Context::new(stream)?;
    let mut events = reis::tokio::EiEventStream::new(context.clone())?;
    let serial = Arc::new(AtomicU32::new(0));
    let committer = Arc::new(EisTextCommitter {
        device: Mutex::new(None),
        serial: Arc::clone(&serial),
    });
    let mut evidence = EisTextEvidence::default();
    let mut seat: Option<ei::Seat> = None;
    let mut seat_done = false;
    let mut text_mask: Option<u64> = None;
    let mut devices: HashMap<ei::Device, DeviceState> = HashMap::new();
    let flush = |context: &ei::Context| {
        context
            .flush()
            .map_err(|error| EisTextError::Io(std::io::Error::from(error)))
    };
    let deadline = tokio::time::Instant::now() + SEAT_TIMEOUT;
    let outcome = loop {
        let next = if seat_done {
            events.next().await
        } else {
            match tokio::time::timeout_at(deadline, events.next()).await {
                Ok(next) => next,
                Err(_) => break Err(EisTextError::Timeout(SEAT_TIMEOUT)),
            }
        };
        let Some(event) = next else {
            break Err(EisTextError::Ended);
        };
        let event = match event? {
            PendingRequestResult::Request(event) => event,
            PendingRequestResult::ParseError(error) => {
                break Err(EisTextError::Protocol(error.to_string()));
            }
            PendingRequestResult::InvalidObject(_) => continue,
        };
        match event {
            ei::Event::Handshake(handshake, ei::handshake::Event::HandshakeVersion { .. }) => {
                handshake.handshake_version(1);
                handshake.name(CLIENT_NAME);
                handshake.context_type(ei::handshake::ContextType::Sender);
                for (name, version) in announced_interfaces() {
                    handshake.interface_version(name, version);
                }
                handshake.finish();
                flush(&context)?;
            }
            ei::Event::Handshake(_, ei::handshake::Event::InterfaceVersion { name, version }) => {
                evidence.negotiated.insert(name, version);
            }
            ei::Event::Handshake(_, ei::handshake::Event::Connection { serial: s, .. }) => {
                serial.store(s, Ordering::Relaxed);
            }
            ei::Event::Connection(_, ei::connection::Event::Seat { seat: s }) => {
                seat.get_or_insert(s);
            }
            ei::Event::Connection(_, ei::connection::Event::Ping { ping }) => {
                ping.done(0);
                flush(&context)?;
            }
            ei::Event::Connection(
                _,
                ei::connection::Event::Disconnected {
                    reason,
                    explanation,
                    last_serial,
                },
            ) => {
                serial.store(last_serial, Ordering::Relaxed);
                break Err(EisTextError::Disconnected(format!(
                    "{reason:?}{}",
                    explanation.map_or_else(String::new, |text| format!(": {text}"))
                )));
            }
            ei::Event::Seat(event_seat, seat_event) if seat.as_ref() == Some(&event_seat) => {
                match seat_event {
                    ei::seat::Event::Capability { mask, interface } => {
                        // The bit is bound once the seat is described.
                        if interface == EI_TEXT {
                            text_mask = Some(mask);
                        }
                        evidence.seat_interfaces.insert(interface);
                    }
                    ei::seat::Event::Done => {
                        seat_done = true;
                        text.set_eis_evidence(
                            evidence.negotiated.get(EI_TEXT).copied(),
                            evidence.seat_interfaces.iter().cloned().collect(),
                        );
                        tracing::info!(
                            target: target::INPUT,
                            negotiated = ?evidence.negotiated,
                            seat_interfaces = ?evidence.seat_interfaces,
                            ei_text = evidence.offers_text(),
                            "compositor EIS seat read for committed text"
                        );
                        if let Some(mask) = text_mask {
                            event_seat.bind(mask);
                            flush(&context)?;
                            text.set_eis_error(Some(
                                "text capability bound; waiting for the device".to_owned(),
                            ));
                        } else {
                            text.set_utf8(Utf8Route::NotOffered, None);
                            text.set_eis_error(Some(evidence.no_text_reason()));
                            tracing::info!(
                                target: target::INPUT,
                                "the compositor's EIS seat offers no ei_text: characters the session layout lacks are refused"
                            );
                            break Ok(());
                        }
                    }
                    ei::seat::Event::Device { device } => {
                        devices.insert(device, DeviceState::default());
                    }
                    _ => {}
                }
            }
            ei::Event::Device(device, device_event) => match device_event {
                // `ei_device` v3 servers (libei 1.6) wait for this before
                // resuming the device.
                ei::device::Event::Done if device.version() >= 3 => {
                    device.ready();
                    flush(&context)?;
                }
                ei::device::Event::Interface { object } => {
                    if let Some(state) = devices.get_mut(&device) {
                        state
                            .interfaces
                            .insert(object.interface().to_owned(), object);
                    }
                }
                ei::device::Event::Resumed { serial: s } => {
                    serial.store(s, Ordering::Relaxed);
                    let text_object = devices
                        .get(&device)
                        .and_then(|state| state.interfaces.get(EI_TEXT).cloned())
                        .and_then(|object| object.downcast::<ei::Text>());
                    if let Some(text_interface) = text_object {
                        *committer
                            .device
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(TextDevice {
                                context: context.clone(),
                                device: device.clone(),
                                text: text_interface,
                                sequence: 0,
                            });
                        text.set_utf8(
                            Utf8Route::Offered,
                            Some(Arc::clone(&committer) as Arc<dyn Utf8Committer>),
                        );
                        tracing::info!(
                            target: target::INPUT,
                            "compositor EIS text device resumed: characters the session layout lacks are committed through its input method"
                        );
                        text.set_eis_error(None);
                    }
                }
                ei::device::Event::Paused { serial: s } => {
                    serial.store(s, Ordering::Relaxed);
                    *committer
                        .device
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                }
                ei::device::Event::Destroyed { serial: s } => {
                    serial.store(s, Ordering::Relaxed);
                    devices.remove(&device);
                    *committer
                        .device
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                    text.set_utf8(Utf8Route::NotOffered, None);
                }
                _ => {}
            },
            _ => {}
        }
    };
    *committer
        .device
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    if text.utf8_route() == Utf8Route::Offered {
        text.set_utf8(Utf8Route::NotOffered, None);
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use reis::eis;

    /// A minimal in-process EIS server (reis's server side). It negotiates
    /// what it supports from what the client announced and describes one
    /// seat with `capabilities`; when the client binds `ei_text`, it adds a
    /// text device and resumes it. Returns every `utf8` string it received
    /// until the client goes away or `expect` strings arrived.
    fn serve(
        socket: UnixStream,
        capabilities: &'static [&'static str],
        expect: usize,
    ) -> std::thread::JoinHandle<Vec<String>> {
        std::thread::spawn(move || {
            let context = eis::Context::new(socket).unwrap();
            context.handshake().handshake_version(1);
            context.flush().unwrap();
            let mut announced = Vec::new();
            let mut received = Vec::new();
            let mut seat = None;
            let started = std::time::Instant::now();
            while started.elapsed() < Duration::from_secs(5)
                && (expect == 0 || received.len() < expect)
            {
                match context.read() {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => panic!("{error}"),
                }
                while let Some(request) = context.pending_request() {
                    let PendingRequestResult::Request(request) = request else {
                        panic!("unreadable client request");
                    };
                    match request {
                        eis::Request::Handshake(
                            _,
                            eis::handshake::Request::InterfaceVersion { name, version },
                        ) => announced.push((name, version)),
                        eis::Request::Handshake(handshake, eis::handshake::Request::Finish) => {
                            for (name, version) in &announced {
                                if *name != EI_TEXT || capabilities.contains(&EI_TEXT) {
                                    handshake.interface_version(name, *version);
                                }
                            }
                            let connection = handshake.connection(1, 1);
                            let new_seat = connection.seat(1);
                            new_seat.name("test seat");
                            for (bit, interface) in capabilities.iter().enumerate() {
                                new_seat.capability(1 << bit, interface);
                            }
                            new_seat.done();
                            seat = Some(new_seat);
                            context.flush().unwrap();
                        }
                        eis::Request::Seat(_, eis::seat::Request::Bind { capabilities: mask }) => {
                            let bit = capabilities.iter().position(|c| *c == EI_TEXT).unwrap();
                            assert_eq!(mask, 1 << bit, "only the text capability is bound");
                            // Like libei 1.6: an `ei_device` v3 device is
                            // resumed only once the client says it is ready.
                            let version = announced
                                .iter()
                                .find(|(name, _)| name == "ei_device")
                                .map_or(1, |(_, version)| (*version).min(3));
                            let device = seat.as_ref().unwrap().device(version);
                            device.name("virtual text");
                            device.device_type(eis::device::DeviceType::Virtual);
                            let _text: eis::Text = device.interface(1);
                            device.done();
                            if version < 3 {
                                device.resumed(7);
                            }
                            context.flush().unwrap();
                        }
                        eis::Request::Device(device, eis::device::Request::Ready) => {
                            device.resumed(7);
                            context.flush().unwrap();
                        }
                        eis::Request::Text(_, eis::text::Request::Utf8 { text }) => {
                            received.push(text);
                        }
                        eis::Request::Connection(_, eis::connection::Request::Disconnect) => {
                            return received;
                        }
                        _ => {}
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            received
        })
    }

    #[tokio::test]
    async fn a_seat_with_ei_text_commits_utf8_chunks() {
        let (client, server) = UnixStream::pair().unwrap();
        let server = serve(server, &["ei_keyboard", EI_TEXT], 2);
        let text = Arc::new(SessionText::default());
        let running = tokio::spawn(run(client, Arc::clone(&text)));
        let started = tokio::time::Instant::now();
        while text.utf8_route() != Utf8Route::Offered {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "text device in time"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let (plan, outcome) = text.plan("😀日本語");
        assert_eq!(outcome.utf8_chars, 4);
        for run in &plan.runs {
            if let arcen_input::text::TextRun::Utf8(chunks) = run {
                for chunk in chunks {
                    text.commit_utf8(chunk).unwrap();
                }
            }
        }
        let long = "日".repeat(100);
        for chunk in arcen_input::text::utf8_chunks(&long, arcen_input::text::MAX_UTF8_CHUNK_BYTES)
        {
            assert!(chunk.len() <= arcen_input::text::MAX_UTF8_CHUNK_BYTES);
        }
        text.commit_utf8("ø").unwrap();
        let received = server.join().unwrap();
        assert_eq!(received, ["😀日本語", "ø"]);
        running.abort();
    }

    /// Lab proof against libei's own `eis-demo-server` (which prints each
    /// `utf8` event it receives): set `ARCEN_EIS_DEMO_SOCKET` to its socket
    /// and run with `--ignored`.
    #[tokio::test]
    #[ignore = "needs a running libei eis-demo-server"]
    async fn libei_demo_server_receives_committed_text() {
        let path = std::env::var("ARCEN_EIS_DEMO_SOCKET").unwrap();
        let text = Arc::new(SessionText::default());
        let running = tokio::spawn(run(UnixStream::connect(path).unwrap(), Arc::clone(&text)));
        let started = tokio::time::Instant::now();
        while text.utf8_route() != Utf8Route::Offered {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{:?}",
                text.facts()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let facts = text.facts();
        assert_eq!(facts.ei_text_version, Some(1));
        assert!(facts.eis_seat.iter().any(|interface| interface == EI_TEXT));
        for chunk in ["æøå", "😀", "日本語"] {
            text.commit_utf8(chunk).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        running.abort();
    }

    #[test]
    fn a_compositor_without_ei_text_is_named_from_the_handshake() {
        let mut evidence = EisTextEvidence::default();
        evidence.negotiated.insert("ei_connection".to_owned(), 1);
        evidence.negotiated.insert("ei_seat".to_owned(), 1);
        assert_eq!(
            evidence.no_text_reason(),
            "the compositor's EIS does not implement ei_text (negotiated: ei_connection v1, ei_seat v1)"
        );
        evidence.negotiated.insert(EI_TEXT.to_owned(), 1);
        assert!(evidence
            .no_text_reason()
            .starts_with("the EIS seat offers no ei_text device"));
    }

    #[tokio::test]
    async fn a_seat_like_mutter_49_offers_no_text() {
        let (client, server) = UnixStream::pair().unwrap();
        let server = serve(
            server,
            &[
                "ei_pointer",
                "ei_pointer_absolute",
                "ei_keyboard",
                "ei_touchscreen",
            ],
            0,
        );
        let text = Arc::new(SessionText::default());
        text.set_utf8(Utf8Route::NotOffered, None);
        run(client, Arc::clone(&text)).await.unwrap();
        assert_eq!(text.utf8_route(), Utf8Route::NotOffered);
        assert!(text
            .facts()
            .eis_error
            .is_some_and(|reason| reason.contains("ei_text")));
        let (_, outcome) = text.plan("😀");
        assert_eq!(outcome.utf8_chars, 0);
        drop(server);
    }
}
