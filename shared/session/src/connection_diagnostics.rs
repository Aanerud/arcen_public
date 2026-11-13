// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Connection diagnostics: the step model, typed findings and the wording
//! behind `arcen-deck diagnose` and the Deck's **Test connection**.
//!
//! A platform adapter does the probing — resolve the name, send one empty UDP
//! datagram, attempt a QUIC/TLS handshake, read the Pier's first message
//! before sign-in, read the outgoing interface — and records what it saw as
//! [`Observations`]. [`assess`] turns those facts into a [`DiagnosticReport`]
//! whose every finding has a stable code (documented in
//! `docs/operations/error-codes.md`), a verdict, one plain sentence and what
//! to do about it.
//!
//! No step needs credentials and none is ever sent. A Pier discloses its
//! version, platform and capabilities only after sign-in, so the greeting step
//! reports what the Pier says *before* sign-in and stops there.

use std::fmt::Write as _;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};

use arcen_telemetry::QosTargets;

use crate::network_reachability::{ReachabilityFailure, ReachabilityScope};

/// The Pier's default UDP port.
pub const DEFAULT_PIER_PORT: u16 = 18_444;

/// Bytes of UDP payload in Arcen's QUIC packets. Packets start at the
/// protocol minimum and stay there unless path MTU discovery is switched on
/// and proves a larger size, so a path that carries the handshake carries the
/// stream.
pub const ARCEN_QUIC_PACKET_BYTES: u32 = 1_200;

/// Certificates expiring within this many days earn a warning.
pub const CERTIFICATE_EXPIRY_WARNING_DAYS: u64 = 30;

/// Version of the plain-text report layout.
pub const REPORT_FORMAT_VERSION: u32 = 1;

/// Where the codes in a report are explained.
pub const ERROR_CODES_DOCUMENT: &str = "docs/operations/error-codes.md";

/// Most characters of one network-supplied string a report repeats.
pub const MAX_UNTRUSTED_CHARS: usize = 120;

/// Most sign-in methods a report lists.
const MAX_LISTED_METHODS: usize = 8;

/// Whether `character` could change how a terminal or a text view shows the
/// text around it: control characters (ESC begins terminal sequences),
/// Unicode bidirectional overrides and isolates, and invisible separators.
fn is_unsafe_to_print(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061C}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2069}'
                | '\u{FEFF}'
        )
}

/// Text that came from the network, made safe to print and copy.
///
/// Before sign-in on a first connection the Pier's identity is unverified, so
/// anything it says — message types, close reasons, sign-in methods, names —
/// may come from whoever is on the path. Unsafe characters are shown as
/// visible `\u{..}` escapes rather than interpreted, so an answer cannot
/// rewrite the fingerprint lines a user is asked to compare, and the text is
/// cut to [`MAX_UNTRUSTED_CHARS`].
#[must_use]
pub fn untrusted_text(text: &str) -> String {
    let mut safe = String::new();
    for (count, character) in text.chars().enumerate() {
        if count == MAX_UNTRUSTED_CHARS {
            safe.push('…');
            break;
        }
        push_printable(&mut safe, character);
    }
    safe
}

fn push_printable(output: &mut String, character: char) {
    if is_unsafe_to_print(character) {
        let _ = write!(output, "\\u{{{:x}}}", u32::from(character));
    } else {
        output.push(character);
    }
}

/// Escapes unsafe characters without shortening: the last guard on every
/// line a report prints.
fn printable_line(line: &str) -> String {
    let mut safe = String::with_capacity(line.len());
    for character in line.chars() {
        push_printable(&mut safe, character);
    }
    safe
}

/// A Pier address as typed: `host`, `host:port`, `[v6]:port` or a bare IPv6
/// address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticTarget {
    /// Host name or IP literal, without brackets.
    pub host: String,
    /// UDP port.
    pub port: u16,
}

/// Why an address could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetError {
    /// Nothing was typed.
    Empty,
    /// The host part is not a host name or IP address.
    InvalidHost,
    /// The port is not a number from 1 to 65535.
    InvalidPort,
}

impl std::fmt::Display for TargetError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "enter the Pier's address",
            Self::InvalidHost => {
                "the address is not a host name or IP address (write IPv6 with a port as [address]:port)"
            }
            Self::InvalidPort => "the port must be a number from 1 to 65535",
        })
    }
}

impl std::error::Error for TargetError {}

impl DiagnosticTarget {
    /// Reads `host`, `host:port`, `[v6]:port` or a bare IPv6 address. The
    /// port defaults to [`DEFAULT_PIER_PORT`].
    ///
    /// # Errors
    ///
    /// Returns [`TargetError`] for an empty input, an invalid host or port.
    pub fn parse(input: &str) -> Result<Self, TargetError> {
        Self::parse_with_port(input, None)
    }

    /// Like [`Self::parse`], with `port` used when the input names none.
    ///
    /// # Errors
    ///
    /// Returns [`TargetError`] for an empty input, an invalid host or port.
    pub fn parse_with_port(input: &str, port: Option<u16>) -> Result<Self, TargetError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(TargetError::Empty);
        }
        let default_port = port.unwrap_or(DEFAULT_PIER_PORT);
        let (host, port) = if let Some(rest) = input.strip_prefix('[') {
            let (address, after) = rest.split_once(']').ok_or(TargetError::InvalidHost)?;
            address
                .parse::<Ipv6Addr>()
                .map_err(|_| TargetError::InvalidHost)?;
            let port = match after {
                "" => default_port,
                other => parse_port(other.strip_prefix(':').ok_or(TargetError::InvalidPort)?)?,
            };
            (address.to_owned(), port)
        } else if input.parse::<IpAddr>().is_ok() {
            (input.to_owned(), default_port)
        } else if let Some((host, port)) = input.rsplit_once(':') {
            if host.contains(':') {
                return Err(TargetError::InvalidHost);
            }
            (host.to_owned(), parse_port(port)?)
        } else {
            (input.to_owned(), default_port)
        };
        if host.parse::<IpAddr>().is_err() && !is_host_name(&host) {
            return Err(TargetError::InvalidHost);
        }
        Ok(Self { host, port })
    }

    /// `host:port`, bracketing IPv6.
    #[must_use]
    pub fn display(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

fn parse_port(value: &str) -> Result<u16, TargetError> {
    match value.parse::<u16>() {
        Ok(0) | Err(_) => Err(TargetError::InvalidPort),
        Ok(port) => Ok(port),
    }
}

fn is_host_name(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        })
}

/// The steps, in the order they run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StepKind {
    /// The name resolves to addresses.
    NameResolution,
    /// The Pier answers on its UDP port with a QUIC handshake.
    UdpReachability,
    /// The certificate it presents is the one this Deck expects.
    Certificate,
    /// What the Pier says before sign-in.
    PierGreeting,
    /// Whether Arcen's packets fit the path.
    PathMtu,
    /// How long a round trip takes.
    RoundTrip,
}

impl StepKind {
    /// Every step, in order.
    pub const ALL: [Self; 6] = [
        Self::NameResolution,
        Self::UdpReachability,
        Self::Certificate,
        Self::PierGreeting,
        Self::PathMtu,
        Self::RoundTrip,
    ];

    /// Short title.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::NameResolution => "Name resolution",
            Self::UdpReachability => "UDP reachability (QUIC handshake)",
            Self::Certificate => "Certificate identity",
            Self::PierGreeting => "Pier greeting (before sign-in)",
            Self::PathMtu => "Path MTU",
            Self::RoundTrip => "Round-trip time",
        }
    }

    /// One-based position.
    #[must_use]
    pub const fn number(self) -> u8 {
        match self {
            Self::NameResolution => 1,
            Self::UdpReachability => 2,
            Self::Certificate => 3,
            Self::PierGreeting => 4,
            Self::PathMtu => 5,
            Self::RoundTrip => 6,
        }
    }
}

