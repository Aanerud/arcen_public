#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientState {
    Disconnected,
    Handshaking,
    Authenticating,
    CapabilityExchange,
    Streaming,
    Degraded,
    Reconnecting,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerState {
    Bootstrapping,
    Authenticating,
    CapabilityExchange,
    Streaming,
    Reconfiguring,
    Draining,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientEvent {
    ConnectRequested,
    TlsOk,
    TlsFailed,
    AuthOk,
    AuthFailed,
    AuthTimeout,
    HelloReceived,
    ProtoMismatch,
    HealthDegraded,
    HealthRestored,
    TransportLost,
    MaxRetries,
    UserQuit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransitionNotAllowed {
    pub state: ClientState,
    pub event: ClientEvent,
}

pub const CLIENT_STATES: &[&str] = &[
    "disconnected",
    "handshaking",
    "authenticating",
    "capability_exchange",
    "streaming",
    "degraded",
    "reconnecting",
    "closed",
];

pub const SERVER_STATES: &[&str] = &[
    "bootstrapping",
    "authenticating",
    "capability_exchange",
    "streaming",
    "reconfiguring",
    "draining",
    "closed",
];

pub const ALLOWED_PAIRS: &[(&str, &str)] = &[
    ("handshaking", "bootstrapping"),
    ("authenticating", "authenticating"),
    ("capability_exchange", "capability_exchange"),
    ("streaming", "streaming"),
    ("streaming", "reconfiguring"),
    ("degraded", "streaming"),
    ("reconnecting", "draining"),
    ("reconnecting", "closed"),
    ("closed", "closed"),
];

impl ClientState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disconnected => "disconnected",
            Self::Handshaking => "handshaking",
            Self::Authenticating => "authenticating",
            Self::CapabilityExchange => "capability_exchange",
            Self::Streaming => "streaming",
            Self::Degraded => "degraded",
            Self::Reconnecting => "reconnecting",
            Self::Closed => "closed",
        }
    }
}

impl ServerState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bootstrapping => "bootstrapping",
            Self::Authenticating => "authenticating",
            Self::CapabilityExchange => "capability_exchange",
            Self::Streaming => "streaming",
            Self::Reconfiguring => "reconfiguring",
            Self::Draining => "draining",
            Self::Closed => "closed",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ClientFsm {
    state: ClientState,
}

impl Default for ClientFsm {
    fn default() -> Self {
        Self {
            state: ClientState::Disconnected,
        }
    }
}

impl ClientFsm {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> ClientState {
        self.state
    }

    pub fn state_id(&self) -> &'static str {
        self.state.as_str()
    }

    pub fn send(&mut self, event: ClientEvent) -> Result<ClientState, TransitionNotAllowed> {
        use ClientEvent::*;
        use ClientState::*;

        let next = match (self.state, event) {
            (Disconnected, ConnectRequested) | (Reconnecting, ConnectRequested) => Handshaking,
            (Handshaking, TlsOk) => Authenticating,
            (Handshaking, TlsFailed) => Reconnecting,
            (Authenticating, AuthOk) => CapabilityExchange,
            (Authenticating, AuthFailed) | (CapabilityExchange, ProtoMismatch) => Disconnected,
            (Authenticating, AuthTimeout) => Reconnecting,
            (CapabilityExchange, HelloReceived) => Streaming,
            (Streaming, HealthDegraded) => Degraded,
            (Degraded, HealthRestored) => Streaming,
            (Streaming, TransportLost)
            | (Degraded, TransportLost)
            | (Handshaking, TransportLost)
            | (Authenticating, TransportLost)
            | (CapabilityExchange, TransportLost) => Reconnecting,
            (Reconnecting, MaxRetries) => Closed,
            (Disconnected, UserQuit)
            | (Handshaking, UserQuit)
            | (Authenticating, UserQuit)
            | (CapabilityExchange, UserQuit)
            | (Streaming, UserQuit)
            | (Degraded, UserQuit)
            | (Reconnecting, UserQuit) => Closed,
            _ => {
                return Err(TransitionNotAllowed {
                    state: self.state,
                    event,
                })
            }
        };
        self.state = next;
        Ok(next)
    }
}

pub fn is_state_pair_allowed(client_state: &str, server_state: &str) -> bool {
    if !CLIENT_STATES.contains(&client_state) || !SERVER_STATES.contains(&server_state) {
        return false;
    }
    ALLOWED_PAIRS
        .iter()
        .any(|(client, server)| *client == client_state && *server == server_state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_lifecycle_matches_python_contract() {
        let mut fsm = ClientFsm::new();
        assert_eq!(fsm.state_id(), "disconnected");
        fsm.send(ClientEvent::ConnectRequested).unwrap();
        assert_eq!(fsm.state_id(), "handshaking");
        fsm.send(ClientEvent::TlsOk).unwrap();
        assert_eq!(fsm.state_id(), "authenticating");
        fsm.send(ClientEvent::AuthOk).unwrap();
        assert_eq!(fsm.state_id(), "capability_exchange");
        fsm.send(ClientEvent::HelloReceived).unwrap();
        assert_eq!(fsm.state_id(), "streaming");
    }

    #[test]
    fn rejects_unknown_state_pairs() {
        assert!(is_state_pair_allowed("streaming", "streaming"));
        assert!(!is_state_pair_allowed("streaming", "closed"));
        assert!(!is_state_pair_allowed("bogus", "streaming"));
    }
}
