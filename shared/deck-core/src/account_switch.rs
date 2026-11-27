// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Deck's side of a confirmed change of account, both Decks alike
//! (protocol `account_switch_v1`, ADR 0025 §13).
//!
//! A Pier whose workstation another account holds, with no remote client
//! attached to it, may answer this Deck's sign-in with an
//! `account_switch_offer` instead of `auth_result`. The Deck asks its user,
//! Cancel being the default, and answers once with `account_switch_reply`.
//! The Pier then reports how it ended with `account_switch_result`: on
//! `signed_out` the sign-in goes on, on the same connection, and every other
//! outcome closes it.
//!
//! Two pieces live here, with every word the user reads:
//!
//! - [`AccountSwitchExchange`]: the protocol rules the session engine
//!   follows (one offer, one reply naming that offer, a `signed_out` only
//!   after this Deck confirmed);
//! - [`AccountSwitchPrompt`]: what the shells show (the question, then the
//!   progress), fed by the engine's [`AccountSwitchNews`].
//!
//! Leaving, closing the window or not answering before the offer expires
//! cancels. Nothing here names the other account or shows its desktop.

use std::fmt;
use std::time::{Duration, Instant};

use arcen_protocol::messages::{
    AccountSwitchChoiceMsg, AccountSwitchOfferMsg, AccountSwitchOutcomeMsg, AccountSwitchReplyMsg,
    AccountSwitchResultMsg, AccountSwitchValidationError,
};

/// The question's title.
pub const TITLE: &str = "Sign the other user out?";
/// The question.
pub const QUESTION: &str = "Another user is still signed in. Signing them out will close their \
                            applications. Continue only after they have saved their work.";
/// The default button: change nothing.
pub const CANCEL: &str = "Cancel";
/// The button that confirms.
pub const CONFIRM: &str = "Sign out and continue";
/// Shown after the user confirmed, until the Pier answers.
pub const SIGNING_OUT: &str = "Signing the other user out\u{2026}";
/// Shown after the other user was signed out, until the desktop appears.
pub const SIGNING_IN: &str = "The other user is signed out. Signing you in\u{2026}";

/// How long after the question appears "Sign out and continue" takes no
/// click or key. The host decides when the question appears, so a click
/// meant for the screen under it must not land on the destructive choice.
/// Cancel answers at once.
pub const CONFIRM_ARMING_DELAY: Duration = Duration::from_millis(600);

/// Whether a connection answers a host's offer to sign another account
/// out, and how. Only a connection that answers asks for the offer
/// (`AuthResponse.account_switch_v1`); every other one keeps the refusal
/// it always had.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AccountSwitchMode {
    /// Never asks: a tool that cannot answer.
    #[default]
    Off,
    /// The Deck's window asks its user ([`AccountSwitchPrompt`]).
    Interactive,
    /// Test only: a command-line tool answers with this at once
    /// (`--account-switch confirm|cancel`).
    Scripted(AccountSwitchChoiceMsg),
}

impl AccountSwitchMode {
    /// Whether the connection asks for the offer.
    #[must_use]
    pub const fn advertises(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// Why a change of account stopped without a desktop. Every value but
/// [`Self::NewLoginFailed`] signed nobody in; the logout outcomes say the
/// other user may still be signed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountSwitchStop {
    /// The user chose Cancel, closed the window, or the Pier withdrew the
    /// offer as this Deck left.
    Cancelled,
    /// Nobody answered before the offer expired.
    Expired,
    /// Something changed at the workstation before the confirmation (a
    /// remote client returned, the session changed).
    Stale,
    /// The Pier had no live offer for the reply.
    Rejected,
    /// An application or the other user stopped the sign-out.
    LogoutVetoed,
    /// The sign-out did not finish in time.
    LogoutTimedOut,
    /// The sign-out failed, or its end could not be proved.
    LogoutFailed,
    /// The other user was signed out, then this Deck's sign-in failed.
    NewLoginFailed,
    /// An outcome this Deck does not know.
    Unknown,
}

impl AccountSwitchStop {
    /// The stop a Pier's outcome states. `None` for `signed_out`, which
    /// goes on to sign in.
    #[must_use]
    pub const fn from_outcome(outcome: AccountSwitchOutcomeMsg) -> Option<Self> {
        Some(match outcome {
            AccountSwitchOutcomeMsg::SignedOut => return None,
            AccountSwitchOutcomeMsg::Cancelled => Self::Cancelled,
            AccountSwitchOutcomeMsg::Expired => Self::Expired,
            AccountSwitchOutcomeMsg::Stale => Self::Stale,
            AccountSwitchOutcomeMsg::Rejected => Self::Rejected,
            AccountSwitchOutcomeMsg::LogoutVetoed => Self::LogoutVetoed,
            AccountSwitchOutcomeMsg::LogoutTimedOut => Self::LogoutTimedOut,
            AccountSwitchOutcomeMsg::LogoutFailed => Self::LogoutFailed,
            AccountSwitchOutcomeMsg::Unknown => Self::Unknown,
        })
    }