/// How a finding should be read. Ordered from best to worst.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Verdict {
    /// Works.
    Pass,
    /// Nothing wrong; something to know.
    Info,
    /// The step did not run because an earlier one failed.
    Skipped,
    /// Works, but worse than it should.
    Warning,
    /// Stops a connection.
    Fail,
}

impl Verdict {
    /// Fixed-width label for text reports.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Info => "INFO",
            Self::Skipped => "SKIP",
            Self::Warning => "WARN",
            Self::Fail => "FAIL",
        }
    }
}

/// Every result a step can have. The code of each is stable and documented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Finding {
    /// The name resolved.
    DnsResolved,
    /// The resolver answered with no address.
    DnsNoAddresses,
    /// The resolver said the name does not exist or failed.
    DnsFailed,
    /// The resolver did not answer in time.
    DnsTimedOut,
    /// A QUIC handshake completed.
    UdpHandshakeCompleted,
    /// The Pier's machine answered that nothing listens on the port.
    UdpRefused,
    /// No route to a Pier on the local network.
    UdpLocalNetworkUnreachable,
    /// No route to the Pier.
    UdpUnreachable,
    /// Nothing answered.
    UdpTimedOut,
    /// The Pier speaks a different transport layout.
    UdpTransportMismatch,
    /// The handshake failed for another reason.
    UdpHandshakeFailed,
    /// The certificate matches the identity this Deck remembers.
    CertRemembered,
    /// The certificate matches the fingerprint supplied for this test.
    CertSupplied,
    /// The certificate chains to an authority this computer trusts.
    CertSystemTrusted,
    /// First connection: the Deck will ask to confirm the fingerprint.
    CertFirstUse,
    /// The certificate is not the one this Deck expects.
    CertMismatch,
    /// The certificate does not name the address that was dialled.
    CertNameMismatch,
    /// The certificate has expired.
    CertExpired,
    /// The certificate is not valid yet.
    CertNotYetValid,
    /// The certificate expires soon.
    CertExpiringSoon,
    /// The certificate cannot be used for another reason.
    CertUnusable,
    /// The Pier asks for sign-in; diagnostics stop before credentials.
    GreetingSignIn,
    /// The Pier served a desktop without asking for sign-in.
    GreetingNoSignIn,
    /// The Pier sent something other than a sign-in request.
    GreetingUnexpected,
    /// The Pier closed the connection before asking for sign-in.
    GreetingClosed,
    /// The Pier did not ask for sign-in in time.
    GreetingTimedOut,
    /// Arcen's packets crossed the path.
    MtuFits,
    /// The outgoing interface cannot carry Arcen's packets.
    MtuTooSmall,
    /// The path could not be measured.
    MtuUnknown,
    /// Round trip within the interactive target.
    RttGood,
    /// Round trip above the degraded threshold.
    RttDegraded,
    /// Round trip above the critical threshold.
    RttHigh,
    /// Round trip could not be measured.
    RttUnknown,
    /// An earlier step failed, so this one did not run.
    NotRun,
}

impl Finding {
    /// Every finding, for the documentation check.
    pub const ALL: [Self; 34] = [
        Self::DnsResolved,
        Self::DnsNoAddresses,
        Self::DnsFailed,
        Self::DnsTimedOut,
        Self::UdpHandshakeCompleted,
        Self::UdpRefused,
        Self::UdpLocalNetworkUnreachable,
        Self::UdpUnreachable,
        Self::UdpTimedOut,
        Self::UdpTransportMismatch,
        Self::UdpHandshakeFailed,
        Self::CertRemembered,
        Self::CertSupplied,
        Self::CertSystemTrusted,
        Self::CertFirstUse,
        Self::CertMismatch,
        Self::CertNameMismatch,
        Self::CertExpired,
        Self::CertNotYetValid,
        Self::CertExpiringSoon,
        Self::CertUnusable,
        Self::GreetingSignIn,
        Self::GreetingNoSignIn,
        Self::GreetingUnexpected,
        Self::GreetingClosed,
        Self::GreetingTimedOut,
        Self::MtuFits,
        Self::MtuTooSmall,
        Self::MtuUnknown,
        Self::RttGood,
        Self::RttDegraded,
        Self::RttHigh,
        Self::RttUnknown,
        Self::NotRun,
    ];

