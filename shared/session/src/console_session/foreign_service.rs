// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Naming the other remote-desktop service that holds a workstation.
//!
//! When a console is held through a session the display manager did not
//! start, the refusal names what holds it, as the host reports it at run
//! time: the PAM service that opened the session, and the system service
//! (or process) that started it. Arcen never carries a product name of its
//! own for this; the words come from the host. A product that runs the
//! workstation's sign-in ([`ConsoleRefusal::SignInOtherProduct`]) is named
//! by its display-manager unit's own description, where it has a plain one.
//! Every part is checked
//! before it reaches a log line or a Deck: only a short identifier is
//! kept, so a hostile or malformed name cannot put text in front of the
//! user.

use serde::{Deserialize, Serialize};

use super::{ConsoleRefusal, ConsoleSessionFact};

/// The longest part of a service name kept (a PAM service, a systemd unit
/// or a process name).
pub const SERVICE_PART_MAX: usize = 40;

/// A system service's description (`Description=`) kept only when it is
/// plain words: letters, digits, single spaces and `.-_+`.
fn clean_description(raw: &str) -> Option<String> {
    let raw = raw.trim();
    (!raw.is_empty()
        && raw.len() <= SERVICE_PART_MAX
        && !raw.contains("  ")
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b' ' | b'.' | b'_' | b'-' | b'+')))
    .then(|| raw.to_owned())
}

/// The longest close reason a Deck is sent.
pub const DECK_REASON_MAX: usize = 120;

fn clean_part(raw: &str) -> Option<String> {
    let raw = raw.trim();
    (!raw.is_empty()
        && raw.len() <= SERVICE_PART_MAX
        && raw.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'@' | b':' | b'+' | b'-')
        }))
    .then(|| raw.to_owned())
}

/// What the host says holds its console, read at run time. Every part is
/// optional and already checked ([`clean_part`]); an empty value names
/// nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(try_from = "RawForeignService")]
pub struct ForeignService {
    /// The PAM service that opened the session (logind `Service=`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pam_service: Option<String>,
    /// The system service that started it (`<name>.service`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unit: Option<String>,
    /// The name of the process in that service that started it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    process: Option<String>,
    /// How the system service describes itself (its `Description=`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

#[derive(Deserialize)]
struct RawForeignService {
    #[serde(default)]
    pam_service: Option<String>,
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    process: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

impl TryFrom<RawForeignService> for ForeignService {
    type Error = &'static str;

    /// A name from another process is checked again on arrival.
    fn try_from(raw: RawForeignService) -> Result<Self, Self::Error> {
        Ok(Self::new(
            raw.pam_service.as_deref().unwrap_or_default(),
            raw.unit.as_deref(),
            raw.process.as_deref(),
        )
        .described(raw.description.as_deref()))
    }
}

impl ForeignService {
    /// Keeps the parts that are short identifiers; drops the rest.
    #[must_use]
    pub fn new(pam_service: &str, unit: Option<&str>, process: Option<&str>) -> Self {
        Self {
            pam_service: clean_part(pam_service),
            unit: unit.and_then(clean_part),
            process: process.and_then(clean_part),
            description: None,
        }
    }

    /// With the system service's own description, kept when it is plain
    /// words ([`clean_description`]).
    #[must_use]
    pub fn described(mut self, description: Option<&str>) -> Self {
        self.description = description.and_then(clean_description);
        self
    }

    /// Whether any part is known.
    #[must_use]
    pub const fn is_known(&self) -> bool {
        self.pam_service.is_some() || self.unit.is_some() || self.process.is_some()
    }

    /// What a Windows host reports for a session another remote-desktop
    /// service attaches: the session's window station, the listener it came in
    /// through (`RDP-Tcp#3` is a connection of the `RDP-Tcp` listener, so
    /// the connection number is left off), and its client protocol
    /// (`WTSClientProtocolType`: 1 is ICA, 2 RDP). The window station takes the
    /// place of a PAM service, the protocol that of the process.
    #[must_use]
    pub fn windows_station(win_station: &str, protocol: Option<u16>) -> Self {
        let listener = win_station
            .split_once('#')
            .map_or(win_station, |(listener, _)| listener);
        let protocol = match protocol {
            Some(1) => Some("ica"),
            Some(2) => Some("rdp"),
            _ => None,
        };
        Self::new(listener, None, protocol)
    }

