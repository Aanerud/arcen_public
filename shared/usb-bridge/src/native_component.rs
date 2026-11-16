// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Where a Deck's optional Arcen USB component stands, and what the Deck says
//! about it.
//!
//! Native tablet needs a privileged component beside the unprivileged Deck: a
//! launchd daemon on macOS (ADR 0011), a `LocalSystem` service with a
//! per-tablet `WinUSB` package on Windows. The Native tablet option stays visible but
//! unavailable until the component is [`NativeUsbComponentState::Ready`], and
//! the Deck never falls back to Tablet support silently.
//!
//! Both Decks show the same words for the same state, with the platform noun
//! ("this Mac" / "this PC"); only the steps that differ (where an approval
//! happens, which system is required, what blocks it) are selected by
//! [`DeckPlatform`]. A platform adapter only reports facts
//! ([`NativeComponentFacts`]) or maps its own status onto a state; it never
//! writes its own guidance.

use std::fmt::{Display, Formatter};

/// The Deck platform whose wording a state is shown in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeckPlatform {
    /// The macOS Deck: a launchd daemon registered with `SMAppService`.
    MacOs,
    /// The Windows Deck: a separately installed service and tablet packages.
    Windows,
}

impl DeckPlatform {
    /// The platform noun in shared wording: "this Mac" or "this PC".
    #[must_use]
    pub const fn this_computer(self) -> &'static str {
        match self {
            Self::MacOs => "this Mac",
            Self::Windows => "this PC",
        }
    }
}

/// Longest component reason a Deck shows, in characters.
pub const MAX_COMPONENT_REASON_CHARS: usize = 240;

/// A reason the component itself gave, made safe to show: control characters
/// become spaces, runs of whitespace collapse, and the text is cut after
/// [`MAX_COMPONENT_REASON_CHARS`] characters, marked with an ellipsis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentReason(String);

impl ComponentReason {
    /// Bounds and cleans a reason received from the component.
    #[must_use]
    pub fn new(text: &str) -> Self {
        let mut out = String::new();
        let mut chars = 0;
        let mut pending_space = false;
        for c in text.chars() {
            if c.is_whitespace() || c.is_control() {
                pending_space = !out.is_empty();
                continue;
            }
            let needed = usize::from(pending_space) + 1;
            if chars + needed > MAX_COMPONENT_REASON_CHARS {
                out.push('…');
                break;
            }
            if pending_space {
                out.push(' ');
                chars += 1;
                pending_space = false;
            }
            out.push(c);
            chars += 1;
        }
        if out.is_empty() {
            out.push_str("no reason given");
        }
        Self(out)
    }

    /// The cleaned text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for ComponentReason {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where the Arcen USB component stands, from the Deck's point of view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeUsbComponentState {
    /// Not installed (or removed).
    NotInstalled,
    /// Installed, but the system waits for the user to approve it (macOS
    /// Login Items).
    AwaitingApproval,
    /// Installed and approved, but not running.
    ServiceStopped,
    /// The component runs, but the tablet's model has not been prepared for
    /// it yet (Windows: no `WinUSB` package for this tablet).
    DeviceNotPrepared,
    /// A system policy keeps the component from running.
    BlockedByPolicy,
    /// This system cannot run the component at all.
    UnsupportedOs,
    /// The component runs and can bridge the tablet.
    Ready,
    /// The component answered and refused, with its own reason.
    Refused(ComponentReason),
    /// A status this build does not recognise.
    Unknown,
}

/// What the component's status control does when clicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeComponentAction {
    /// Install the component (one administrator approval).
    Install,
    /// Take the user to where the system approves it.
    Approve,
    /// Start the installed component.
    Start,
    /// Prepare this tablet's model (one administrator approval per model).
    Prepare,
    /// Nothing to change; read the status again.
    Recheck,
}

impl NativeUsbComponentState {
    /// Whether Native tablet may be offered: the option is unavailable
    /// (greyed, with [`Self::guidance`]) in every other state.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// What the status control beside the Native tablet option does.
    #[must_use]
    pub const fn action(&self) -> NativeComponentAction {
        match self {
            Self::NotInstalled => NativeComponentAction::Install,
            Self::AwaitingApproval => NativeComponentAction::Approve,
            Self::ServiceStopped => NativeComponentAction::Start,
            Self::DeviceNotPrepared => NativeComponentAction::Prepare,
            Self::BlockedByPolicy
            | Self::UnsupportedOs
            | Self::Ready
            | Self::Refused(_)
            | Self::Unknown => NativeComponentAction::Recheck,
        }
    }