    /// The `reason_class` of the Deck's session end, in the Pier's
    /// `SESSION_HANDOVER` vocabulary where one exists.
    #[must_use]
    pub const fn reason_class(self) -> &'static str {
        match self {
            Self::Cancelled => "switch_cancelled",
            Self::Expired => "switch_expired",
            Self::Stale => "switch_stale",
            Self::Rejected => "switch_rejected",
            Self::LogoutVetoed => "logout_vetoed",
            Self::LogoutTimedOut => "logout_timed_out",
            Self::LogoutFailed => "logout_failed",
            Self::NewLoginFailed => "new_login_failed",
            Self::Unknown => "switch_unknown",
        }
    }

    /// What the user reads, naming the host.
    #[must_use]
    pub fn message(self, host: &str) -> String {
        match self {
            Self::Cancelled => format!("Nobody was signed out at {host}."),
            Self::Expired => {
                format!("You didn\u{2019}t answer in time, so nobody was signed out at {host}.")
            }
            Self::Stale => format!(
                "Something changed at {host} before you confirmed, so nobody was signed out. \
                 Connect again to try once more."
            ),
            Self::Rejected => format!(
                "{host} no longer had that question open, so nobody was signed out. Connect \
                 again to try once more."
            ),
            Self::LogoutVetoed => format!(
                "An application stopped the sign-out at {host}, so the other user is still \
                 signed in. Some of their applications may have closed."
            ),
            Self::LogoutTimedOut => format!(
                "The sign-out at {host} didn\u{2019}t finish in time, so Arcen didn\u{2019}t \
                 sign you in. The other user may still be signed in."
            ),
            Self::LogoutFailed => format!(
                "{host} couldn\u{2019}t sign the other user out, so Arcen didn\u{2019}t sign \
                 you in."
            ),
            Self::NewLoginFailed => format!(
                "The other user is signed out, but {host} couldn\u{2019}t sign you in. Connect \
                 again to try once more."
            ),
            Self::Unknown => {
                format!("{host} stopped the change of account, so Arcen didn\u{2019}t sign you in.")
            }
        }
    }
}

/// A Pier broke the exchange's rules. The engine ends the connection; a
/// broken exchange never confirms and never signs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountSwitchProtocolError {
    /// An offer reached a Deck that did not set `account_switch_v1`.
    NotAdvertised,
    /// A message failed its own validation.
    Malformed(AccountSwitchValidationError),
    /// The result names another offer.
    OtherOffer,
    /// `signed_out` arrived although this Deck did not confirm.
    SignedOutWithoutConfirmation,
    /// The Deck tried to answer a second time.
    AlreadyAnswered,
}

impl fmt::Display for AccountSwitchProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAdvertised => formatter
                .write_str("the host offered a change of account this Deck did not ask for"),
            Self::Malformed(error) => write!(formatter, "{error}"),
            Self::OtherOffer => {
                formatter.write_str("the host's change-of-account result names another offer")
            }
            Self::SignedOutWithoutConfirmation => formatter.write_str(
                "the host signed the other account out without this Deck's confirmation",
            ),
            Self::AlreadyAnswered => {
                formatter.write_str("the change-of-account offer was already answered")
            }
        }
    }
}

impl std::error::Error for AccountSwitchProtocolError {}

/// What a result means for the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountSwitchStep {
    /// The other account's session ended; the Pier signs this Deck's
    /// account in and answers with `auth_result`.
    SignIn,
    /// The exchange ended without a sign-in; the Pier closes.
    Stop(AccountSwitchStop),
}

/// One offer on one connection, as the session engine follows it.
#[derive(Debug)]
pub struct AccountSwitchExchange {
    offer: AccountSwitchOfferMsg,
    answer: Option<AccountSwitchChoiceMsg>,
}

