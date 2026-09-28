//! The contract between a Pier's network service and its desktop agents.
//!
//! A Pier that has to survive logout, user switching and a reboot with nobody
//! logged in cannot be one process running as whoever is at the screen. The
//! network half — the listener, the TLS key, admission — has to belong to the
//! machine, and the desktop half — capture, input, pasteboard, audio — has to
//! belong to the session it serves. Linux and Windows already split the two;
//! this is the vocabulary a split host uses to hand a Deck from one to the
//! other, kept here so every host that adopts the shape agrees on it.
//!
//! The exchange is one line of JSON each way, then raw bytes:
//!
//! 1. an agent connects and sends an [`AgentRegistration`];
//! 2. the service answers [`ServiceMessage::Registered`] and parks the agent;
//! 3. when a Deck is admitted the service picks one parked agent with
//!    [`select_agent`], sends it [`ServiceMessage::Attach`], waits for
//!    [`AgentMessage::Attached`], and from then on relays the Deck's stream
//!    verbatim in both directions.
//!
//! Identity is never taken from a message. The account an agent runs as comes
//! from the kernel's peer credentials, so a registration can describe what the
//! agent *is* but cannot claim who it runs as.

use arcen_telemetry::PathSignal;
use serde::{Deserialize, Serialize};

/// Version of the exchange described in this module.
pub const AGENT_RELAY_VERSION: u16 = 1;

/// Longest control line either side may send, newline included.
///
/// The lines carry a handful of short fields. A peer sending more than this is
/// broken or hostile, and reading it unbounded would let one local process
/// hold the service's memory.
pub const MAX_RELAY_LINE_BYTES: usize = 4096;

/// Which kind of graphical session an agent lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopSessionKind {
    /// A logged-in user's desktop.
    User,
    /// The login screen, before anyone has signed in.
    LoginWindow,
}

/// What an agent tells the service when it connects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRegistration {
    /// Must equal [`AGENT_RELAY_VERSION`].
    pub version: u16,
    /// The session the agent serves.
    pub kind: DesktopSessionKind,
    /// The agent's own build version, recorded for diagnostics only.
    pub agent_version: String,
    /// When set, this connection is not a parked agent but the audio side
    /// channel of the attached session with this id: after registering, it
    /// carries u32-length-prefixed audio frames that the service forwards on
    /// the Deck's audio priority stream. A side channel of its own keeps
    /// sound from waiting behind a video frame on the session socket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_for_session: Option<u64>,
}

impl AgentRegistration {
    /// Builds a registration for this build.
    #[must_use]
    pub fn new(kind: DesktopSessionKind, agent_version: impl Into<String>) -> Self {
        Self {
            version: AGENT_RELAY_VERSION,
            kind,
            agent_version: agent_version.into(),
            audio_for_session: None,
        }
    }

    /// Builds the registration for an attached session's audio side channel.
    #[must_use]
    pub fn audio_channel(
        kind: DesktopSessionKind,
        agent_version: impl Into<String>,
        session: u64,
    ) -> Self {
        Self {
            audio_for_session: Some(session),
            ..Self::new(kind, agent_version)
        }
    }
}

/// Lines the service sends to an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServiceMessage {
    /// The agent is parked and may be handed a Deck.
    Registered {
        /// The service's exchange version.
        version: u16,
    },
    /// A Deck has been admitted; everything after the agent's
    /// [`AgentMessage::Attached`] line is that Deck's stream.
    Attach {
        /// The Deck's network address, for the agent's records.
        peer: String,
        /// This session's id, which the agent's audio side channel names.
        #[serde(default)]
        session: u64,
    },
    /// Live transport signal forwarded by a split service to its desktop agent.
    /// Old agents never see this before they have accepted an attachment; old
    /// services never send it, so absence means "no path signal available".
    PathSignal {
        /// This session's id.
        #[serde(default)]
        session: u64,
        /// Latest transport sample.
        signal: PathSignal,
    },
    /// The registration was refused; the service closes the connection.
    Refused {
        /// Why, in words an operator can act on.
        reason: String,
    },
}