    #[must_use]
    pub fn pam_service(&self) -> Option<&str> {
        self.pam_service.as_deref()
    }

    #[must_use]
    pub fn unit(&self) -> Option<&str> {
        self.unit.as_deref()
    }

    #[must_use]
    pub fn process(&self) -> Option<&str> {
        self.process.as_deref()
    }

    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// The names a Deck is shown, longest first: the PAM service with the
    /// system service (or, without one, the process), the same with the
    /// unit's `.service` suffix left off (a close reason is short), then
    /// each alone.
    fn labels(&self) -> Vec<String> {
        let starter = self.unit.as_deref().or(self.process.as_deref());
        let mut labels = Vec::new();
        match (self.pam_service.as_deref(), starter) {
            (Some(pam), Some(starter)) if pam != starter => {
                labels.push(format!("{pam}, {starter}"));
                if let Some(short) = starter.strip_suffix(".service") {
                    labels.push(format!("{pam}, {short}"));
                }
                labels.push(pam.to_owned());
                labels.push(starter.to_owned());
            }
            (Some(pam), _) => labels.push(pam.to_owned()),
            (None, Some(starter)) => labels.push(starter.to_owned()),
            (None, None) => {}
        }
        labels
    }

    /// The longest name that fits `budget` bytes.
    #[must_use]
    pub fn label_within(&self, budget: usize) -> Option<String> {
        self.labels()
            .into_iter()
            .find(|label| label.len() <= budget)
    }

    /// The full name, for logs.
    #[must_use]
    pub fn label(&self) -> Option<String> {
        self.labels().into_iter().next()
    }
}

/// A console refusal and, when another remote-desktop service holds the
/// workstation, what the host says that service is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleNotice {
    refusal: ConsoleRefusal,
    service: Option<ForeignService>,
}

impl From<ConsoleRefusal> for ConsoleNotice {
    fn from(refusal: ConsoleRefusal) -> Self {
        Self {
            refusal,
            service: None,
        }
    }
}

impl ConsoleNotice {
    /// A refusal naming the service; the name is kept only for the refusals
    /// it explains (another remote-desktop service holds the workstation, or
    /// runs its sign-in: then its unit is the display manager's).
    #[must_use]
    pub fn naming(refusal: ConsoleRefusal, service: Option<ForeignService>) -> Self {
        let service = service.filter(|service| {
            service.is_known()
                && matches!(
                    refusal,
                    ConsoleRefusal::OtherUserOtherProduct | ConsoleRefusal::SignInOtherProduct
                )
        });
        Self { refusal, service }
    }

    /// The refusal of a sign-in on a host whose display-manager unit
    /// `unit` (another product's, described by the host as `description`)
    /// runs the workstation's sign-in.
    #[must_use]
    pub fn sign_in_other_product(unit: Option<&str>, description: Option<&str>) -> Self {
        Self::naming(
            ConsoleRefusal::SignInOtherProduct,
            Some(ForeignService::new("", unit, None).described(description)),
        )
    }

    #[must_use]
    pub const fn refusal(&self) -> ConsoleRefusal {
        self.refusal
    }

    #[must_use]
    pub const fn token(&self) -> &'static str {
        self.refusal.token()
    }

    #[must_use]
    pub const fn service(&self) -> Option<&ForeignService> {
        self.service.as_ref()
    }

    /// What the Deck shows: the code first, the service the host named
    /// when it fits, never more than [`DECK_REASON_MAX`] bytes.
    #[must_use]
    pub fn deck_reason(&self) -> String {
        if self.refusal == ConsoleRefusal::SignInOtherProduct {
            // The product's own description, else its unit.
            let names = self
                .service
                .iter()
                .flat_map(|service| [service.description(), service.unit()])
                .flatten();
            for name in names {
                let reason = format!(
                    "{}: {name} runs this workstation's sign-in; sign in with it first.",
                    self.refusal.token()
                );
                if reason.len() <= DECK_REASON_MAX {
                    return reason;
                }
            }
            return self.refusal.deck_reason().to_owned();
        }
        if let Some(service) = &self.service {
            let head = format!("{}: another remote-desktop service (", self.refusal.token());
            let tail = ") uses this workstation.";
            let budget = DECK_REASON_MAX.saturating_sub(head.len() + tail.len());
            if let Some(label) = service.label_within(budget) {
                return format!("{head}{label}{tail}");
            }
        }
        self.refusal.deck_reason().to_owned()
    }