    /// The control's label: short enough for the option's own row.
    #[must_use]
    pub const fn action_label(&self, platform: DeckPlatform) -> &'static str {
        match self {
            Self::NotInstalled => "Install component…",
            Self::AwaitingApproval => "Approve component…",
            Self::ServiceStopped => "Start service…",
            Self::DeviceNotPrepared => "Prepare this tablet…",
            Self::BlockedByPolicy => "Blocked by policy",
            Self::UnsupportedOs => match platform {
                DeckPlatform::MacOs => "Requires macOS 13",
                DeckPlatform::Windows => "Requires Windows 11 x64",
            },
            Self::Ready => "Component ready",
            Self::Refused(_) | Self::Unknown => "Check component…",
        }
    }

    /// The sentence shown under the option, so no Deck invents wording for a
    /// state it did not expect.
    #[must_use]
    pub fn guidance(&self, platform: DeckPlatform) -> String {
        match self {
            Self::NotInstalled => "Native tablet needs the Arcen USB component. Installing it \
                                   asks for administrator approval once."
                .to_owned(),
            Self::AwaitingApproval => match platform {
                DeckPlatform::MacOs => "Approve the Arcen USB component in System Settings > \
                                        General > Login Items, then reconnect."
                    .to_owned(),
                DeckPlatform::Windows => "Approve the Arcen USB component in Windows, then \
                                          reconnect."
                    .to_owned(),
            },
            Self::ServiceStopped => {
                "The Arcen USB component is installed but not running. Start it, then \
                 reconnect."
                    .to_owned()
            }
            Self::DeviceNotPrepared => "Prepare this tablet for Native tablet. Preparing asks \
                                        for administrator approval once per tablet model."
                .to_owned(),
            Self::BlockedByPolicy => format!(
                "{} blocked the Arcen USB component. Use Tablet support on {}.",
                match platform {
                    DeckPlatform::MacOs => "macOS policy",
                    DeckPlatform::Windows => "Windows policy (Smart App Control or App Control)",
                },
                platform.this_computer()
            ),
            Self::UnsupportedOs => format!(
                "Native tablet needs {}. Use Tablet support on {}.",
                match platform {
                    DeckPlatform::MacOs => "macOS 13 or later",
                    DeckPlatform::Windows => "Windows 11 on an x64 PC",
                },
                platform.this_computer()
            ),
            Self::Ready => format!(
                "Native tablet is available. The tablet stops working on {} while a session \
                 uses it.",
                platform.this_computer()
            ),
            Self::Refused(reason) => {
                format!("The Arcen USB component refused Native tablet: {reason}")
            }
            Self::Unknown => "The Arcen USB component is in an unrecognised state.".to_owned(),
        }
    }
}

/// What a platform adapter observed, in the order a Deck learns it.
///
/// The adapter fills in only what it can observe; [`Self::resolve`] decides
/// the state, so both Decks rank the same problems the same way.
// Independent observations, each of which an adapter may or may not make.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeComponentFacts {
    /// The system cannot run the component (wrong OS version or CPU).
    pub unsupported_os: bool,
    /// A system policy blocks the component.
    pub blocked_by_policy: bool,
    /// The component is installed.
    pub installed: bool,
    /// The system still waits for the user to approve it.
    pub awaiting_approval: bool,
    /// The component answers its local endpoint.
    pub running: bool,
    /// The component's own answer when it refused, already received.
    pub refusal: Option<ComponentReason>,
    /// Whether the tablet in question is prepared; `None` when no tablet is
    /// being asked about, or the platform needs no preparation.
    pub device_prepared: Option<bool>,
}