impl ServiceMessage {
    /// Every `type` this enum is tagged with.
    pub const TYPES: [&'static str; 4] = ["registered", "attach", "path_signal", "refused"];

    /// Whether a message `type` belongs to the service's own vocabulary to its
    /// agent. Only the service speaks it: a relay must never forward such a
    /// message when it arrived from a Deck, or a client could feed the agent
    /// a forged path signal or attach command.
    #[must_use]
    pub fn is_service_type(message_type: &str) -> bool {
        Self::TYPES.contains(&message_type)
    }
}

/// Lines an agent sends after registering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMessage {
    /// The agent accepted the Deck and is ready to read its stream.
    Attached,
}

/// Why a registration is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationError {
    /// The agent speaks a different version of this exchange.
    VersionMismatch {
        /// What the agent sent.
        agent: u16,
    },
    /// A user-session agent is running as the superuser.
    UserAgentIsRoot,
    /// A login-window agent is running as an ordinary account.
    LoginWindowAgentIsNotRoot {
        /// The account it runs as.
        uid: u32,
    },
    /// The account is one the service will never hand a Deck to.
    Forbidden {
        /// The account it runs as.
        uid: u32,
    },
}

impl std::fmt::Display for RegistrationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VersionMismatch { agent } => write!(
                formatter,
                "agent relay version {agent} is not {AGENT_RELAY_VERSION}; the agent and the \
                 service come from different installs"
            ),
            Self::UserAgentIsRoot => {
                formatter.write_str("a user-session agent must not run as root")
            }
            Self::LoginWindowAgentIsNotRoot { uid } => write!(
                formatter,
                "a login-window agent must run as root, not uid {uid}"
            ),
            Self::Forbidden { uid } => write!(formatter, "uid {uid} may not serve a desktop"),
        }
    }
}

impl std::error::Error for RegistrationError {}

/// A registered agent, as the service knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParkedAgent {
    /// The account the kernel says the agent runs as.
    pub uid: u32,
    /// The session it serves.
    pub kind: DesktopSessionKind,
    /// Increases with every registration, so the newest agent can be told
    /// from an older one for the same session.
    pub sequence: u64,
}

/// Accepts or refuses a registration against the kernel's view of the peer.
///
/// `forbidden_uid` is the service's own account: the service never hands a
/// Deck back to itself.
///
/// # Errors
///
/// Returns [`RegistrationError`] when the version differs or the claimed
/// session kind does not fit the account the kernel reports.
pub fn validate_registration(
    registration: &AgentRegistration,
    peer_uid: u32,
    forbidden_uid: Option<u32>,
    sequence: u64,
) -> Result<ParkedAgent, RegistrationError> {
    if registration.version != AGENT_RELAY_VERSION {
        return Err(RegistrationError::VersionMismatch {
            agent: registration.version,
        });
    }
    if forbidden_uid == Some(peer_uid) {
        return Err(RegistrationError::Forbidden { uid: peer_uid });
    }
    match (registration.kind, peer_uid) {
        (DesktopSessionKind::User, 0) => return Err(RegistrationError::UserAgentIsRoot),
        (DesktopSessionKind::LoginWindow, uid) if uid != 0 => {
            return Err(RegistrationError::LoginWindowAgentIsNotRoot { uid });
        }
        _ => {}
    }
    Ok(ParkedAgent {
        uid: peer_uid,
        kind: registration.kind,
        sequence,
    })
}

/// Who holds the physical console right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleHolder {
    /// A logged-in account.
    User(u32),
    /// Nobody: the login screen owns the console.
    LoginWindow,
}

/// Why no agent can take a Deck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteRefusal {
    /// Somebody is logged in but their session has no agent yet.
    UserAgentMissing {
        /// The console owner.
        uid: u32,
    },
    /// Nobody is logged in and the login screen has no agent.
    LoginWindowAgentMissing,
}

