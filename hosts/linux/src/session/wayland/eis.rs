//! Probe of the compositor's emulated-input (EIS) seat for a stylus.
//!
//! Pen input reaches a headless Wayland session only through an emulated
//! stylus: libei's `ei_stylus` interface (libei 1.7), offered by the
//! compositor's EIS server on a `RemoteDesktop.Session.ConnectToEIS` socket.
//! Whether the compositor offers one is a fact to establish, not to assume,
//! so the session asks: it announces `ei_stylus` in the libei handshake and
//! reads which interfaces the server negotiated and which capabilities its
//! seat offers. The connection binds nothing and is closed again; pointer and
//! keyboard stay on `RemoteDesktop` D-Bus.
//!
//! The client is `reis`, a pure-Rust implementation of the libei protocol, so
//! the answer depends only on the compositor's libeis, never on a libei
//! library on this host. `reis` 0.7 has no `ei_stylus` bindings yet: a
//! session whose compositor offers a stylus is reported as
//! [`StylusUnavailable::ClientLacksStylus`], with the negotiated version as
//! evidence, rather than claimed.

use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::OwnedFd;
use std::path::Path;
use std::time::Duration;

use arcen_input::pen_emulation::StylusUnavailable;
use futures_util::StreamExt;
use reis::ei;
use reis::PendingRequestResult;

use super::StylusFacts;
use crate::logging::target;

/// The libei interface name of an emulated stylus.
pub const EI_STYLUS: &str = "ei_stylus";

/// The `ei_stylus` version this probe announces.
const EI_STYLUS_VERSION: u32 = 1;

/// How long the compositor has to complete the handshake and describe its
/// seat.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

const CLIENT_NAME: &str = "arcen-pier stylus probe";

/// What the compositor's EIS server told the probe.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EisSeatEvidence {
    /// Interfaces the server negotiated in the handshake, with versions.
    pub negotiated: BTreeMap<String, u32>,
    /// Interfaces the first seat offers as capabilities.
    pub seat_interfaces: BTreeSet<String>,
    /// The first seat's name.
    pub seat_name: Option<String>,
}

impl EisSeatEvidence {
    /// The `ei_stylus` version the server offers, if it offers one.
    #[must_use]
    pub fn stylus_version(&self) -> Option<u32> {
        self.negotiated.get(EI_STYLUS).copied().or_else(|| {
            self.seat_interfaces
                .contains(EI_STYLUS)
                .then_some(EI_STYLUS_VERSION)
        })
    }

    /// The session's stylus facts. A stylus the server offers cannot be
    /// bound yet: this client has no `ei_stylus` bindings.
    #[must_use]
    pub fn stylus_facts(&self) -> StylusFacts {
        let version = self.stylus_version();
        StylusFacts {
            binding: None,
            unavailable: Some(if version.is_some() {
                StylusUnavailable::ClientLacksStylus
            } else {
                StylusUnavailable::StylusNotOffered
            }),
            ei_stylus_version: version,
            seat_interfaces: self.seat_interfaces.iter().cloned().collect(),
        }
    }
}

/// Why the probe could not read the seat.
#[derive(Debug, thiserror::Error)]
pub enum EisProbeError {
    /// The socket could not be used.
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
    /// The connection ended before a seat was described.
    #[error("the EIS connection ended before a seat was described")]
    Ended,
}

/// Probes the EIS server on `socket` (from `ConnectToEIS`).
///
/// # Errors
///
/// [`EisProbeError`] when the handshake or the seat description fails.
pub async fn probe(socket: OwnedFd) -> Result<EisSeatEvidence, EisProbeError> {
    let stream = std::os::unix::net::UnixStream::from(socket);
    tokio::time::timeout(PROBE_TIMEOUT, probe_stream(stream))
        .await
        .map_err(|_| EisProbeError::Timeout(PROBE_TIMEOUT))?
}

/// Probes the EIS server listening at `path` (a libei socket such as
/// `$XDG_RUNTIME_DIR/eis-0`): for qualification against libei's own demo
/// server.
///
/// # Errors
///
/// [`EisProbeError`] when the connection, handshake or seat description
/// fails.
pub async fn probe_path(path: &Path) -> Result<EisSeatEvidence, EisProbeError> {
    let stream = std::os::unix::net::UnixStream::connect(path)?;
    tokio::time::timeout(PROBE_TIMEOUT, probe_stream(stream))
        .await
        .map_err(|_| EisProbeError::Timeout(PROBE_TIMEOUT))?
}