impl AccountSwitchExchange {
    /// Takes the Pier's offer. `advertised`: this Deck set
    /// `account_switch_v1` on its `auth_response`.
    ///
    /// # Errors
    ///
    /// An offer this Deck did not ask for, or a malformed one.
    pub fn offered(
        offer: AccountSwitchOfferMsg,
        advertised: bool,
    ) -> Result<Self, AccountSwitchProtocolError> {
        if !advertised {
            return Err(AccountSwitchProtocolError::NotAdvertised);
        }
        offer
            .validate()
            .map_err(AccountSwitchProtocolError::Malformed)?;
        Ok(Self {
            offer,
            answer: None,
        })
    }

    /// How long the user has to answer, from the offer's arrival.
    #[must_use]
    pub fn expires_in(&self) -> Duration {
        Duration::from_secs(u64::from(self.offer.expires_in_secs))
    }

    /// The answer sent, if any.
    #[must_use]
    pub const fn answer(&self) -> Option<AccountSwitchChoiceMsg> {
        self.answer
    }

    /// The one reply to this offer, naming it exactly.
    ///
    /// # Errors
    ///
    /// The offer was already answered.
    pub fn reply(
        &mut self,
        choice: AccountSwitchChoiceMsg,
    ) -> Result<AccountSwitchReplyMsg, AccountSwitchProtocolError> {
        if self.answer.is_some() {
            return Err(AccountSwitchProtocolError::AlreadyAnswered);
        }
        self.answer = Some(choice);
        Ok(AccountSwitchReplyMsg::new(&self.offer, choice))
    }

    /// What the Pier's result means. A Pier may end the offer before the
    /// Deck answers (it expired, a client returned); only `signed_out`
    /// needs this Deck's confirmation first.
    ///
    /// # Errors
    ///
    /// A malformed result, one for another offer, or `signed_out` without
    /// a confirmation.
    pub fn result(
        &self,
        result: &AccountSwitchResultMsg,
    ) -> Result<AccountSwitchStep, AccountSwitchProtocolError> {
        result
            .validate()
            .map_err(AccountSwitchProtocolError::Malformed)?;
        if result.offer_id != self.offer.offer_id {
            return Err(AccountSwitchProtocolError::OtherOffer);
        }
        match AccountSwitchStop::from_outcome(result.outcome) {
            Some(stop) => Ok(AccountSwitchStep::Stop(stop)),
            None if self.answer == Some(AccountSwitchChoiceMsg::Confirm) => {
                Ok(AccountSwitchStep::SignIn)
            }
            None => Err(AccountSwitchProtocolError::SignedOutWithoutConfirmation),
        }
    }
}

/// What the session engine tells the shells, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountSwitchNews {
    /// Ask the user; the engine cancels by itself after `expires_in`.
    Offered { expires_in: Duration },
    /// The user confirmed; the Pier is signing the other user out.
    SigningOut,
    /// The other user is signed out; the Pier is signing this one in.
    SignedOut,
}

/// What the user did with the question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountSwitchAction {
    None,
    /// Cancel, Escape, Enter on the default button, or a click outside.
    Cancel,
    /// Sign out and continue.
    Confirm,
}