impl NativeComponentFacts {
    /// The one state these facts mean. The first unmet prerequisite wins:
    /// the system, its policy, installation, approval, the running
    /// component, its own refusal, then the tablet.
    #[must_use]
    pub fn resolve(&self) -> NativeUsbComponentState {
        if self.unsupported_os {
            NativeUsbComponentState::UnsupportedOs
        } else if self.blocked_by_policy {
            NativeUsbComponentState::BlockedByPolicy
        } else if !self.installed {
            NativeUsbComponentState::NotInstalled
        } else if self.awaiting_approval {
            NativeUsbComponentState::AwaitingApproval
        } else if !self.running {
            NativeUsbComponentState::ServiceStopped
        } else if let Some(reason) = &self.refusal {
            NativeUsbComponentState::Refused(reason.clone())
        } else if self.device_prepared == Some(false) {
            NativeUsbComponentState::DeviceNotPrepared
        } else {
            NativeUsbComponentState::Ready
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLATFORMS: [DeckPlatform; 2] = [DeckPlatform::MacOs, DeckPlatform::Windows];

    fn every_state() -> Vec<NativeUsbComponentState> {
        vec![
            NativeUsbComponentState::NotInstalled,
            NativeUsbComponentState::AwaitingApproval,
            NativeUsbComponentState::ServiceStopped,
            NativeUsbComponentState::DeviceNotPrepared,
            NativeUsbComponentState::BlockedByPolicy,
            NativeUsbComponentState::UnsupportedOs,
            NativeUsbComponentState::Ready,
            NativeUsbComponentState::Refused(ComponentReason::new("tablet busy")),
            NativeUsbComponentState::Unknown,
        ]
    }

    #[test]
    fn only_ready_offers_native_tablet() {
        for state in every_state() {
            assert_eq!(
                state.is_ready(),
                state == NativeUsbComponentState::Ready,
                "{state:?}"
            );
        }
    }

    #[test]
    fn every_state_explains_itself_and_labels_its_control_on_both_decks() {
        for platform in PLATFORMS {
            for state in every_state() {
                assert!(!state.guidance(platform).is_empty(), "{state:?}");
                assert!(!state.action_label(platform).is_empty(), "{state:?}");
            }
        }
    }

    #[test]
    fn the_decks_differ_only_where_their_systems_do() {
        let differ: Vec<_> = every_state()
            .into_iter()
            .filter(|state| {
                state.guidance(DeckPlatform::MacOs) != state.guidance(DeckPlatform::Windows)
                    || state.action_label(DeckPlatform::MacOs)
                        != state.action_label(DeckPlatform::Windows)
            })
            .collect();
        assert_eq!(
            differ,
            [
                NativeUsbComponentState::AwaitingApproval,
                NativeUsbComponentState::BlockedByPolicy,
                NativeUsbComponentState::UnsupportedOs,
                NativeUsbComponentState::Ready,
            ]
        );
        assert_eq!(
            NativeUsbComponentState::Ready
                .guidance(DeckPlatform::Windows)
                .replace("this PC", "this Mac"),
            NativeUsbComponentState::Ready.guidance(DeckPlatform::MacOs),
            "Ready differs only in the platform noun"
        );
    }

    #[test]
    fn each_fixable_state_offers_its_own_fix() {
        use NativeComponentAction as A;
        use NativeUsbComponentState as S;
        assert_eq!(S::NotInstalled.action(), A::Install);
        assert_eq!(S::AwaitingApproval.action(), A::Approve);
        assert_eq!(S::ServiceStopped.action(), A::Start);
        assert_eq!(S::DeviceNotPrepared.action(), A::Prepare);
        for state in [
            S::BlockedByPolicy,
            S::UnsupportedOs,
            S::Ready,
            S::Refused(ComponentReason::new("x")),
            S::Unknown,
        ] {
            assert_eq!(state.action(), A::Recheck, "{state:?} cannot be fixed here");
        }
    }

    #[test]
    fn approval_on_a_mac_points_at_login_items() {
        assert!(
            NativeUsbComponentState::AwaitingApproval
                .guidance(DeckPlatform::MacOs)
                .contains("Login Items")
        );
    }

    #[test]
    fn a_refusal_is_shown_bounded_and_on_one_line() {
        let raw = format!("line one\r\nline\ttwo\u{7}{}", "x".repeat(1000));
        let reason = ComponentReason::new(&raw);
        assert!(reason.as_str().starts_with("line one line two x"));
        assert!(reason.as_str().chars().count() <= MAX_COMPONENT_REASON_CHARS + 1);
        assert!(reason.as_str().ends_with('…'));
        assert!(!reason.as_str().chars().any(char::is_control));
        let shown =
            NativeUsbComponentState::Refused(reason.clone()).guidance(DeckPlatform::Windows);
        assert!(shown.ends_with(reason.as_str()));
        assert_eq!(ComponentReason::new(" \n\t").as_str(), "no reason given");
        assert_eq!(ComponentReason::new("  ok  ").as_str(), "ok");
    }

    #[test]
    fn facts_resolve_to_the_first_unmet_prerequisite() {
        use NativeUsbComponentState as S;
        let ready = NativeComponentFacts {
            installed: true,
            running: true,
            device_prepared: Some(true),
            ..NativeComponentFacts::default()
        };
        assert_eq!(ready.resolve(), S::Ready);
        assert_eq!(
            NativeComponentFacts {
                device_prepared: None,
                ..ready.clone()
            }
            .resolve(),
            S::Ready
        );
        assert_eq!(
            NativeComponentFacts {
                device_prepared: Some(false),
                ..ready.clone()
            }
            .resolve(),
            S::DeviceNotPrepared
        );
        let refused = NativeComponentFacts {
            refusal: Some(ComponentReason::new("tablet in use")),
            device_prepared: Some(false),
            ..ready.clone()
        };
        assert_eq!(
            refused.resolve(),
            S::Refused(ComponentReason::new("tablet in use"))
        );
        let stopped = NativeComponentFacts {
            running: false,
            ..refused
        };
        assert_eq!(stopped.resolve(), S::ServiceStopped);
        let pending = NativeComponentFacts {
            awaiting_approval: true,
            ..stopped
        };
        assert_eq!(pending.resolve(), S::AwaitingApproval);
        let missing = NativeComponentFacts {
            installed: false,
            ..pending
        };
        assert_eq!(missing.resolve(), S::NotInstalled);
        let blocked = NativeComponentFacts {
            blocked_by_policy: true,
            ..missing
        };
        assert_eq!(blocked.resolve(), S::BlockedByPolicy);
        let unsupported = NativeComponentFacts {
            unsupported_os: true,
            ..blocked
        };
        assert_eq!(unsupported.resolve(), S::UnsupportedOs);
    }
}