impl std::fmt::Display for RouteRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UserAgentMissing { .. } => formatter.write_str(
                "the desktop session at this Mac's console has no Arcen agent running yet; \
                 wait a few seconds after login and reconnect",
            ),
            Self::LoginWindowAgentMissing => formatter.write_str(
                "nobody is logged in at this Mac's console and the login screen is not being \
                 served; log in at the machine and reconnect",
            ),
        }
    }
}

/// Picks the agent that should receive the next Deck.
///
/// Only the session on the console is served: a background Fast User
/// Switching session cannot be captured, and handing a Deck to it would
/// authenticate someone and then show them nothing. Among several agents for
/// the console session the newest wins, because an older one is the survivor
/// of a restart that has not noticed yet.
///
/// # Errors
///
/// Returns [`RouteRefusal`] naming what is missing.
pub fn select_agent(agents: &[ParkedAgent], console: ConsoleHolder) -> Result<usize, RouteRefusal> {
    let wanted = |agent: &ParkedAgent| match console {
        ConsoleHolder::User(uid) => agent.kind == DesktopSessionKind::User && agent.uid == uid,
        ConsoleHolder::LoginWindow => agent.kind == DesktopSessionKind::LoginWindow,
    };
    agents
        .iter()
        .enumerate()
        .filter(|(_, agent)| wanted(agent))
        .max_by_key(|(_, agent)| agent.sequence)
        .map(|(index, _)| index)
        .ok_or(match console {
            ConsoleHolder::User(uid) => RouteRefusal::UserAgentMissing { uid },
            ConsoleHolder::LoginWindow => RouteRefusal::LoginWindowAgentMissing,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deck_frame_is_never_mistaken_for_the_service() {
        let samples = [
            ServiceMessage::Registered {
                version: AGENT_RELAY_VERSION,
            },
            ServiceMessage::PathSignal {
                session: 7,
                signal: PathSignal {
                    rtt_micros: 30_000,
                    baseline_rtt_micros: 30_000,
                    congestion_window_bytes: 64_000,
                    bytes_in_flight: None,
                    congestion_events_delta: 0,
                    lost_packets_delta: 0,
                    lost_bytes_delta: 0,
                    sent_packets_delta: 10,
                },
            },
            ServiceMessage::Refused {
                reason: "no".to_string(),
            },
        ];
        for sample in samples {
            let value = serde_json::to_value(&sample).expect("encode");
            let message_type = value["type"].as_str().expect("tagged");
            assert!(
                ServiceMessage::is_service_type(message_type),
                "{message_type}"
            );
        }
        assert!(ServiceMessage::is_service_type("attach"));
        assert!(!ServiceMessage::is_service_type("client_hello"));
        assert!(!ServiceMessage::is_service_type("auth_response"));
    }

    fn parked(uid: u32, kind: DesktopSessionKind, sequence: u64) -> ParkedAgent {
        ParkedAgent {
            uid,
            kind,
            sequence,
        }
    }

    #[test]
    fn identity_comes_from_the_kernel_not_the_message() {
        let registration = AgentRegistration::new(DesktopSessionKind::User, "0.12.0");
        let agent = validate_registration(&registration, 501, Some(290), 7).expect("valid");
        assert_eq!(agent.uid, 501);
        assert_eq!(agent.sequence, 7);
    }

    #[test]
    fn a_user_agent_running_as_root_is_refused() {
        let registration = AgentRegistration::new(DesktopSessionKind::User, "0.12.0");
        assert_eq!(
            validate_registration(&registration, 0, None, 1),
            Err(RegistrationError::UserAgentIsRoot)
        );
    }

    #[test]
    fn a_login_window_agent_must_be_root() {
        let registration = AgentRegistration::new(DesktopSessionKind::LoginWindow, "0.12.0");
        assert_eq!(
            validate_registration(&registration, 501, None, 1),
            Err(RegistrationError::LoginWindowAgentIsNotRoot { uid: 501 })
        );
        assert!(validate_registration(&registration, 0, None, 1).is_ok());
    }

    #[test]
    fn the_service_account_never_serves_a_desktop() {
        let registration = AgentRegistration::new(DesktopSessionKind::User, "0.12.0");
        assert_eq!(
            validate_registration(&registration, 290, Some(290), 1),
            Err(RegistrationError::Forbidden { uid: 290 })
        );
    }

    #[test]
    fn a_different_version_is_refused_by_name() {
        let mut registration = AgentRegistration::new(DesktopSessionKind::User, "0.12.0");
        registration.version = AGENT_RELAY_VERSION + 1;
        let error = validate_registration(&registration, 501, None, 1).expect_err("mismatch");
        assert!(error.to_string().contains("different installs"));
    }

    #[test]
    fn only_the_console_session_is_served() {
        let agents = [
            parked(502, DesktopSessionKind::User, 1),
            parked(501, DesktopSessionKind::User, 2),
            parked(0, DesktopSessionKind::LoginWindow, 3),
        ];
        assert_eq!(select_agent(&agents, ConsoleHolder::User(501)), Ok(1));
        assert_eq!(select_agent(&agents, ConsoleHolder::User(502)), Ok(0));
        assert_eq!(select_agent(&agents, ConsoleHolder::LoginWindow), Ok(2));
    }

    #[test]
    fn the_newest_agent_for_a_session_wins() {
        let agents = [
            parked(501, DesktopSessionKind::User, 4),
            parked(501, DesktopSessionKind::User, 9),
            parked(501, DesktopSessionKind::User, 6),
        ];
        assert_eq!(select_agent(&agents, ConsoleHolder::User(501)), Ok(1));
    }

    #[test]
    fn a_missing_agent_is_named_rather_than_guessed() {
        let agents = [parked(502, DesktopSessionKind::User, 1)];
        assert_eq!(
            select_agent(&agents, ConsoleHolder::User(501)),
            Err(RouteRefusal::UserAgentMissing { uid: 501 })
        );
        assert_eq!(
            select_agent(&agents, ConsoleHolder::LoginWindow),
            Err(RouteRefusal::LoginWindowAgentMissing)
        );
        // A background session's agent is not a fallback for the login screen.
        assert!(
            select_agent(&[], ConsoleHolder::LoginWindow)
                .expect_err("none")
                .to_string()
                .contains("log in at the machine")
        );
    }

    #[test]
    fn the_lines_have_a_stable_spelling() {
        let registration = AgentRegistration::new(DesktopSessionKind::LoginWindow, "0.12.0");
        let json = serde_json::to_string(&registration).expect("encode");
        assert_eq!(
            json,
            r#"{"version":1,"kind":"login_window","agent_version":"0.12.0"}"#
        );
        let attach = serde_json::to_string(&ServiceMessage::Attach {
            peer: "203.0.113.7:50000".to_owned(),
            session: 7,
        })
        .expect("encode");
        assert_eq!(
            attach,
            r#"{"type":"attach","peer":"203.0.113.7:50000","session":7}"#
        );
        let channel = serde_json::to_string(&AgentRegistration::audio_channel(
            DesktopSessionKind::User,
            "0.12.0",
            7,
        ))
        .expect("encode");
        assert_eq!(
            channel,
            r#"{"version":1,"kind":"user","agent_version":"0.12.0","audio_for_session":7}"#
        );
        let attached = serde_json::to_string(&AgentMessage::Attached).expect("encode");
        assert_eq!(attached, r#"{"type":"attached"}"#);
        let back: ServiceMessage =
            serde_json::from_str(r#"{"type":"registered","version":1}"#).expect("decode");
        assert_eq!(back, ServiceMessage::Registered { version: 1 });
    }
}