/// What the shells draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountSwitchView {
    /// The question, Cancel focused.
    Question,
    /// A line of progress with no choice left to make.
    Progress(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Phase {
    #[default]
    Idle,
    Asking {
        opened: Instant,
        deadline: Instant,
    },
    /// Cancel was sent; the engine ends the connection.
    Cancelling,
    SigningOut,
    SigningIn,
}

/// The shells' view of the exchange: the question while it is open, then
/// the progress until the desktop appears or the connection ends. The
/// shell keeps one per connection and [`Self::reset`]s it when the
/// connection ends or a desktop appears.
#[derive(Debug, Default)]
pub struct AccountSwitchPrompt {
    phase: Phase,
    focus_pending: bool,
}

impl AccountSwitchPrompt {
    /// Applies the engine's news, `now` being when the shell read it.
    pub fn on_news(&mut self, news: AccountSwitchNews, now: Instant) {
        self.phase = match news {
            AccountSwitchNews::Offered { expires_in } => {
                self.focus_pending = true;
                Phase::Asking {
                    opened: now,
                    deadline: now + expires_in,
                }
            }
            AccountSwitchNews::SigningOut => Phase::SigningOut,
            AccountSwitchNews::SignedOut => Phase::SigningIn,
        };
    }

    /// Whether "Sign out and continue" takes a click or a key at `now`:
    /// only [`CONFIRM_ARMING_DELAY`] after the question appeared.
    #[must_use]
    pub fn confirm_armed(&self, now: Instant) -> bool {
        matches!(self.phase, Phase::Asking { opened, .. }
            if now.saturating_duration_since(opened) >= CONFIRM_ARMING_DELAY)
    }

    /// How long until "Sign out and continue" is armed, while the question
    /// is open and it is not yet.
    #[must_use]
    pub fn confirm_arms_in(&self, now: Instant) -> Option<Duration> {
        match self.phase {
            Phase::Asking { opened, .. } => {
                CONFIRM_ARMING_DELAY.checked_sub(now.saturating_duration_since(opened))
            }
            _ => None,
        }
        .filter(|left| !left.is_zero())
    }

    /// The user's action on the question at `now`. Returns the choice to
    /// send, once; nothing outside the question, and nothing for a
    /// confirmation before it is armed ([`Self::confirm_armed`]).
    pub fn answer(
        &mut self,
        action: AccountSwitchAction,
        now: Instant,
    ) -> Option<AccountSwitchChoiceMsg> {
        if !matches!(self.phase, Phase::Asking { .. }) {
            return None;
        }
        if action == AccountSwitchAction::Confirm && !self.confirm_armed(now) {
            // The keyboard goes back to Cancel, so a later Enter cancels.
            self.focus_pending = true;
            return None;
        }
        let choice = match action {
            AccountSwitchAction::None => return None,
            AccountSwitchAction::Cancel => {
                self.phase = Phase::Cancelling;
                AccountSwitchChoiceMsg::Cancel
            }
            AccountSwitchAction::Confirm => {
                self.phase = Phase::SigningOut;
                AccountSwitchChoiceMsg::Confirm
            }
        };
        self.focus_pending = false;
        Some(choice)
    }

    /// The offer expired while the question was open: the question goes
    /// and Cancel is the answer. The engine keeps its own deadline, so this
    /// only keeps the picture honest.
    pub fn expire(&mut self, now: Instant) -> Option<AccountSwitchChoiceMsg> {
        match self.phase {
            Phase::Asking { deadline, .. } if now >= deadline => {
                self.answer(AccountSwitchAction::Cancel, now)
            }
            _ => None,
        }
    }

    /// The connection ended, or a desktop appeared.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Whether the question is open.
    #[must_use]
    pub const fn is_asking(&self) -> bool {
        matches!(self.phase, Phase::Asking { .. })
    }

    /// What to draw, if anything.
    #[must_use]
    pub const fn view(&self) -> Option<AccountSwitchView> {
        match self.phase {
            Phase::Idle | Phase::Cancelling => None,
            Phase::Asking { .. } => Some(AccountSwitchView::Question),
            Phase::SigningOut => Some(AccountSwitchView::Progress(SIGNING_OUT)),
            Phase::SigningIn => Some(AccountSwitchView::Progress(SIGNING_IN)),
        }
    }

    /// Whether Cancel takes the keyboard focus this pass: once, when the
    /// question opens.
    pub fn take_focus(&mut self) -> bool {
        std::mem::take(&mut self.focus_pending) && self.is_asking()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFER_ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const OTHER_ID: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    fn offer() -> AccountSwitchOfferMsg {
        AccountSwitchOfferMsg::new(OFFER_ID, 120).unwrap()
    }

    fn result(outcome: AccountSwitchOutcomeMsg) -> AccountSwitchResultMsg {
        AccountSwitchResultMsg::new(OFFER_ID, outcome)
    }

    #[test]
    fn an_offer_reaches_only_a_deck_that_asked_for_it() {
        assert_eq!(
            AccountSwitchExchange::offered(offer(), false).unwrap_err(),
            AccountSwitchProtocolError::NotAdvertised
        );
        let mut malformed = offer();
        malformed.expires_in_secs = 0;
        assert!(matches!(
            AccountSwitchExchange::offered(malformed, true),
            Err(AccountSwitchProtocolError::Malformed(
                AccountSwitchValidationError::Lifetime
            ))
        ));
        let exchange = AccountSwitchExchange::offered(offer(), true).unwrap();
        assert_eq!(exchange.expires_in(), Duration::from_secs(120));
        assert_eq!(exchange.answer(), None);
    }

    #[test]
    fn the_reply_names_the_offer_exactly_and_is_sent_once() {
        for choice in [
            AccountSwitchChoiceMsg::Confirm,
            AccountSwitchChoiceMsg::Cancel,
        ] {
            let mut exchange = AccountSwitchExchange::offered(offer(), true).unwrap();
            let reply = exchange.reply(choice).unwrap();
            assert_eq!(reply.offer_id, OFFER_ID);
            assert_eq!(reply.choice, choice);
            reply.validate().unwrap();
            assert_eq!(
                exchange.reply(AccountSwitchChoiceMsg::Confirm).unwrap_err(),
                AccountSwitchProtocolError::AlreadyAnswered
            );
            assert_eq!(exchange.answer(), Some(choice));
        }
    }

    #[test]
    fn signed_out_signs_in_only_after_a_confirmation() {
        let unanswered = AccountSwitchExchange::offered(offer(), true).unwrap();
        assert_eq!(
            unanswered
                .result(&result(AccountSwitchOutcomeMsg::SignedOut))
                .unwrap_err(),
            AccountSwitchProtocolError::SignedOutWithoutConfirmation
        );
        let mut cancelled = AccountSwitchExchange::offered(offer(), true).unwrap();
        cancelled.reply(AccountSwitchChoiceMsg::Cancel).unwrap();
        assert_eq!(
            cancelled
                .result(&result(AccountSwitchOutcomeMsg::SignedOut))
                .unwrap_err(),
            AccountSwitchProtocolError::SignedOutWithoutConfirmation
        );
        let mut confirmed = AccountSwitchExchange::offered(offer(), true).unwrap();
        confirmed.reply(AccountSwitchChoiceMsg::Confirm).unwrap();
        assert_eq!(
            confirmed
                .result(&result(AccountSwitchOutcomeMsg::SignedOut))
                .unwrap(),
            AccountSwitchStep::SignIn
        );
    }

    #[test]
    fn a_result_for_another_offer_or_a_malformed_one_is_refused() {
        let mut exchange = AccountSwitchExchange::offered(offer(), true).unwrap();
        exchange.reply(AccountSwitchChoiceMsg::Confirm).unwrap();
        assert_eq!(
            exchange
                .result(&AccountSwitchResultMsg::new(
                    OTHER_ID,
                    AccountSwitchOutcomeMsg::SignedOut
                ))
                .unwrap_err(),
            AccountSwitchProtocolError::OtherOffer
        );
        assert!(matches!(
            exchange.result(&AccountSwitchResultMsg::new(
                "ABC",
                AccountSwitchOutcomeMsg::SignedOut
            )),
            Err(AccountSwitchProtocolError::Malformed(
                AccountSwitchValidationError::OfferId
            ))
        ));
    }

    #[test]
    fn every_other_outcome_stops_in_plain_words_even_before_an_answer() {
        let exchange = AccountSwitchExchange::offered(offer(), true).unwrap();
        let cases = [
            (
                AccountSwitchOutcomeMsg::Cancelled,
                AccountSwitchStop::Cancelled,
                "switch_cancelled",
            ),
            (
                AccountSwitchOutcomeMsg::Expired,
                AccountSwitchStop::Expired,
                "switch_expired",
            ),
            (
                AccountSwitchOutcomeMsg::Stale,
                AccountSwitchStop::Stale,
                "switch_stale",
            ),
            (
                AccountSwitchOutcomeMsg::Rejected,
                AccountSwitchStop::Rejected,
                "switch_rejected",
            ),
            (
                AccountSwitchOutcomeMsg::LogoutVetoed,
                AccountSwitchStop::LogoutVetoed,
                "logout_vetoed",
            ),
            (
                AccountSwitchOutcomeMsg::LogoutTimedOut,
                AccountSwitchStop::LogoutTimedOut,
                "logout_timed_out",
            ),
            (
                AccountSwitchOutcomeMsg::LogoutFailed,
                AccountSwitchStop::LogoutFailed,
                "logout_failed",
            ),
            (
                AccountSwitchOutcomeMsg::Unknown,
                AccountSwitchStop::Unknown,
                "switch_unknown",
            ),
        ];
        let mut messages = std::collections::BTreeSet::new();
        for (outcome, stop, class) in cases {
            assert_eq!(
                exchange.result(&result(outcome)).unwrap(),
                AccountSwitchStep::Stop(stop)
            );
            assert_eq!(stop.reason_class(), class);
            let message = stop.message("Studio");
            assert!(message.contains("Studio"), "{message}");
            assert!(messages.insert(message));
        }
        let failed = AccountSwitchStop::NewLoginFailed.message("Studio");
        assert!(failed.starts_with("The other user is signed out"));
        assert!(messages.insert(failed));
        assert!(
            AccountSwitchStop::LogoutVetoed
                .message("Studio")
                .contains("still signed in")
        );
    }

    #[test]
    fn the_prompt_asks_once_with_cancel_focused_then_shows_progress() {
        let now = Instant::now();
        let mut prompt = AccountSwitchPrompt::default();
        assert_eq!(prompt.view(), None);
        assert!(!prompt.take_focus());
        prompt.on_news(
            AccountSwitchNews::Offered {
                expires_in: Duration::from_secs(120),
            },
            now,
        );
        assert_eq!(prompt.view(), Some(AccountSwitchView::Question));
        assert!(prompt.take_focus());
        assert!(!prompt.take_focus(), "the focus moves to Cancel once");
        let armed = now + CONFIRM_ARMING_DELAY;
        assert_eq!(prompt.answer(AccountSwitchAction::None, armed), None);
        assert_eq!(
            prompt.answer(AccountSwitchAction::Confirm, armed),
            Some(AccountSwitchChoiceMsg::Confirm)
        );
        assert_eq!(
            prompt.view(),
            Some(AccountSwitchView::Progress(SIGNING_OUT))
        );
        assert_eq!(
            prompt.answer(AccountSwitchAction::Cancel, armed),
            None,
            "no second answer"
        );
        prompt.on_news(AccountSwitchNews::SigningOut, now);
        prompt.on_news(AccountSwitchNews::SignedOut, now);
        assert_eq!(prompt.view(), Some(AccountSwitchView::Progress(SIGNING_IN)));
        prompt.reset();
        assert_eq!(prompt.view(), None);
    }

    #[test]
    fn cancel_and_expiry_close_the_question() {
        let now = Instant::now();
        let mut prompt = AccountSwitchPrompt::default();
        prompt.on_news(
            AccountSwitchNews::Offered {
                expires_in: Duration::from_secs(30),
            },
            now,
        );
        assert_eq!(
            prompt.answer(AccountSwitchAction::Cancel, now),
            Some(AccountSwitchChoiceMsg::Cancel),
            "Cancel answers at once"
        );
        assert_eq!(prompt.view(), None);
        assert!(!prompt.is_asking());

        prompt.on_news(
            AccountSwitchNews::Offered {
                expires_in: Duration::from_secs(30),
            },
            now,
        );
        assert_eq!(prompt.expire(now + Duration::from_secs(29)), None);
        assert!(prompt.is_asking());
        assert_eq!(
            prompt.expire(now + Duration::from_secs(30)),
            Some(AccountSwitchChoiceMsg::Cancel)
        );
        assert_eq!(prompt.view(), None);
        assert_eq!(
            prompt.answer(AccountSwitchAction::Confirm, now + Duration::from_secs(31)),
            None,
            "an expired question cannot be confirmed"
        );
    }

    #[test]
    fn sign_out_and_continue_takes_nothing_until_it_is_armed() {
        let now = Instant::now();
        let mut prompt = AccountSwitchPrompt::default();
        prompt.on_news(
            AccountSwitchNews::Offered {
                expires_in: Duration::from_secs(120),
            },
            now,
        );
        assert!(!prompt.confirm_armed(now));
        assert_eq!(prompt.confirm_arms_in(now), Some(CONFIRM_ARMING_DELAY));
        let early = now + CONFIRM_ARMING_DELAY - Duration::from_millis(1);
        assert!(prompt.take_focus());
        assert_eq!(prompt.answer(AccountSwitchAction::Confirm, early), None);
        assert!(prompt.is_asking(), "an early click changes nothing");
        assert!(prompt.take_focus(), "the focus goes back to Cancel");
        let armed = now + CONFIRM_ARMING_DELAY;
        assert!(prompt.confirm_armed(armed));
        assert_eq!(prompt.confirm_arms_in(armed), None);
        assert_eq!(
            prompt.answer(AccountSwitchAction::Confirm, armed),
            Some(AccountSwitchChoiceMsg::Confirm)
        );
    }
}