    /// The notice a launcher's status names, or `None` for a status that is
    /// no console refusal.
    #[must_use]
    pub fn from_status(status: &str, service: Option<ForeignService>) -> Option<Self> {
        ConsoleRefusal::ALL
            .into_iter()
            .find(|refusal| refusal.token() == status)
            .map(|refusal| Self::naming(refusal, service))
    }
}

/// The session that makes `decide` refuse `uid` with
/// [`ConsoleRefusal::OtherUserOtherProduct`]: another account's session the
/// display manager did not start, the one in front first.
#[must_use]
pub fn other_product_holder(
    sessions: &[ConsoleSessionFact],
    uid: u32,
) -> Option<&ConsoleSessionFact> {
    sessions
        .iter()
        .filter(|s| {
            s.is_console_desktop()
                && s.uid != uid
                && s.origin() == super::ConsoleOrigin::OtherProduct
        })
        .max_by_key(|s| s.active)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_short_identifiers_are_kept() {
        let service = ForeignService::new(
            "labrdp-session",
            Some("labrdp.service"),
            Some("labrdp-agent"),
        );
        assert_eq!(
            service.label().as_deref(),
            Some("labrdp-session, labrdp.service")
        );
        for hostile in [
            "a b",
            "x\u{1b}[2J",
            "évil",
            "line\nbreak",
            "",
            "    ",
            "a\"b",
            &"x".repeat(SERVICE_PART_MAX + 1),
        ] {
            assert!(
                !ForeignService::new(hostile, Some(hostile), Some(hostile)).is_known(),
                "{hostile:?}"
            );
        }
        // Without a unit, the process names what started it.
        assert_eq!(
            ForeignService::new("su-l", None, Some("startx"))
                .label()
                .as_deref(),
            Some("su-l, startx")
        );
        // A part equal to the PAM service is not repeated.
        assert_eq!(
            ForeignService::new("same", Some("same"), None)
                .label()
                .as_deref(),
            Some("same")
        );
    }

    #[test]
    fn a_windows_session_is_named_by_its_listener_and_protocol() {
        let notice = ConsoleNotice::naming(
            ConsoleRefusal::OtherUserOtherProduct,
            Some(ForeignService::windows_station("RDP-Tcp#3", Some(2))),
        );
        assert_eq!(
            notice.deck_reason(),
            "console_other_user_other_product: another remote-desktop service (RDP-Tcp, rdp) uses this workstation."
        );
        assert_eq!(
            ForeignService::windows_station("ICA-CGP#12", Some(1))
                .label()
                .as_deref(),
            Some("ICA-CGP, ica")
        );
        // An unknown protocol names the listener alone; a hostile name nothing.
        assert_eq!(
            ForeignService::windows_station("Remote Station", Some(9)).label(),
            None
        );
        assert_eq!(
            ForeignService::windows_station("Vendor-1#0", Some(7))
                .label()
                .as_deref(),
            Some("Vendor-1")
        );
    }

    #[test]
    fn the_deck_reason_names_the_service_and_fits_a_close_frame() {
        let notice = ConsoleNotice::naming(
            ConsoleRefusal::OtherUserOtherProduct,
            Some(ForeignService::new(
                "labrdp-session",
                Some("labrdp.service"),
                None,
            )),
        );
        assert_eq!(
            notice.deck_reason(),
            "console_other_user_other_product: another remote-desktop service (labrdp-session, labrdp.service) uses this workstation."
        );
        assert!(notice.deck_reason().len() <= DECK_REASON_MAX);
        assert!(notice.deck_reason().starts_with(notice.token()));
        assert!(notice.deck_reason().contains("labrdp-session"));
        // A unit name that does not fit loses its suffix first.
        let notice = ConsoleNotice::naming(
            ConsoleRefusal::OtherUserOtherProduct,
            Some(ForeignService::new(
                "labrdp-session",
                Some("labrdp-9.service"),
                None,
            )),
        );
        assert!(
            notice.deck_reason().contains("(labrdp-session, labrdp-9)"),
            "{}",
            notice.deck_reason()
        );
        // Long names fall back to one part, then to the plain sentence.
        let long = "x".repeat(SERVICE_PART_MAX);
        let notice = ConsoleNotice::naming(
            ConsoleRefusal::OtherUserOtherProduct,
            Some(ForeignService::new(&long, Some(&long), None)),
        );
        assert!(
            notice.deck_reason().len() <= DECK_REASON_MAX,
            "{}",
            notice.deck_reason()
        );
        assert!(notice.deck_reason().starts_with(notice.token()));
    }

    #[test]
    fn a_name_is_kept_only_for_the_refusal_it_explains() {
        let service = ForeignService::new("labrdp-session", None, None);
        let notice = ConsoleNotice::naming(ConsoleRefusal::OtherUser, Some(service.clone()));
        assert_eq!(notice.service(), None);
        assert_eq!(
            notice.deck_reason(),
            ConsoleRefusal::OtherUser.deck_reason()
        );
        let notice = ConsoleNotice::naming(
            ConsoleRefusal::OtherUserOtherProduct,
            Some(ForeignService::default()),
        );
        assert_eq!(notice.service(), None, "an unknown service names nothing");
        assert_eq!(
            ConsoleNotice::from_status("console_other_user_other_product", Some(service))
                .map(|n| n.refusal()),
            Some(ConsoleRefusal::OtherUserOtherProduct)
        );
        assert_eq!(ConsoleNotice::from_status("agent_error", None), None);
    }

    #[test]
    fn the_product_that_runs_the_sign_in_is_named_as_the_host_describes_it() {
        let notice = ConsoleNotice::sign_in_other_product(
            Some("labproduct.service"),
            Some("Lab Remote Product"),
        );
        assert_eq!(
            notice.deck_reason(),
            "console_sign_in_other_product: Lab Remote Product runs this workstation's sign-in; sign in with it first."
        );
        // Without a plain description, the unit.
        for description in [None, Some("Lab\u{1b}[2J"), Some("a  b"), Some("Lab (beta)")] {
            assert_eq!(
                ConsoleNotice::sign_in_other_product(Some("labproduct.service"), description)
                    .deck_reason(),
                "console_sign_in_other_product: labproduct.service runs this workstation's sign-in; sign in with it first.",
                "{description:?}"
            );
        }
        // No unit, a hostile one, or one too long for a close frame: the
        // plain sentence.
        let long = format!("{}.service", "x".repeat(SERVICE_PART_MAX - 8));
        for unit in [None, Some("bad name"), Some(long.as_str())] {
            let notice = ConsoleNotice::sign_in_other_product(unit, None);
            assert_eq!(
                notice.deck_reason(),
                ConsoleRefusal::SignInOtherProduct.deck_reason(),
                "{unit:?}"
            );
        }
        // The names survive the launcher's status.
        let service: Option<ForeignService> = notice
            .service()
            .map(|service| serde_json::to_string(service).expect("serialise"))
            .map(|text| serde_json::from_str(&text).expect("parse"));
        assert_eq!(
            ConsoleNotice::from_status("console_sign_in_other_product", service),
            Some(notice)
        );
        for refusal in ConsoleRefusal::ALL {
            assert!(
                refusal.deck_reason().len() <= DECK_REASON_MAX,
                "{refusal:?}"
            );
            assert!(refusal.deck_reason().starts_with(refusal.token()));
        }
    }

    #[test]
    fn a_name_from_another_process_is_checked_again() {
        let parsed: ForeignService = serde_json::from_str(
            r#"{"pam_service":"ok-name","unit":"bad name\u001b","process":"p"}"#,
        )
        .expect("parse");
        assert_eq!(parsed.pam_service(), Some("ok-name"));
        assert_eq!(parsed.unit(), None);
        assert_eq!(parsed.process(), Some("p"));
    }
}