/// The interfaces announced to the server: what a stylus sender needs.
fn announced_interfaces() -> [(&'static str, u32); 7] {
    use reis::Interface;
    [
        (ei::Connection::NAME, ei::Connection::VERSION),
        (ei::Callback::NAME, ei::Callback::VERSION),
        (ei::Pingpong::NAME, ei::Pingpong::VERSION),
        (ei::Seat::NAME, ei::Seat::VERSION),
        (ei::Device::NAME, ei::Device::VERSION),
        (ei::Button::NAME, ei::Button::VERSION),
        (EI_STYLUS, EI_STYLUS_VERSION),
    ]
}

async fn probe_stream(
    stream: std::os::unix::net::UnixStream,
) -> Result<EisSeatEvidence, EisProbeError> {
    let context = ei::Context::new(stream)?;
    let mut events = reis::tokio::EiEventStream::new(context.clone())?;
    let mut evidence = EisSeatEvidence::default();
    let mut connection: Option<ei::Connection> = None;
    let mut seat: Option<ei::Seat> = None;
    let flush = |context: &ei::Context| {
        context
            .flush()
            .map_err(|error| EisProbeError::Io(std::io::Error::from(error)))
    };
    while let Some(event) = events.next().await {
        let event = match event? {
            PendingRequestResult::Request(event) => event,
            PendingRequestResult::ParseError(error) => {
                return Err(EisProbeError::Protocol(error.to_string()));
            }
            // An object this client already released; harmless.
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
            ei::Event::Handshake(_, ei::handshake::Event::Connection { connection: c, .. }) => {
                connection = Some(c);
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
                    ..
                },
            ) => {
                return Err(EisProbeError::Disconnected(format!(
                    "{reason:?}{}",
                    explanation.map_or_else(String::new, |text| format!(": {text}"))
                )));
            }
            ei::Event::Seat(event_seat, seat_event) if seat.as_ref() == Some(&event_seat) => {
                match seat_event {
                    ei::seat::Event::Name { name } => evidence.seat_name = Some(name),
                    ei::seat::Event::Capability { interface, .. } => {
                        evidence.seat_interfaces.insert(interface);
                    }
                    ei::seat::Event::Done => {
                        if let Some(connection) = &connection {
                            connection.disconnect();
                            // The server may already have closed its end.
                            let _ = context.flush();
                        }
                        return Ok(evidence);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    Err(EisProbeError::Ended)
}

/// Whether the compositor's `RemoteDesktop` session offers `ConnectToEIS`,
/// from `introspect`, bounded like the probe: [`PROBE_TIMEOUT`] cut to the
/// agent's start `deadline`. Out of time is unknown (`None`), which skips
/// the probe.
pub async fn eis_offered_within<Fut>(
    deadline: tokio::time::Instant,
    introspect: Fut,
) -> Option<bool>
where
    Fut: std::future::Future<Output = Option<bool>>,
{
    let limit = deadline.min(tokio::time::Instant::now() + PROBE_TIMEOUT);
    if let Ok(offered) = tokio::time::timeout_at(limit, introspect).await {
        offered
    } else {
        tracing::warn!(
            target: target::INPUT,
            "introspecting RemoteDesktop for ConnectToEIS ran out of time; no stylus probe"
        );
        None
    }
}

/// The session's stylus facts from the compositor: `eis_offered` is whether
/// its `RemoteDesktop` session has `ConnectToEIS`, and `connect` opens it.
///
/// `ConnectToEIS` and the handshake share one bound: [`PROBE_TIMEOUT`], cut
/// to the agent's start `deadline` when that comes first, so the probe can
/// never push the session's ready message past its budget. A probe that
/// runs out of time is a failed connection, never a failed session.
pub async fn session_stylus_facts<F, Fut, E>(
    eis_offered: Option<bool>,
    deadline: tokio::time::Instant,
    connect: F,
) -> StylusFacts
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<OwnedFd, E>>,
    E: std::fmt::Display,
{
    match eis_offered {
        Some(false) => StylusFacts::unavailable(StylusUnavailable::NoEmulatedInput),
        None => {
            tracing::warn!(
                target: target::INPUT,
                "whether the compositor offers ConnectToEIS is unknown; the stylus probe is skipped"
            );
            StylusFacts::unavailable(StylusUnavailable::ConnectionFailed)
        }
        Some(true) => {
            let limit = deadline.min(tokio::time::Instant::now() + PROBE_TIMEOUT);
            let probed = tokio::time::timeout_at(limit, async {
                let socket = connect()
                    .await
                    .map_err(|error| format!("ConnectToEIS failed: {error}"))?;
                probe_stream(std::os::unix::net::UnixStream::from(socket))
                    .await
                    .map_err(|error| format!("EIS stylus probe failed: {error}"))
            })
            .await;
            match probed {
                Ok(Ok(evidence)) => {
                    let facts = evidence.stylus_facts();
                    tracing::info!(
                        target: target::INPUT,
                        seat = ?evidence.seat_name,
                        negotiated = ?evidence.negotiated,
                        seat_interfaces = ?evidence.seat_interfaces,
                        ei_stylus = ?facts.ei_stylus_version,
                        unavailable = ?facts.unavailable,
                        "compositor EIS seat probed for a stylus"
                    );
                    facts
                }
                Ok(Err(error)) => {
                    tracing::warn!(target: target::INPUT, %error, "no stylus");
                    StylusFacts::unavailable(StylusUnavailable::ConnectionFailed)
                }
                Err(_) => {
                    tracing::warn!(
                        target: target::INPUT,
                        "ConnectToEIS and the stylus probe ran out of time; no stylus"
                    );
                    StylusFacts::unavailable(StylusUnavailable::ConnectionFailed)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn far() -> tokio::time::Instant {
        tokio::time::Instant::now() + Duration::from_secs(60)
    }

    /// Introspection that never answers is unknown at the start deadline,
    /// and an unknown answer skips the probe.
    #[tokio::test]
    async fn an_introspection_that_never_answers_is_unknown_at_the_start_deadline() {
        let started = tokio::time::Instant::now();
        let offered = eis_offered_within(
            started + Duration::from_millis(100),
            std::future::pending::<Option<bool>>(),
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(offered, None);
        let facts = session_stylus_facts(offered, far(), || async {
            Err::<OwnedFd, &str>("never called")
        })
        .await;
        assert_eq!(facts.unavailable, Some(StylusUnavailable::ConnectionFailed));
        assert_eq!(
            eis_offered_within(far(), async { Some(true) }).await,
            Some(true)
        );
    }

    /// A compositor whose EIS server accepts the connection but never
    /// speaks costs no more than what is left of the start deadline.
    #[tokio::test]
    async fn a_silent_eis_server_is_cut_off_at_the_start_deadline() {
        let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
        let started = tokio::time::Instant::now();
        let facts =
            session_stylus_facts(Some(true), started + Duration::from_millis(200), || async {
                Ok::<OwnedFd, &str>(client.into())
            })
            .await;
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(facts.unavailable, Some(StylusUnavailable::ConnectionFailed));
        drop(server);
    }

    /// `ConnectToEIS` that never returns shares the same bound.
    #[tokio::test]
    async fn a_connect_that_never_returns_is_cut_off_at_the_start_deadline() {
        let started = tokio::time::Instant::now();
        let facts = session_stylus_facts(
            Some(true),
            started + Duration::from_millis(100),
            std::future::pending::<Result<OwnedFd, &str>>,
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(facts.unavailable, Some(StylusUnavailable::ConnectionFailed));
    }

    /// Connect and handshake together never exceed one probe timeout, even
    /// with a far deadline.
    #[tokio::test]
    async fn connect_and_handshake_share_one_probe_timeout() {
        let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
        let started = tokio::time::Instant::now();
        let facts = session_stylus_facts(Some(true), far(), || async {
            tokio::time::sleep(PROBE_TIMEOUT - Duration::from_millis(500)).await;
            Ok::<OwnedFd, &str>(client.into())
        })
        .await;
        assert!(started.elapsed() <= PROBE_TIMEOUT + Duration::from_millis(250));
        assert_eq!(facts.unavailable, Some(StylusUnavailable::ConnectionFailed));
        drop(server);
    }

    #[test]
    fn a_seat_without_a_stylus_is_reported_not_offered_with_its_interfaces() {
        let evidence = EisSeatEvidence {
            negotiated: [("ei_seat".to_owned(), 2), ("ei_device".to_owned(), 3)]
                .into_iter()
                .collect(),
            seat_interfaces: ["ei_pointer_absolute".to_owned(), "ei_button".to_owned()]
                .into_iter()
                .collect(),
            seat_name: Some("mutter default seat".to_owned()),
        };
        let facts = evidence.stylus_facts();
        assert_eq!(facts.binding, None);
        assert_eq!(facts.unavailable, Some(StylusUnavailable::StylusNotOffered));
        assert_eq!(facts.ei_stylus_version, None);
        assert_eq!(facts.seat_interfaces, ["ei_button", "ei_pointer_absolute"]);
    }

    #[test]
    fn an_offered_stylus_is_evidence_but_never_claimed_without_bindings() {
        let evidence = EisSeatEvidence {
            negotiated: [(EI_STYLUS.to_owned(), 1)].into_iter().collect(),
            seat_interfaces: [EI_STYLUS.to_owned()].into_iter().collect(),
            seat_name: None,
        };
        let facts = evidence.stylus_facts();
        assert_eq!(facts.binding, None);
        assert_eq!(
            facts.unavailable,
            Some(StylusUnavailable::ClientLacksStylus)
        );
        assert_eq!(facts.ei_stylus_version, Some(1));
    }

    #[tokio::test]
    async fn a_compositor_without_connect_to_eis_has_no_emulated_input() {
        let facts = session_stylus_facts(Some(false), far(), || async {
            Err::<OwnedFd, &str>("never called")
        })
        .await;
        assert_eq!(facts.unavailable, Some(StylusUnavailable::NoEmulatedInput));
    }

    #[tokio::test]
    async fn a_refused_connect_to_eis_is_a_failed_connection() {
        let facts = session_stylus_facts(Some(true), far(), || async {
            Err::<OwnedFd, &str>("refused")
        })
        .await;
        assert_eq!(facts.unavailable, Some(StylusUnavailable::ConnectionFailed));
    }

    /// A server that closes without speaking ends the probe rather than
    /// hanging it.
    #[tokio::test]
    async fn a_silent_server_that_closes_ends_the_probe() {
        let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
        drop(server);
        let result = probe(client.into()).await;
        assert!(matches!(
            result,
            Err(EisProbeError::Ended | EisProbeError::Io(_))
        ));
    }

    /// A minimal in-process EIS server (reis's server side): negotiates the
    /// interfaces it supports from what the client announced, then describes
    /// one seat with `capabilities`. Returns what the client announced.
    fn serve_seat(
        socket: std::os::unix::net::UnixStream,
        supported: &'static [&'static str],
        capabilities: &'static [&'static str],
    ) -> std::thread::JoinHandle<Vec<String>> {
        use reis::eis;
        std::thread::spawn(move || {
            let context = eis::Context::new(socket).unwrap();
            context.handshake().handshake_version(1);
            context.flush().unwrap();
            let mut announced = Vec::new();
            let started = std::time::Instant::now();
            while started.elapsed() < Duration::from_secs(5) {
                match context.read() {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
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
                        ) => announced.push(format!("{name}:{version}")),
                        eis::Request::Handshake(handshake, eis::handshake::Request::Finish) => {
                            for entry in &announced {
                                let (name, version) = entry.split_once(':').unwrap();
                                if supported.contains(&name) {
                                    handshake.interface_version(name, version.parse().unwrap());
                                }
                            }
                            let connection = handshake.connection(1, 1);
                            let seat = connection.seat(1);
                            seat.name("test seat");
                            for (bit, interface) in capabilities.iter().enumerate() {
                                seat.capability(1 << bit, interface);
                            }
                            seat.done();
                            context.flush().unwrap();
                        }
                        eis::Request::Connection(_, eis::connection::Request::Disconnect) => {
                            return announced;
                        }
                        _ => {}
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            announced
        })
    }

    #[tokio::test]
    async fn the_probe_announces_a_stylus_and_reads_a_seat_that_offers_one() {
        let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
        let server = serve_seat(
            server,
            &[
                "ei_connection",
                "ei_callback",
                "ei_pingpong",
                "ei_seat",
                "ei_device",
                "ei_button",
                EI_STYLUS,
            ],
            &["ei_button", EI_STYLUS],
        );
        let evidence = probe(client.into()).await.unwrap();
        assert_eq!(evidence.stylus_version(), Some(1));
        assert_eq!(evidence.seat_name.as_deref(), Some("test seat"));
        let announced = server.join().unwrap();
        assert!(announced.contains(&"ei_stylus:1".to_owned()));
        assert_eq!(
            evidence.stylus_facts().unavailable,
            Some(StylusUnavailable::ClientLacksStylus)
        );
    }

    #[tokio::test]
    async fn a_seat_like_mutter_49_offers_no_stylus() {
        let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
        let server = serve_seat(
            server,
            &[
                "ei_connection",
                "ei_callback",
                "ei_pingpong",
                "ei_seat",
                "ei_device",
                "ei_button",
            ],
            &[
                "ei_pointer",
                "ei_pointer_absolute",
                "ei_keyboard",
                "ei_touchscreen",
                "ei_scroll",
                "ei_button",
            ],
        );
        let evidence = probe(client.into()).await.unwrap();
        server.join().unwrap();
        assert_eq!(evidence.stylus_version(), None);
        let facts = evidence.stylus_facts();
        assert_eq!(facts.unavailable, Some(StylusUnavailable::StylusNotOffered));
        assert!(facts.seat_interfaces.contains(&"ei_touchscreen".to_owned()));
    }
}
