//! Whether a host session may carry the clipboard at all.
//!
//! The checks run before negotiation. A host that is not eligible advertises
//! `disabled` and never reads its desktop clipboard, whatever the client asks
//! for. Every Linux desktop provider makes this decision from the same facts,
//! so the rule lives here once and is not written separately per provider.

/// What a host knows about the session it would carry the clipboard for.
///
/// Four independent yes/no facts, each a separate refusal reason; an enum
/// would hide which one failed.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipboardHostFacts {
    /// The client authenticated as the session's user.
    pub authenticated: bool,
    /// The desktop belongs to this session alone (not a shared or console
    /// display that someone else is using).
    pub dedicated_desktop: bool,
    /// The session's user is not a privileged account.
    pub unprivileged_user: bool,
    /// The provider proved a native clipboard bridge for this session.
    pub native_bridge: bool,
}

/// Why a session carries no clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardIneligible {
    /// No per-user authentication.
    NotAuthenticated,
    /// The desktop is shared or not the session's own.
    SharedDesktop,
    /// The session runs as a privileged user.
    PrivilegedUser,
    /// The desktop provider has no proven clipboard bridge.
    NoNativeBridge,
}

impl ClipboardIneligible {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::NotAuthenticated => "not_authenticated",
            Self::SharedDesktop => "shared_desktop",
            Self::PrivilegedUser => "privileged_user",
            Self::NoNativeBridge => "no_native_bridge",
        }
    }
}

/// Decides eligibility, naming the first reason it fails.
///
/// # Errors
///
/// [`ClipboardIneligible`] when the session may not carry the clipboard.
pub const fn clipboard_eligibility(facts: ClipboardHostFacts) -> Result<(), ClipboardIneligible> {
    if !facts.authenticated {
        return Err(ClipboardIneligible::NotAuthenticated);
    }
    if !facts.dedicated_desktop {
        return Err(ClipboardIneligible::SharedDesktop);
    }
    if !facts.unprivileged_user {
        return Err(ClipboardIneligible::PrivilegedUser);
    }
    if !facts.native_bridge {
        return Err(ClipboardIneligible::NoNativeBridge);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ELIGIBLE: ClipboardHostFacts = ClipboardHostFacts {
        authenticated: true,
        dedicated_desktop: true,
        unprivileged_user: true,
        native_bridge: true,
    };

    #[test]
    fn every_fact_is_required_and_named() {
        assert_eq!(clipboard_eligibility(ELIGIBLE), Ok(()));
        let cases = [
            (
                ClipboardHostFacts {
                    authenticated: false,
                    ..ELIGIBLE
                },
                "not_authenticated",
            ),
            (
                ClipboardHostFacts {
                    dedicated_desktop: false,
                    ..ELIGIBLE
                },
                "shared_desktop",
            ),
            (
                ClipboardHostFacts {
                    unprivileged_user: false,
                    ..ELIGIBLE
                },
                "privileged_user",
            ),
            (
                ClipboardHostFacts {
                    native_bridge: false,
                    ..ELIGIBLE
                },
                "no_native_bridge",
            ),
        ];
        for (facts, token) in cases {
            assert_eq!(clipboard_eligibility(facts).unwrap_err().token(), token);
        }
    }
}