    /// Stable code, as listed in `docs/operations/error-codes.md`.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::DnsResolved => "diag.dns.resolved",
            Self::DnsNoAddresses => "diag.dns.no_addresses",
            Self::DnsFailed => "diag.dns.failed",
            Self::DnsTimedOut => "diag.dns.timeout",
            Self::UdpHandshakeCompleted => "diag.udp.handshake_ok",
            Self::UdpRefused => "diag.udp.refused",
            Self::UdpLocalNetworkUnreachable => "diag.udp.local_network_unreachable",
            Self::UdpUnreachable => "diag.udp.unreachable",
            Self::UdpTimedOut => "diag.udp.timeout",
            Self::UdpTransportMismatch => "diag.udp.transport_mismatch",
            Self::UdpHandshakeFailed => "diag.udp.handshake_failed",
            Self::CertRemembered => "diag.cert.remembered",
            Self::CertSupplied => "diag.cert.supplied_match",
            Self::CertSystemTrusted => "diag.cert.system_trusted",
            Self::CertFirstUse => "diag.cert.first_use",
            Self::CertMismatch => "diag.cert.mismatch",
            Self::CertNameMismatch => "diag.cert.name_mismatch",
            Self::CertExpired => "diag.cert.expired",
            Self::CertNotYetValid => "diag.cert.not_yet_valid",
            Self::CertExpiringSoon => "diag.cert.expiring",
            Self::CertUnusable => "diag.cert.unusable",
            Self::GreetingSignIn => "diag.greeting.sign_in",
            Self::GreetingNoSignIn => "diag.greeting.no_sign_in",
            Self::GreetingUnexpected => "diag.greeting.unexpected",
            Self::GreetingClosed => "diag.greeting.closed",
            Self::GreetingTimedOut => "diag.greeting.timeout",
            Self::MtuFits => "diag.mtu.fits",
            Self::MtuTooSmall => "diag.mtu.too_small",
            Self::MtuUnknown => "diag.mtu.unknown",
            Self::RttGood => "diag.rtt.good",
            Self::RttDegraded => "diag.rtt.degraded",
            Self::RttHigh => "diag.rtt.high",
            Self::RttUnknown => "diag.rtt.unknown",
            Self::NotRun => "diag.not_run",
        }
    }

    /// How to read it.
    #[must_use]
    pub const fn verdict(self) -> Verdict {
        match self {
            Self::DnsResolved
            | Self::UdpHandshakeCompleted
            | Self::CertRemembered
            | Self::CertSupplied
            | Self::CertSystemTrusted
            | Self::GreetingSignIn
            | Self::MtuFits
            | Self::RttGood => Verdict::Pass,
            Self::CertFirstUse | Self::MtuUnknown | Self::RttUnknown => Verdict::Info,
            Self::NotRun => Verdict::Skipped,
            Self::CertExpiringSoon | Self::GreetingNoSignIn | Self::RttDegraded | Self::RttHigh => {
                Verdict::Warning
            }
            Self::DnsNoAddresses
            | Self::DnsFailed
            | Self::DnsTimedOut
            | Self::UdpRefused
            | Self::UdpLocalNetworkUnreachable
            | Self::UdpUnreachable
            | Self::UdpTimedOut
            | Self::UdpTransportMismatch
            | Self::UdpHandshakeFailed
            | Self::CertMismatch
            | Self::CertNameMismatch
            | Self::CertExpired
            | Self::CertNotYetValid
            | Self::CertUnusable
            | Self::GreetingUnexpected
            | Self::GreetingClosed
            | Self::GreetingTimedOut
            | Self::MtuTooSmall => Verdict::Fail,
        }
    }

    /// What it means, in one sentence.
    #[must_use]
    pub const fn meaning(self) -> &'static str {
        match self {
            Self::DnsResolved => "The name resolved.",
            Self::DnsNoAddresses => "The name exists but has no address.",
            Self::DnsFailed => "The name could not be resolved.",
            Self::DnsTimedOut => "The name server did not answer in time.",
            Self::UdpHandshakeCompleted => {
                "The Pier answered on its UDP port and the encrypted handshake completed."
            }
            Self::UdpRefused => "The machine answered, but nothing is listening on that UDP port.",
            Self::UdpLocalNetworkUnreachable => {
                "This Mac has no route to that local-network address."
            }
            Self::UdpUnreachable => "There is no route to the Pier.",
            Self::UdpTimedOut => "Nothing answered on that UDP port.",
            Self::UdpTransportMismatch => {
                "The Pier answered but speaks a different Arcen transport version."
            }
            Self::UdpHandshakeFailed => "The Pier answered but the handshake failed.",
            Self::CertRemembered => "The Pier presented the identity this Deck remembers.",
            Self::CertSupplied => "The Pier presented the fingerprint you supplied.",
            Self::CertSystemTrusted => {
                "The certificate is issued by an authority this computer trusts."
            }
            Self::CertFirstUse => {
                "This Deck has not met this Pier before; it will show the fingerprint and ask you to confirm it."
            }
            Self::CertMismatch => {
                "The Pier presented a different identity from the one this Deck expects."
            }
            Self::CertNameMismatch => "The certificate does not name the address you connected to.",
            Self::CertExpired => "The Pier's certificate has expired.",
            Self::CertNotYetValid => "The Pier's certificate is not valid yet.",
            Self::CertExpiringSoon => "The Pier's certificate expires soon.",
            Self::CertUnusable => "The Pier's certificate cannot be used.",
            Self::GreetingSignIn => {
                "The Pier is ready and asks for sign-in. Diagnostics stop here; no credentials were sent."
            }
            Self::GreetingNoSignIn => {
                "The Pier served without asking for sign-in, which a release build never does."
            }
            Self::GreetingUnexpected => "The Pier sent something other than a sign-in request.",
            Self::GreetingClosed => "The Pier closed the connection before asking for sign-in.",
            Self::GreetingTimedOut => {
                "The Pier completed the handshake but did not ask for sign-in."
            }
            Self::MtuFits => "Arcen's packets cross this path in both directions.",
            Self::MtuTooSmall => "The network interface is too small for Arcen's packets.",
            Self::MtuUnknown => "The path size could not be checked.",
            Self::RttGood => "Round trips are fast enough for interactive work.",
            Self::RttDegraded => "Round trips are slow enough to notice.",
            Self::RttHigh => "Round trips are slow; drawing and typing will lag.",
            Self::RttUnknown => "The round-trip time could not be measured.",
            Self::NotRun => "An earlier step failed, so this one did not run.",
        }
    }

    /// What to do. Empty when nothing needs doing.
    #[must_use]
    pub const fn action(self) -> &'static str {
        match self {
            Self::DnsResolved
            | Self::UdpHandshakeCompleted
            | Self::CertRemembered
            | Self::CertSupplied
            | Self::CertSystemTrusted
            | Self::MtuFits
            | Self::RttGood
            | Self::MtuUnknown
            | Self::RttUnknown
            | Self::NotRun => "",
            Self::DnsNoAddresses | Self::DnsFailed => {
                "Check the spelling, or use the Pier's IP address. If the name only resolves inside your organisation, connect to its network or VPN first."
            }
            Self::DnsTimedOut => {
                "Check that this Mac is online and its name servers answer, or use the Pier's IP address."
            }
            Self::UdpRefused => {
                "The Pier service is not running or listens on another port. On the Pier: Linux `systemctl status arcen-pier`, Windows `Get-Service ArcenPier`, macOS `launchctl print system/com.arcen.pier`; compare `listen.port` in pier.json with the port you used."
            }
            Self::UdpLocalNetworkUnreachable => {
                "Allow Arcen Deck in System Settings › Privacy & Security › Local Network, then check that the Pier is switched on and on the same network."
            }
            Self::UdpUnreachable => {
                "Check that the Pier is switched on, that this Mac's network or VPN is up, and that no router drops the route."
            }
            Self::UdpTimedOut => {
                "The Pier is down, the port is wrong, or a firewall drops UDP to it. Check that the Pier service runs and listens on this port (listen.port, default 18444), then allow inbound UDP on the Pier's firewall and on any firewall or VPN in between. Arcen uses UDP only, never TCP."
            }
            Self::UdpTransportMismatch | Self::GreetingUnexpected => {
                "Update the Deck and the Pier to the same Arcen release."
            }
            Self::UdpHandshakeFailed => {
                "Note the detail line, update the Deck and the Pier to the same release, and check the Pier's log for a TLS or listener error."
            }
            Self::CertFirstUse => {
                "Before you trust it, compare the SPKI fingerprint below with the Pier's: host.spki-sha256 in the Pier's configuration directory, or the spki_sha256 field of its TLS_CERTIFICATE_ACTIVE (1400) log record."
            }
            Self::CertMismatch => {
                "Do not sign in. Ask the Pier's administrator whether its certificate was replaced and compare the SPKI fingerprint below with host.spki-sha256 on the Pier. Only if they match, choose Forget and Verify in the Deck."
            }
            Self::CertNameMismatch => {
                "Connect with a name or address the certificate lists, or reissue the certificate with this name (tls.expected_sans, or rerun the installer)."
            }
            Self::CertExpired | Self::CertNotYetValid => {
                "Renew the Pier's certificate (rerun the installer; Linux and macOS also `arcen-pier new-host-cert`), and check that the Pier's and this Mac's clocks are right."
            }
            Self::CertExpiringSoon => {
                "Renew the Pier's certificate before it expires. A renewal that keeps the key keeps the SPKI fingerprint, so Decks are not asked again."
            }
            Self::CertUnusable => {
                "Replace the Pier's certificate with one the installer generates, or one from your authority with an RSA ≥ 2048 or ECDSA key."
            }
            Self::GreetingSignIn => {
                "Sign in with an account on the Pier. The Pier's version, platform and capabilities appear in the Deck's session details after sign-in."
            }
            Self::GreetingNoSignIn => {
                "This is a laboratory build. Do not use it on a network you do not control."
            }
            Self::GreetingClosed => {
                "Read the Pier's log at the time of this test for the reason it closed the connection (for example a full host or a refused peer)."
            }
            Self::GreetingTimedOut => {
                "The Pier may be overloaded or stuck. Check its log and restart the service if it does not answer."
            }
            Self::MtuTooSmall => {
                "Raise the MTU of the VPN or tunnel to at least 1280; QUIC cannot run below 1200-byte packets."
            }
            Self::RttDegraded => {
                "Prefer a wired connection or a closer network path; choose Speed for a lighter stream."
            }
            Self::RttHigh => {
                "Use a closer or wired network path; a VPN that hairpins through a distant site adds every millisecond here."
            }
        }
    }

    /// The step that produces it, or `None` for [`Self::NotRun`].
    #[must_use]
    pub const fn step(self) -> Option<StepKind> {
        match self {
            Self::DnsResolved | Self::DnsNoAddresses | Self::DnsFailed | Self::DnsTimedOut => {
                Some(StepKind::NameResolution)
            }
            Self::UdpHandshakeCompleted
            | Self::UdpRefused
            | Self::UdpLocalNetworkUnreachable
            | Self::UdpUnreachable
            | Self::UdpTimedOut
            | Self::UdpTransportMismatch
            | Self::UdpHandshakeFailed => Some(StepKind::UdpReachability),
            Self::CertRemembered
            | Self::CertSupplied
            | Self::CertSystemTrusted
            | Self::CertFirstUse
            | Self::CertMismatch
            | Self::CertNameMismatch
            | Self::CertExpired
            | Self::CertNotYetValid
            | Self::CertExpiringSoon
            | Self::CertUnusable => Some(StepKind::Certificate),
            Self::GreetingSignIn
            | Self::GreetingNoSignIn
            | Self::GreetingUnexpected
            | Self::GreetingClosed
            | Self::GreetingTimedOut => Some(StepKind::PierGreeting),
            Self::MtuFits | Self::MtuTooSmall | Self::MtuUnknown => Some(StepKind::PathMtu),
            Self::RttGood | Self::RttDegraded | Self::RttHigh | Self::RttUnknown => {
                Some(StepKind::RoundTrip)
            }
            Self::NotRun => None,
        }
    }
}

