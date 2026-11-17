// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Naming the other remote-desktop service that holds a workstation.
//!
//! When a console is held through a session the display manager did not
//! start, the refusal names what holds it, as the host reports it at run
//! time: the PAM service that opened the session, and the system service
//! (or process) that started it. Arcen never carries a product name of its
//! own for this; the words come from the host. Every part is checked
//! before it reaches a log line or a Deck: only a short identifier is
//! kept, so a hostile or malformed name cannot put text in front of the
//! user.

use serde::{Deserialize, Serialize};

use super::{ConsoleRefusal, ConsoleSessionFact};

/// The longest part of a service name kept (a PAM service, a systemd unit
/// or a process name).
pub const SERVICE_PART_MAX: usize = 40;

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
}

#[derive(Deserialize)]
struct RawForeignService {
    #[serde(default)]
    pam_service: Option<String>,
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    process: Option<String>,
}

impl TryFrom<RawForeignService> for ForeignService {
    type Error = &'static str;

    /// A name from another process is checked again on arrival.
    fn try_from(raw: RawForeignService) -> Result<Self, Self::Error> {
        Ok(Self::new(
            raw.pam_service.as_deref().unwrap_or_default(),
            raw.unit.as_deref(),
            raw.process.as_deref(),
        ))
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
        }
    }

    /// Whether any part is known.
    #[must_use]
    pub const fn is_known(&self) -> bool {
        self.pam_service.is_some() || self.unit.is_some() || self.process.is_some()
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
    /// A refusal naming the service; the name is kept only for the refusal
    /// it explains (another remote-desktop service holds the workstation).
    #[must_use]
    pub fn naming(refusal: ConsoleRefusal, service: Option<ForeignService>) -> Self {
        let service = service.filter(|service| {
            service.is_known() && refusal == ConsoleRefusal::OtherUserOtherProduct
        });
        Self { refusal, service }
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