/// What resolving the name produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolutionObservation {
    /// Addresses, in the resolver's order.
    Resolved {
        /// Resolved addresses.
        addresses: Vec<IpAddr>,
        /// Time taken.
        elapsed_ms: u64,
    },
    /// The resolver answered with nothing.
    NoAddresses,
    /// The resolver failed.
    Failed {
        /// Resolver error text.
        detail: String,
    },
    /// The resolver did not answer.
    TimedOut {
        /// How long the Deck waited.
        waited_ms: u64,
    },
}

/// What one empty UDP datagram sent to the Pier's port provoked. It reveals a
/// refusal (ICMP port unreachable) that QUIC would otherwise only see as a
/// timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UdpProbe {
    /// No error came back; the port may be open or silently filtered.
    NoError,
    /// ICMP port unreachable: the machine is there, the port is closed.
    Refused,
    /// No route.
    Unreachable(ReachabilityFailure),
}

/// Classifies the error from sending or receiving the probe datagram.
#[must_use]
pub fn classify_udp_probe_error(error: &std::io::Error, destination: SocketAddr) -> UdpProbe {
    if error.kind() == std::io::ErrorKind::ConnectionRefused {
        return UdpProbe::Refused;
    }
    crate::network_reachability::classify_reachability_error(error, destination)
        .map_or(UdpProbe::NoError, UdpProbe::Unreachable)
}

/// The outcome of one QUIC handshake attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeOutcome {
    /// The handshake completed.
    Completed {
        /// Time from the first packet to a usable connection.
        handshake_ms: u64,
    },
    /// Nothing answered.
    TimedOut {
        /// How long the Deck waited.
        waited_ms: u64,
    },
    /// The port is closed.
    Refused,
    /// No route.
    Unreachable(ReachabilityFailure),
    /// The certificate was not the expected one; the Deck stopped.
    PinMismatch,
    /// Arcen versions disagree on the transport layout.
    TransportMismatch {
        /// Detail.
        detail: String,
    },
    /// Another failure.
    Failed {
        /// Detail.
        detail: String,
    },
}

impl HandshakeOutcome {
    /// Folds the probe datagram's evidence into a handshake that did not
    /// complete: a timeout with a refused probe is a closed port.
    #[must_use]
    pub fn with_probe(self, probe: UdpProbe) -> Self {
        match (self, probe) {
            (Self::TimedOut { .. } | Self::Failed { .. }, UdpProbe::Refused) => Self::Refused,
            (Self::TimedOut { .. } | Self::Failed { .. }, UdpProbe::Unreachable(failure)) => {
                Self::Unreachable(failure)
            }
            (outcome, _) => outcome,
        }
    }

    const fn finding(&self) -> Finding {
        match self {
            Self::Completed { .. } | Self::PinMismatch => Finding::UdpHandshakeCompleted,
            Self::TimedOut { .. } => Finding::UdpTimedOut,
            Self::Refused => Finding::UdpRefused,
            Self::Unreachable(failure) => match failure.scope {
                ReachabilityScope::LocalNetwork => Finding::UdpLocalNetworkUnreachable,
                ReachabilityScope::Other => Finding::UdpUnreachable,
            },
            Self::TransportMismatch { .. } => Finding::UdpTransportMismatch,
            Self::Failed { .. } => Finding::UdpHandshakeFailed,
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Completed { handshake_ms } => {
                format!("connected in {handshake_ms} ms (QUIC handshake and control lane)")
            }
            Self::TimedOut { waited_ms } => format!("no answer within {waited_ms} ms"),
            Self::Refused => "port closed (ICMP port unreachable)".to_owned(),
            Self::Unreachable(failure) => format!("no route ({})", failure.reason_class()),
            Self::PinMismatch => "answered; the Deck stopped at the certificate".to_owned(),
            Self::TransportMismatch { detail } | Self::Failed { detail } => untrusted_text(detail),
        }
    }
}

/// One dialled address and what happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressAttempt {
    /// Address and port.
    pub address: SocketAddr,
    /// Outcome.
    pub outcome: HandshakeOutcome,
}

/// Fingerprint kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinKind {
    /// SHA-256 of the whole certificate.
    Certificate,
    /// SHA-256 of the subject public key; survives renewal with the same key.
    Spki,
}

impl PinKind {
    /// Label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Certificate => "certificate SHA-256",
            Self::Spki => "SPKI SHA-256",
        }
    }
}

/// A fingerprint the Deck expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedPin {
    /// Kind.
    pub kind: PinKind,
    /// Hex digest, any case, with or without separators.
    pub digest: String,
}

/// Where the expected identity came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpectedIdentity {
    /// Nothing known: a first connection.
    Unknown,
    /// Remembered by this Deck (`trusted_pins.json`).
    Remembered(ExpectedPin),
    /// Supplied for this run (`--pin-sha256`, `--pin-spki`).
    Supplied(ExpectedPin),
}

impl ExpectedIdentity {
    /// The pin, if any.
    #[must_use]
    pub const fn pin(&self) -> Option<&ExpectedPin> {
        match self {
            Self::Unknown => None,
            Self::Remembered(pin) | Self::Supplied(pin) => Some(pin),
        }
    }
}

/// A problem with the certificate itself, independent of trust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeafProblem {
    /// It does not name the dialled host.
    NameMismatch,
    /// Expired.
    Expired,
    /// Not valid yet.
    NotYetValid,
    /// Anything else.
    Unusable {
        /// Detail.
        detail: String,
    },
}

/// The certificate the Pier presented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentedCertificate {
    /// Colon-separated certificate SHA-256.
    pub certificate_sha256: String,
    /// Colon-separated SPKI SHA-256.
    pub spki_sha256: String,
    /// Start of validity.
    pub not_before_epoch_secs: u64,
    /// End of validity.
    pub not_after_epoch_secs: u64,
    /// Whether it chains to a system-trusted authority.
    pub system_trusted: bool,
    /// What the Deck's own leaf checks found.
    pub leaf_problem: Option<LeafProblem>,
}

impl PresentedCertificate {
    /// Whether `pin` matches this certificate.
    #[must_use]
    pub fn matches(&self, pin: &ExpectedPin) -> bool {
        let presented = match pin.kind {
            PinKind::Certificate => &self.certificate_sha256,
            PinKind::Spki => &self.spki_sha256,
        };
        let normalized = normalize_digest(&pin.digest);
        !normalized.is_empty() && normalized == normalize_digest(presented)
    }
}

fn normalize_digest(digest: &str) -> String {
    digest
        .chars()
        .filter(char::is_ascii_hexdigit)
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

/// What the Pier sent before sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GreetingObservation {
    /// A sign-in request.
    SignIn {
        /// Advertised sign-in methods.
        auth_methods: Vec<String>,
        /// Host authentication mode, if named.
        auth_mode: Option<String>,
        /// Whether a disclaimer must be accepted first.
        disclaimer: bool,
        /// Most displays the Pier offers before sign-in, if it offers Match
        /// My Layout.
        max_monitors: Option<u8>,
        /// Whether the Pier lets a dropped session resume without a password.
        resume_supported: bool,
    },
    /// A desktop without sign-in (laboratory builds only).
    NoSignIn {
        /// Pier name.
        server_name: String,
        /// Pier version.
        version: String,
    },
    /// Anything else.
    Unexpected {
        /// Message type.
        message_type: String,
    },
    /// The Pier closed the connection.
    Closed {
        /// Close reason.
        detail: String,
    },
    /// Nothing arrived.
    TimedOut {
        /// How long the Deck waited.
        waited_ms: u64,
    },
}

/// The local side of the path.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PathObservation {
    /// Interface kind label (`Wi-Fi`, `Ethernet`, `VPN`, …).
    pub interface_kind: Option<String>,
    /// Interface MTU.
    pub interface_mtu: Option<u32>,
    /// UDP payload size QUIC was using when the test ended (its path MTU:
    /// [`ARCEN_QUIC_PACKET_BYTES`] unless discovery found more), if
    /// connected.
    pub quic_mtu: Option<u32>,
}

/// Round-trip measurement.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RoundTripObservation {
    /// QUIC's smoothed round-trip estimate.
    pub smoothed_ms: Option<f64>,
}

/// Everything an adapter observed.
#[derive(Debug, Clone, PartialEq)]
pub struct Observations {
    /// The address as tested.
    pub target: DiagnosticTarget,
    /// Name resolution.
    pub resolution: ResolutionObservation,
    /// One attempt per dialled address, in order.
    pub attempts: Vec<AddressAttempt>,
    /// Expected identity.
    pub expected_identity: ExpectedIdentity,
    /// The certificate seen, if the handshake got that far.
    pub certificate: Option<PresentedCertificate>,
    /// What the Pier sent before sign-in, if connected.
    pub greeting: Option<GreetingObservation>,
    /// Local path facts.
    pub path: PathObservation,
    /// Round trip, if connected.
    pub round_trip: RoundTripObservation,
}

/// One step's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepResult {
    /// Which step.
    pub step: StepKind,
    /// The decisive finding.
    pub finding: Finding,
    /// Facts behind it, one per line.
    pub details: Vec<String>,
}

impl StepResult {
    /// The verdict.
    #[must_use]
    pub const fn verdict(&self) -> Verdict {
        self.finding.verdict()
    }
}

/// The assessed report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticReport {
    /// The address as tested.
    pub target: DiagnosticTarget,
    /// One result per step, in order.
    pub steps: Vec<StepResult>,
}

impl DiagnosticReport {
    /// The worst verdict.
    #[must_use]
    pub fn overall(&self) -> Verdict {
        self.steps
            .iter()
            .map(StepResult::verdict)
            .max()
            .unwrap_or(Verdict::Pass)
    }

    /// The first failing step.
    #[must_use]
    pub fn first_failure(&self) -> Option<&StepResult> {
        self.steps
            .iter()
            .find(|step| step.verdict() == Verdict::Fail)
    }

    /// One-line summary.
    #[must_use]
    pub fn summary(&self) -> String {
        if let Some(failure) = self.first_failure() {
            return format!(
                "Cannot connect. Step {}, {}: {}",
                failure.step.number(),
                failure.step.title(),
                failure.finding.meaning()
            );
        }
        let warnings = self
            .steps
            .iter()
            .filter(|step| step.verdict() == Verdict::Warning)
            .count();
        match warnings {
            0 => "Ready to sign in.".to_owned(),
            1 => "Ready to sign in, with 1 warning.".to_owned(),
            count => format!("Ready to sign in, with {count} warnings."),
        }
    }

    /// Process exit status for a command-line run: 0 ready, 1 warnings,
    /// 2 cannot connect.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self.overall() {
            Verdict::Fail => 2,
            Verdict::Warning => 1,
            Verdict::Pass | Verdict::Info | Verdict::Skipped => 0,
        }
    }

    /// The plain-text report: what `arcen-deck diagnose` prints and the Deck
    /// copies to the clipboard. `deck` names the Deck build.
    #[must_use]
    pub fn render_text(&self, deck: &str) -> String {
        let mut text = String::new();
        let _ = writeln!(
            text,
            "Arcen connection test (report v{REPORT_FORMAT_VERSION})"
        );
        let _ = writeln!(text, "Pier:  {} (UDP)", self.target.display());
        let _ = writeln!(text, "Deck:  {deck}");
        let _ = writeln!(text, "No credentials were sent.");
        let _ = writeln!(text);
        for step in &self.steps {
            let _ = writeln!(
                text,
                "[{}] {}. {} — {} ({})",
                step.verdict().label(),
                step.step.number(),
                step.step.title(),
                step.finding.meaning(),
                step.finding.code()
            );
            for detail in &step.details {
                let _ = writeln!(text, "       {detail}");
            }
            let action = step.finding.action();
            if !action.is_empty() {
                let _ = writeln!(text, "       What to do: {action}");
            }
        }
        let _ = writeln!(text);
        let _ = writeln!(text, "Result: {}", self.summary());
        let _ = writeln!(text, "Codes are explained in {ERROR_CODES_DOCUMENT}.");
        text
    }
}

/// Assesses everything observed. `now_epoch_secs` dates the certificate
/// checks; `qos` supplies the round-trip thresholds the Pier itself uses.
#[must_use]
pub fn assess(
    observations: &Observations,
    now_epoch_secs: u64,
    qos: QosTargets,
) -> DiagnosticReport {
    let mut steps = Vec::with_capacity(StepKind::ALL.len());

    let resolution = assess_resolution(&observations.resolution);
    let resolved = resolution.finding == Finding::DnsResolved;
    steps.push(resolution);

    let connected = observations
        .attempts
        .iter()
        .find(|attempt| matches!(attempt.outcome, HandshakeOutcome::Completed { .. }));
    let reached = observations.attempts.iter().any(|attempt| {
        matches!(
            attempt.outcome,
            HandshakeOutcome::Completed { .. } | HandshakeOutcome::PinMismatch
        )
    });
    steps.push(if resolved {
        assess_reachability(&observations.attempts)
    } else {
        not_run(StepKind::UdpReachability)
    });

    let certificate = match &observations.certificate {
        Some(certificate) if reached => {
            assess_certificate(certificate, &observations.expected_identity, now_epoch_secs)
        }
        _ => not_run(StepKind::Certificate),
    };
    let identity_ok = certificate.verdict() != Verdict::Fail;
    steps.push(certificate);

    steps.push(
        match (&observations.greeting, connected.is_some() && identity_ok) {
            (Some(greeting), true) => assess_greeting(greeting),
            _ => not_run(StepKind::PierGreeting),
        },
    );

    steps.push(assess_path(
        &observations.path,
        observations.attempts.first(),
        reached,
    ));

    steps.push(if connected.is_some() {
        assess_round_trip(observations.round_trip, connected, qos)
    } else {
        not_run(StepKind::RoundTrip)
    });

    for step in &mut steps {
        for detail in &mut step.details {
            *detail = printable_line(detail);
        }
    }
    DiagnosticReport {
        target: observations.target.clone(),
        steps,
    }
}

fn not_run(step: StepKind) -> StepResult {
    StepResult {
        step,
        finding: Finding::NotRun,
        details: Vec::new(),
    }
}

fn assess_resolution(observation: &ResolutionObservation) -> StepResult {
    let (finding, details) = match observation {
        ResolutionObservation::Resolved {
            addresses,
            elapsed_ms,
        } if !addresses.is_empty() => (
            Finding::DnsResolved,
            vec![format!(
                "{} ({elapsed_ms} ms)",
                addresses
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )],
        ),
        ResolutionObservation::Resolved { .. } | ResolutionObservation::NoAddresses => {
            (Finding::DnsNoAddresses, Vec::new())
        }
        ResolutionObservation::Failed { detail } => {
            (Finding::DnsFailed, vec![untrusted_text(detail)])
        }
        ResolutionObservation::TimedOut { waited_ms } => {
            (Finding::DnsTimedOut, vec![format!("waited {waited_ms} ms")])
        }
    };
    StepResult {
        step: StepKind::NameResolution,
        finding,
        details,
    }
}

fn assess_reachability(attempts: &[AddressAttempt]) -> StepResult {
    let details = attempts
        .iter()
        .map(|attempt| format!("{}: {}", attempt.address, attempt.outcome.describe()))
        .collect();
    // The best outcome decides: one reachable address is enough to connect.
    let finding = attempts
        .iter()
        .map(|attempt| attempt.outcome.finding())
        .min_by_key(|finding| reachability_rank(*finding))
        .unwrap_or(Finding::UdpHandshakeFailed);
    StepResult {
        step: StepKind::UdpReachability,
        finding,
        details,
    }
}

const fn reachability_rank(finding: Finding) -> u8 {
    match finding {
        Finding::UdpHandshakeCompleted => 0,
        Finding::UdpTransportMismatch => 1,
        Finding::UdpHandshakeFailed => 2,
        Finding::UdpRefused => 3,
        Finding::UdpTimedOut => 4,
        Finding::UdpLocalNetworkUnreachable => 5,
        _ => 6,
    }
}

fn assess_certificate(
    certificate: &PresentedCertificate,
    expected: &ExpectedIdentity,
    now_epoch_secs: u64,
) -> StepResult {
    let mut details = vec![
        format!("SPKI SHA-256:        {}", certificate.spki_sha256),
        format!("certificate SHA-256: {}", certificate.certificate_sha256),
        format!(
            "valid {} to {}",
            format_utc_date(certificate.not_before_epoch_secs),
            format_utc_date(certificate.not_after_epoch_secs)
        ),
    ];
    let finding = if let Some(pin) = expected.pin() {
        if certificate.matches(pin) {
            match expected {
                ExpectedIdentity::Supplied(_) => Finding::CertSupplied,
                _ => Finding::CertRemembered,
            }
        } else {
            details.push(format!(
                "expected {}: {}",
                pin.kind.label(),
                untrusted_text(&pin.digest)
            ));
            Finding::CertMismatch
        }
    } else if certificate.system_trusted {
        Finding::CertSystemTrusted
    } else {
        Finding::CertFirstUse
    };
    let finding = if finding == Finding::CertMismatch {
        finding
    } else {
        match &certificate.leaf_problem {
            Some(LeafProblem::NameMismatch) => Finding::CertNameMismatch,
            Some(LeafProblem::Expired) => Finding::CertExpired,
            Some(LeafProblem::NotYetValid) => Finding::CertNotYetValid,
            Some(LeafProblem::Unusable { detail }) => {
                details.push(untrusted_text(detail));
                Finding::CertUnusable
            }
            None if certificate.not_after_epoch_secs < now_epoch_secs => Finding::CertExpired,
            None if certificate.not_before_epoch_secs > now_epoch_secs => Finding::CertNotYetValid,
            None if certificate
                .not_after_epoch_secs
                .saturating_sub(now_epoch_secs)
                < CERTIFICATE_EXPIRY_WARNING_DAYS * 86_400 =>
            {
                details.push(format!(
                    "expires in {} days",
                    certificate
                        .not_after_epoch_secs
                        .saturating_sub(now_epoch_secs)
                        / 86_400
                ));
                Finding::CertExpiringSoon
            }
            None => finding,
        }
    };
    StepResult {
        step: StepKind::Certificate,
        finding,
        details,
    }
}

fn assess_greeting(greeting: &GreetingObservation) -> StepResult {
    let (finding, details) = match greeting {
        GreetingObservation::SignIn {
            auth_methods,
            auth_mode,
            disclaimer,
            max_monitors,
            resume_supported,
        } => {
            let offered: Vec<&str> = auth_methods
                .iter()
                .map(String::as_str)
                .filter(|method| *method != "resume")
                .collect();
            let mut methods: Vec<String> = offered
                .iter()
                .take(MAX_LISTED_METHODS)
                .map(|method| untrusted_text(method))
                .collect();
            if offered.len() > MAX_LISTED_METHODS {
                methods.push(format!("and {} more", offered.len() - MAX_LISTED_METHODS));
            }
            let mode = auth_mode
                .as_deref()
                .filter(|mode| offered.as_slice() != [*mode])
                .map(|mode| format!("; host authentication: {}", untrusted_text(mode)))
                .unwrap_or_default();
            let mut details = vec![format!("sign-in methods: {}{mode}", methods.join(", "))];
            if *disclaimer {
                details.push(
                    "the Pier shows a disclaimer that must be accepted before sign-in".to_owned(),
                );
            }
            details.push(match max_monitors {
                Some(count) => format!("Match My Layout offered, up to {count} displays"),
                None => "Match My Layout not offered; one display".to_owned(),
            });
            details.push(if *resume_supported {
                "a dropped session can resume without a password".to_owned()
            } else {
                "session resume is off; a dropped session asks for the password again".to_owned()
            });
            details.push(
                "version, platform and capabilities are disclosed only after sign-in".to_owned(),
            );
            (Finding::GreetingSignIn, details)
        }
        GreetingObservation::NoSignIn {
            server_name,
            version,
        } => (
            Finding::GreetingNoSignIn,
            vec![format!(
                "Pier {}, version {}",
                untrusted_text(server_name),
                untrusted_text(version)
            )],
        ),
        GreetingObservation::Unexpected { message_type } => (
            Finding::GreetingUnexpected,
            vec![format!("first message: {}", untrusted_text(message_type))],
        ),
        GreetingObservation::Closed { detail } => {
            (Finding::GreetingClosed, vec![untrusted_text(detail)])
        }
        GreetingObservation::TimedOut { waited_ms } => (
            Finding::GreetingTimedOut,
            vec![format!("waited {waited_ms} ms")],
        ),
    };
    StepResult {
        step: StepKind::PierGreeting,
        finding,
        details,
    }
}

fn assess_path(
    path: &PathObservation,
    first_attempt: Option<&AddressAttempt>,
    reached: bool,
) -> StepResult {
    let header_bytes = match first_attempt.map(|attempt| attempt.address) {
        Some(SocketAddr::V6(_)) => 48,
        _ => 28,
    };
    let needed = ARCEN_QUIC_PACKET_BYTES + header_bytes;
    let mut details = Vec::new();
    if let Some(kind) = &path.interface_kind {
        details.push(match path.interface_mtu {
            Some(mtu) => format!("outgoing interface: {kind}, MTU {mtu}"),
            None => format!("outgoing interface: {kind}"),
        });
    }
    details.push(format!(
        "Arcen starts with {ARCEN_QUIC_PACKET_BYTES}-byte QUIC packets ({needed} bytes with IP/UDP headers) and sends larger ones only after path MTU discovery proved them"
    ));
    match path.quic_mtu {
        Some(mtu) if mtu > ARCEN_QUIC_PACKET_BYTES => details.push(format!(
            "path MTU discovery found {mtu}-byte packets cross this path"
        )),
        Some(mtu) => details.push(format!("QUIC packet size in use: {mtu} bytes")),
        None => {}
    }
    let finding = if path.interface_mtu.is_some_and(|mtu| mtu < needed) {
        Finding::MtuTooSmall
    } else if reached {
        details.push("the padded handshake crossed the path, so the stream fits".to_owned());
        Finding::MtuFits
    } else {
        Finding::MtuUnknown
    };
    StepResult {
        step: StepKind::PathMtu,
        finding,
        details,
    }
}

fn assess_round_trip(
    round_trip: RoundTripObservation,
    connected: Option<&AddressAttempt>,
    qos: QosTargets,
) -> StepResult {
    let mut details = Vec::new();
    if let Some(HandshakeOutcome::Completed { handshake_ms }) =
        connected.map(|attempt| &attempt.outcome)
    {
        details.push(format!("connection set-up: {handshake_ms} ms"));
    }
    let finding = match round_trip.smoothed_ms {
        Some(rtt) => {
            details.push(format!("smoothed round trip: {rtt:.1} ms"));
            details.push(format!(
                "Pier QoS targets: degraded above {} ms, critical above {} ms",
                qos.rtt_degraded_ms(),
                qos.rtt_critical_ms()
            ));
            if rtt > f64::from(qos.rtt_critical_ms()) {
                Finding::RttHigh
            } else if rtt > f64::from(qos.rtt_degraded_ms()) {
                Finding::RttDegraded
            } else {
                Finding::RttGood
            }
        }
        None => Finding::RttUnknown,
    };
    StepResult {
        step: StepKind::RoundTrip,
        finding,
        details,
    }
}

/// `YYYY-MM-DD` for a Unix time, UTC.
#[must_use]
pub fn format_utc_date(epoch_secs: u64) -> String {
    let days = i64::try_from(epoch_secs / 86_400).unwrap_or(i64::MAX / 2);
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;
    use crate::network_reachability::ReachabilityFailureClass;

    const NOW: u64 = 1_800_000_000;
    const DAY: u64 = 86_400;

    fn target() -> DiagnosticTarget {
        DiagnosticTarget::parse("pier.example.internal").unwrap()
    }

    fn address() -> SocketAddr {
        SocketAddr::from((Ipv4Addr::new(203, 0, 113, 10), DEFAULT_PIER_PORT))
    }

    fn certificate() -> PresentedCertificate {
        PresentedCertificate {
            certificate_sha256: "AA:BB".repeat(16),
            spki_sha256: "CC:DD:".repeat(16).trim_end_matches(':').to_owned(),
            not_before_epoch_secs: NOW - 100 * DAY,
            not_after_epoch_secs: NOW + 400 * DAY,
            system_trusted: false,
            leaf_problem: None,
        }
    }

    fn sign_in() -> GreetingObservation {
        GreetingObservation::SignIn {
            auth_methods: vec!["password".into(), "resume".into()],
            auth_mode: None,
            disclaimer: false,
            max_monitors: Some(2),
            resume_supported: true,
        }
    }

    fn healthy() -> Observations {
        Observations {
            target: target(),
            resolution: ResolutionObservation::Resolved {
                addresses: vec![address().ip()],
                elapsed_ms: 3,
            },
            attempts: vec![AddressAttempt {
                address: address(),
                outcome: HandshakeOutcome::Completed { handshake_ms: 40 },
            }],
            expected_identity: ExpectedIdentity::Remembered(ExpectedPin {
                kind: PinKind::Spki,
                digest: "ccdd".repeat(16),
            }),
            certificate: Some(certificate()),
            greeting: Some(sign_in()),
            path: PathObservation {
                interface_kind: Some("Wi-Fi".into()),
                interface_mtu: Some(1_500),
                quic_mtu: Some(1_200),
            },
            round_trip: RoundTripObservation {
                smoothed_ms: Some(18.0),
            },
        }
    }

    fn findings(report: &DiagnosticReport) -> Vec<Finding> {
        report.steps.iter().map(|step| step.finding).collect()
    }

    #[test]
    fn addresses_parse_with_and_without_ports() {
        let cases = [
            ("pier.example.internal", "pier.example.internal", 18_444),
            (
                "pier.example.internal:19000",
                "pier.example.internal",
                19_000,
            ),
            ("203.0.113.5", "203.0.113.5", 18_444),
            ("203.0.113.5:18445", "203.0.113.5", 18_445),
            ("2001:db8::5", "2001:db8::5", 18_444),
            ("[2001:db8::5]:18446", "2001:db8::5", 18_446),
            ("  pier  ", "pier", 18_444),
        ];
        for (input, host, port) in cases {
            let parsed = DiagnosticTarget::parse(input).unwrap();
            assert_eq!((parsed.host.as_str(), parsed.port), (host, port), "{input}");
        }
        assert_eq!(
            DiagnosticTarget::parse("[2001:db8::5]:1")
                .unwrap()
                .display(),
            "[2001:db8::5]:1"
        );
        for (input, error) in [
            ("", TargetError::Empty),
            ("pier:0", TargetError::InvalidPort),
            ("pier:70000", TargetError::InvalidPort),
            ("pier:", TargetError::InvalidPort),
            ("bad host", TargetError::InvalidHost),
            ("pier/x", TargetError::InvalidHost),
            ("-pier", TargetError::InvalidHost),
            ("[pier]:1", TargetError::InvalidHost),
        ] {
            assert_eq!(DiagnosticTarget::parse(input), Err(error), "{input}");
        }
        assert_eq!(
            DiagnosticTarget::parse_with_port("pier", Some(19_001))
                .unwrap()
                .port,
            19_001
        );
    }

    #[test]
    fn a_healthy_pier_passes_every_step_and_stops_before_credentials() {
        let report = assess(&healthy(), NOW, QosTargets::default());
        assert_eq!(
            findings(&report),
            vec![
                Finding::DnsResolved,
                Finding::UdpHandshakeCompleted,
                Finding::CertRemembered,
                Finding::GreetingSignIn,
                Finding::MtuFits,
                Finding::RttGood,
            ]
        );
        assert_eq!(report.exit_code(), 0);
        let text = report.render_text("Arcen Deck test");
        assert!(text.contains("No credentials were sent."));
        assert!(text.contains("Result: Ready to sign in."));
        assert!(text.contains("Match My Layout offered, up to 2 displays"));
        assert!(text.contains("sign-in methods: password\n"), "{text}");
    }

    #[test]
    fn a_refused_probe_turns_a_timeout_into_a_closed_port() {
        let timed_out = HandshakeOutcome::TimedOut { waited_ms: 5_000 };
        assert_eq!(
            timed_out.clone().with_probe(UdpProbe::Refused),
            HandshakeOutcome::Refused
        );
        assert_eq!(timed_out.clone().with_probe(UdpProbe::NoError), timed_out);
        let completed = HandshakeOutcome::Completed { handshake_ms: 1 };
        assert_eq!(completed.clone().with_probe(UdpProbe::Refused), completed);
        let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert_eq!(
            classify_udp_probe_error(&refused, address()),
            UdpProbe::Refused
        );

        let mut observations = healthy();
        observations.attempts[0].outcome = HandshakeOutcome::Refused;
        observations.certificate = None;
        observations.greeting = None;
        let report = assess(&observations, NOW, QosTargets::default());
        assert_eq!(
            findings(&report),
            vec![
                Finding::DnsResolved,
                Finding::UdpRefused,
                Finding::NotRun,
                Finding::NotRun,
                Finding::MtuUnknown,
                Finding::NotRun,
            ]
        );
        assert_eq!(report.exit_code(), 2);
        assert!(report.summary().starts_with("Cannot connect. Step 2, "));
    }

    #[test]
    fn unreachable_local_addresses_name_the_local_network_permission() {
        let failure = ReachabilityFailure {
            scope: ReachabilityScope::LocalNetwork,
            class: ReachabilityFailureClass::HostUnreachable,
            os_code: Some(65),
        };
        let outcome =
            HandshakeOutcome::TimedOut { waited_ms: 1 }.with_probe(UdpProbe::Unreachable(failure));
        let mut observations = healthy();
        observations.attempts[0].outcome = outcome;
        let report = assess(&observations, NOW, QosTargets::default());
        assert_eq!(report.steps[1].finding, Finding::UdpLocalNetworkUnreachable);
        assert!(report.render_text("d").contains("Local Network"));
    }

    #[test]
    fn one_reachable_address_is_enough() {
        let mut observations = healthy();
        observations.attempts.insert(
            0,
            AddressAttempt {
                address: "[2001:db8::5]:18444".parse().unwrap(),
                outcome: HandshakeOutcome::TimedOut { waited_ms: 3_000 },
            },
        );
        let report = assess(&observations, NOW, QosTargets::default());
        assert_eq!(report.steps[1].finding, Finding::UdpHandshakeCompleted);
        assert_eq!(report.steps[1].details.len(), 2);
    }

    #[test]
    fn name_failures_skip_every_later_network_step() {
        let mut observations = healthy();
        observations.resolution = ResolutionObservation::Failed {
            detail: "nodename nor servname provided".into(),
        };
        observations.attempts.clear();
        observations.certificate = None;
        observations.greeting = None;
        let report = assess(&observations, NOW, QosTargets::default());
        assert_eq!(report.steps[0].finding, Finding::DnsFailed);
        assert!(
            report.steps[1..4]
                .iter()
                .all(|step| step.finding == Finding::NotRun)
        );
    }

    #[test]
    fn a_changed_identity_fails_and_shows_both_fingerprints() {
        let mut observations = healthy();
        observations.attempts[0].outcome = HandshakeOutcome::PinMismatch;
        observations.greeting = None;
        observations.expected_identity = ExpectedIdentity::Remembered(ExpectedPin {
            kind: PinKind::Spki,
            digest: "11".repeat(32),
        });
        let report = assess(&observations, NOW, QosTargets::default());
        assert_eq!(report.steps[1].finding, Finding::UdpHandshakeCompleted);
        assert_eq!(report.steps[2].finding, Finding::CertMismatch);
        assert_eq!(report.steps[3].finding, Finding::NotRun);
        let text = report.render_text("d");
        assert!(text.contains("expected SPKI SHA-256: 1111"));
        assert!(text.contains("Do not sign in."));
    }

    #[test]
    fn certificate_findings_cover_first_use_trust_and_validity() {
        let mut certificate = certificate();
        let first_use = assess_certificate(&certificate, &ExpectedIdentity::Unknown, NOW);
        assert_eq!(first_use.finding, Finding::CertFirstUse);
        assert_eq!(first_use.verdict(), Verdict::Info);

        certificate.system_trusted = true;
        assert_eq!(
            assess_certificate(&certificate, &ExpectedIdentity::Unknown, NOW).finding,
            Finding::CertSystemTrusted
        );
        let supplied = ExpectedIdentity::Supplied(ExpectedPin {
            kind: PinKind::Certificate,
            digest: "aabb".repeat(16),
        });
        assert_eq!(
            assess_certificate(&certificate, &supplied, NOW).finding,
            Finding::CertSupplied
        );

        certificate.not_after_epoch_secs = NOW + 10 * DAY;
        let expiring = assess_certificate(&certificate, &ExpectedIdentity::Unknown, NOW);
        assert_eq!(expiring.finding, Finding::CertExpiringSoon);
        assert!(
            expiring
                .details
                .iter()
                .any(|line| line == "expires in 10 days")
        );

        certificate.not_after_epoch_secs = NOW - DAY;
        assert_eq!(
            assess_certificate(&certificate, &ExpectedIdentity::Unknown, NOW).finding,
            Finding::CertExpired
        );
        certificate.leaf_problem = Some(LeafProblem::NameMismatch);
        assert_eq!(
            assess_certificate(&certificate, &ExpectedIdentity::Unknown, NOW).finding,
            Finding::CertNameMismatch
        );
    }

    #[test]
    fn greeting_problems_are_failures() {
        for (greeting, finding) in [
            (
                GreetingObservation::TimedOut { waited_ms: 5_000 },
                Finding::GreetingTimedOut,
            ),
            (
                GreetingObservation::Closed {
                    detail: "host busy".into(),
                },
                Finding::GreetingClosed,
            ),
            (
                GreetingObservation::Unexpected {
                    message_type: "quality_settings".into(),
                },
                Finding::GreetingUnexpected,
            ),
        ] {
            let mut observations = healthy();
            observations.greeting = Some(greeting);
            let report = assess(&observations, NOW, QosTargets::default());
            assert_eq!(report.steps[3].finding, finding);
            assert_eq!(report.exit_code(), 2);
        }
    }

    #[test]
    fn small_tunnels_and_slow_paths_are_called_out() {
        let mut observations = healthy();
        observations.path.interface_mtu = Some(1_200);
        observations.round_trip.smoothed_ms = Some(90.0);
        let report = assess(&observations, NOW, QosTargets::default());
        assert_eq!(report.steps[4].finding, Finding::MtuTooSmall);
        assert_eq!(report.steps[5].finding, Finding::RttDegraded);

        observations.path.interface_mtu = Some(1_280);
        observations.path.quic_mtu = Some(1_252);
        observations.round_trip.smoothed_ms = Some(400.0);
        let report = assess(&observations, NOW, QosTargets::default());
        assert_eq!(report.steps[4].finding, Finding::MtuFits);
        assert!(
            report.steps[4].details.iter().any(
                |detail| detail == "path MTU discovery found 1252-byte packets cross this path"
            )
        );
        assert_eq!(report.steps[5].finding, Finding::RttHigh);
        assert_eq!(report.exit_code(), 1);
        assert!(report.summary().contains("1 warning"));
    }

    #[test]
    fn every_finding_has_a_unique_code_and_wording() {
        let mut codes = std::collections::BTreeSet::new();
        for finding in Finding::ALL {
            assert!(codes.insert(finding.code()), "{}", finding.code());
            assert!(finding.code().starts_with("diag."));
            assert!(!finding.meaning().is_empty());
            let needs_action = matches!(finding.verdict(), Verdict::Warning | Verdict::Fail);
            assert!(
                !needs_action || !finding.action().is_empty(),
                "{} needs an action",
                finding.code()
            );
            if let Some(step) = finding.step() {
                assert!(StepKind::ALL.contains(&step));
            }
        }
    }

    #[test]
    fn pier_supplied_text_cannot_steer_the_terminal_or_hide_fingerprints() {
        // ESC sequences that would erase the previous line and print a fake
        // fingerprint, a right-to-left override, an isolate, a bare CR.
        let hostile = "\u{1b}[1A\u{1b}[2KSPKI SHA-256: 00:00\u{202E}evil\u{2066}x\u{2069}\r\n";
        let mut observations = healthy();
        observations.expected_identity = ExpectedIdentity::Unknown;
        observations.greeting = Some(GreetingObservation::SignIn {
            auth_methods: (0..20).map(|_| hostile.to_owned()).collect(),
            auth_mode: Some(hostile.to_owned()),
            disclaimer: false,
            max_monitors: None,
            resume_supported: false,
        });
        let report = assess(&observations, NOW, QosTargets::default());
        let text = report.render_text("d");
        for forbidden in ['\u{1b}', '\u{202E}', '\u{2066}', '\u{2069}', '\r'] {
            assert!(
                !text.contains(forbidden),
                "{forbidden:?} reached the report"
            );
            for step in &report.steps {
                assert!(step.details.iter().all(|line| !line.contains(forbidden)));
            }
        }
        assert!(text.contains("\\u{1b}[1A"), "{text}");
        assert!(text.contains("\\u{202e}evil"));
        assert!(text.contains("and 12 more"));
        // The real fingerprint line is still the only one that starts the line.
        assert_eq!(
            text.lines()
                .filter(|line| line.trim_start().starts_with("SPKI SHA-256:"))
                .count(),
            1
        );

        for greeting in [
            GreetingObservation::Unexpected {
                message_type: hostile.to_owned(),
            },
            GreetingObservation::Closed {
                detail: hostile.to_owned(),
            },
            GreetingObservation::NoSignIn {
                server_name: hostile.to_owned(),
                version: "x".repeat(10_000),
            },
        ] {
            observations.greeting = Some(greeting);
            let report = assess(&observations, NOW, QosTargets::default());
            let line = &report.steps[3].details[0];
            assert!(
                !line.contains('\u{1b}') && !line.contains('\u{202E}'),
                "{line}"
            );
            assert!(line.chars().count() < 3 * MAX_UNTRUSTED_CHARS, "{line}");
        }
        observations.attempts[0].outcome = HandshakeOutcome::Failed {
            detail: hostile.to_owned(),
        };
        let report = assess(&observations, NOW, QosTargets::default());
        assert!(report.steps[1].details[0].contains("\\u{1b}"));
    }

    #[test]
    fn untrusted_text_is_capped_and_keeps_ordinary_text() {
        assert_eq!(untrusted_text("pam"), "pam");
        assert_eq!(untrusted_text("Ærø — 東京"), "Ærø — 東京");
        let long = untrusted_text(&"a".repeat(500));
        assert_eq!(long.chars().count(), MAX_UNTRUSTED_CHARS + 1);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn utc_dates_are_civil() {
        assert_eq!(format_utc_date(0), "1970-01-01");
        assert_eq!(format_utc_date(951_782_400), "2000-02-29");
        assert_eq!(format_utc_date(1_800_000_000), "2027-01-15");
    }
}
