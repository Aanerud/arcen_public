// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The console's sign-in decision and a confirmed change of account on the
//! wire (ADR 0025 §13, `account_switch_v1`): the Linux adapter.
//!
//! After the requester authenticated, the shared ledger decides
//! (`SwitchLedger::decide`): the account's own session and a free console
//! go on to the existing console path; a refusal closes; another account's
//! session with nobody attached is offered for a graceful sign-out to a
//! Deck that can confirm it. Only a confirmation that still matches the
//! facts read again signs that session out, and only its proved end lets
//! the requester in through the console's own login.

use std::time::Duration;

use arcen_protocol::messages::{
    AccountSwitchChoiceMsg, AccountSwitchOfferMsg, AccountSwitchOutcomeMsg, AccountSwitchReplyMsg,
    AccountSwitchResultMsg, ACCOUNT_SWITCH_OFFER, ACCOUNT_SWITCH_REPLY, ACCOUNT_SWITCH_RESULT,
};
use arcen_session::console_session::{ConsoleNotice, ConsoleOrigin, ConsoleRefusal};
use arcen_session::session_handover::consent::{
    ConnectionId, LedgerDecision, Opened, ReplyRejection, SwitchNonce, DEFAULT_OFFER_LIFETIME_SECS,
    SWITCH_NONCE_BYTES,
};
use arcen_session::session_handover::displays::AttachmentKind;
use arcen_session::session_handover::logout::{EntryPermit, LogoutBlock, LogoutVerdict};
use arcen_session::session_handover::{
    AuthenticatedPrincipal, ConsoleOccupant, HandoverDecision, HandoverReason, HandoverRefusal,
    RemoteAttachment, RemoteClient, SwitchRoute,
};
use arcen_telemetry::{CorrelationId, LifecycleEventKind};
use futures_util::StreamExt;
use tokio_tungstenite::tungstenite::Message;

use crate::logging::target::SESSION;
use crate::session::handover;
use crate::session::identity::UserIdentity;
use crate::session::lifecycle::SessionRegistry;
use crate::LifecycleEmitter;

/// What the connection does after the decision.
pub(super) enum Admission {
    /// On to the existing console path; with a permit after a completed
    /// change of account (report how its sign-in ended with
    /// [`login_finished`]).
    Proceed(Option<EntryPermit>),
    /// Refused before anything changed.
    Refused(ConsoleNotice),
    /// The connection was answered and closed here.
    Closed,
}

/// Who the record is about.
pub(super) struct Requester<'a> {
    pub(super) identity: &'a UserIdentity,
    pub(super) session_log_id: &'a CorrelationId,
    pub(super) remote_host: &'a str,
    pub(super) emitter: &'a LifecycleEmitter,
    /// The Deck negotiated `account_switch_v1`.
    pub(super) deck_switch: bool,
}

impl Requester<'_> {
    /// `SESSION_HANDOVER` with `reason`.
    fn record(&self, reason: HandoverReason, client: Option<RemoteClient>) {
        tracing::info!(
            target: SESSION,
            username = %self.identity.username,
            reason_class = reason.token(),
            remote_client = client.map_or("", RemoteClient::token),
            "session handover"
        );
        let Some(fields) = reason.fields(Some(AttachmentKind::Connect), client) else {
            return;
        };
        let context = self.emitter.session_context(
            self.session_log_id.clone(),
            Some(self.identity.username.clone()),
            Some(self.remote_host.to_owned()),
            None,
        );
        crate::emit_lifecycle_event_with_context(
            self.emitter,
            LifecycleEventKind::SessionHandover,
            context,
            fields,
        );
    }
}

/// The shared decision for an authenticated console sign-in, and a
/// confirmed change of account when it offers one.
pub(super) async fn admit<S>(
    socket: &mut S,
    registry: &SessionRegistry,
    connection: ConnectionId,
    requester: &Requester<'_>,
) -> Admission
where
    S: futures_util::Sink<Message>
        + futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
    S::Error: std::fmt::Display,
{
    let identity = requester.identity;
    let Some(principal) = handover::principal(identity.uid, &identity.username) else {
        return Admission::Proceed(None);
    };
    let principal = AuthenticatedPrincipal::after_authentication(principal);
    let reading = handover::read(registry, identity.uid).await;
    let origin = reading
        .session
        .as_ref()
        .map_or(ConsoleOrigin::DisplayManager, |fact| fact.origin());
    let route = route(requester.deck_switch, &reading).await;
    let now = registry.resume().monotonic_now();
    // The decision and, for a join, the withdrawal of an open offer to
    // sign that session out happen under one lock: no confirmation can
    // land between them.
    let (LedgerDecision { decision, lapsed }, returned) = {
        let mut handover = registry.handover();
        let decided = handover
            .ledger
            .decide(connection, &principal, &reading.occupant, route, now);
        let returned = matches!(
            decided.decision,
            HandoverDecision::JoinOwnSession { .. }
                | HandoverDecision::JoinOwnSessionAfterIncumbent(_)
        )
        .then(|| handover.ledger.remote_client_returned());
        (decided, returned)
    };
    if let Some(lapse) = lapsed {
        requester.record(lapse.reason(), None);
    }
    match decision {
        HandoverDecision::JoinOwnSession { layout, .. } => {
            tracing::info!(target: SESSION, layout = layout.token(), "the account's own session is joined");
            returning(registry, requester, returned);
            requester.record(HandoverReason::ReusedDesktop, None);
            Admission::Proceed(None)
        }
        HandoverDecision::JoinOwnSessionAfterIncumbent(_) => {
            returning(registry, requester, returned);
            requester.record(HandoverReason::ReusedDesktop, Some(RemoteClient::Arcen));
            Admission::Proceed(None)
        }
        HandoverDecision::SignInFresh => Admission::Proceed(None),
        // Several desktops, or the account's own one whose identity could
        // not be read: the existing console decision speaks (it joins the
        // account's own session, refuses another's).
        HandoverDecision::Refuse(HandoverRefusal::OccupantUnknown)
            if reading.several
                || reading
                    .session
                    .as_ref()
                    .is_some_and(|fact| fact.uid == identity.uid) =>
        {
            Admission::Proceed(None)
        }
        HandoverDecision::Refuse(refusal) => {
            let client = match refusal {
                HandoverRefusal::Busy(client) => Some(client),
                _ => None,
            };
            if refusal == HandoverRefusal::AttachmentUnknown {
                // Recorded at every profile, naming what could not be told.
                tracing::error!(
                    target: SESSION,
                    refusal = refusal.token(),
                    reason = %reading.unknown.join("; "),
                    "the change of account is refused: whether someone is connected to the signed-in account's session cannot be told"
                );
            } else {
                tracing::warn!(target: SESSION, refusal = refusal.token(), "the console sign-in is refused before anything changes");
            }
            requester.record(refusal.reason(), client);
            Admission::Refused(
                refusal
                    .console_refusal(origin, requester.deck_switch)
                    .into(),
            )
        }
        HandoverDecision::OfferSwitch(candidate) => {
            let Some(fact) = reading.session else {
                return Admission::Refused(ConsoleRefusal::other_user(origin).into());
            };
            let mut bytes = [0_u8; SWITCH_NONCE_BYTES];
            let nonce = getrandom::getrandom(&mut bytes)
                .ok()
                .and_then(|()| SwitchNonce::from_random(bytes));
            let Some(nonce) = nonce else {
                return Admission::Refused(ConsoleRefusal::other_user(origin).into());
            };
            let opened = registry.handover().ledger.open(
                candidate,
                connection,
                nonce,
                now,
                DEFAULT_OFFER_LIFETIME_SECS,
            );
            match opened {
                Ok(opened) => {
                    offer(
                        socket, registry, connection, requester, opened, &fact, origin,
                    )
                    .await
                }
                Err(refusal) => {
                    requester.record(refusal.reason(), None);
                    Admission::Refused(
                        refusal
                            .handover_refusal()
                            .map_or(ConsoleRefusal::other_user(origin), |refusal| {
                                refusal.console_refusal(origin, true)
                            })
                            .into(),
                    )
                }
            }
        }
    }
}

/// The account that holds the console is attaching to its own session:
/// the open offer to sign it out was withdrawn with the decision
/// (`returned`), and the epoch advances now, before the attachment (which
/// takes a while) is visible to anyone else. A logout or a sign-in in
/// progress there concerns another session: the shared decision joins
/// the account's own one only when it is not the session being signed out.
fn returning(
    registry: &SessionRegistry,
    requester: &Requester<'_>,
    returned: Option<arcen_session::session_handover::consent::ClientReturn>,
) {
    use arcen_session::session_handover::consent::ClientReturn;
    registry.note_attachment();
    if returned == Some(ClientReturn::OfferWithdrawn) {
        requester.record(HandoverReason::SwitchStale, None);
    }
}

/// The route for this sign-in: the Pier signs out only where the account
/// holding the console has GNOME's session manager on its own bus. Asked
/// only when a sign-out could be offered.
async fn route(deck_switch: bool, reading: &handover::Reading) -> SwitchRoute {
    let other_detached = matches!(
        &reading.occupant,
        ConsoleOccupant::Session(facts) if facts.attachment == RemoteAttachment::ProvedDetached
    );
    let pier = match (&reading.session, deck_switch && other_detached) {
        (Some(fact), true) => handover::graceful_logout_route(fact).await,
        // An older Deck keeps today's refusal whatever the Pier can do.
        _ => true,
    };
    SwitchRoute::from_capabilities(deck_switch, pier)
}

/// Offers the sign-out, waits for the Deck's answer and carries it out.
async fn offer<S>(
    socket: &mut S,
    registry: &SessionRegistry,
    connection: ConnectionId,
    requester: &Requester<'_>,
    opened: Opened,
    fact: &arcen_session::console_session::ConsoleSessionFact,
    origin: ConsoleOrigin,
) -> Admission
where
    S: futures_util::Sink<Message>
        + futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
    S::Error: std::fmt::Display,
{
    if let Some(lapse) = opened.lapsed {
        requester.record(lapse.reason(), None);
    }
    let offer_id = opened.ticket.nonce.to_hex();
    let lifetime = opened.ticket.expires_in_secs;
    requester.record(HandoverReason::SwitchOffered, None);
    let message = AccountSwitchOfferMsg::new(offer_id.clone(), lifetime).ok();
    let sent = match message.as_ref().map(serde_json::to_string) {
        Some(Ok(json)) => {
            super::server::send_critical_control(socket, Message::Text(json), ACCOUNT_SWITCH_OFFER)
                .await
        }
        _ => false,
    };
    if !sent {
        left(registry, connection, requester);
        return Admission::Closed;
    }
    let reply =
        tokio::time::timeout(Duration::from_secs(u64::from(lifetime)), next_reply(socket)).await;
    let close = ConsoleRefusal::other_user(origin);
    let reply = match reply {
        Err(_) => {
            let lapse = registry
                .handover()
                .ledger
                .expire(registry.resume().monotonic_now());
            requester.record(
                lapse.map_or(HandoverReason::SwitchExpired, |lapse| lapse.reason()),
                None,
            );
            return finish(socket, &offer_id, AccountSwitchOutcomeMsg::Expired, close).await;
        }
        Ok(None) => {
            left(registry, connection, requester);
            return Admission::Closed;
        }
        Ok(Some(reply)) => reply,
    };
    let Some(presented) = SwitchNonce::parse_hex(&reply.offer_id) else {
        left(registry, connection, requester);
        return finish(socket, &offer_id, AccountSwitchOutcomeMsg::Rejected, close).await;
    };
    if reply.choice != AccountSwitchChoiceMsg::Confirm {
        let now = registry.resume().monotonic_now();
        let cancelled = registry
            .handover()
            .ledger
            .cancel(connection, &presented, now);
        let outcome = match cancelled {
            Ok(()) => {
                requester.record(HandoverReason::SwitchCancelled, None);
                AccountSwitchOutcomeMsg::Cancelled
            }
            Err(rejection) => {
                requester.record(rejection.reason(), None);
                rejected(rejection)
            }
        };
        return finish(socket, &offer_id, outcome, close).await;
    }
    // The facts again, immediately before anything is asked of the system.
    let fresh = handover::read(registry, requester.identity.uid).await;
    let now = registry.resume().monotonic_now();
    let confirmed =
        registry
            .handover()
            .ledger
            .confirm(connection, &presented, now, &fresh.occupant);
    let authority = match confirmed {
        Ok(authority) => authority,
        Err(rejection) => {
            tracing::warn!(target: SESSION, ?rejection, "the confirmed change of account no longer matches the console; nothing was signed out");
            requester.record(rejection.reason(), None);
            return finish(socket, &offer_id, rejected(rejection), close).await;
        }
    };
    let (outcome, present) = while_present(
        socket,
        handover::sign_out(authority.target(), fact, || {
            requester.record(HandoverReason::LogoutRequested, None);
        }),
    )
    .await;
    if !present {
        // The logout goes on; the ledger reserves nothing for a requester
        // that left (`CompletedRequesterGone`).
        left(registry, connection, requester);
    }
    let verdict = registry.handover().ledger.finish_logout(
        authority,
        outcome,
        registry.resume().monotonic_now(),
    );
    requester.record(verdict.reason(), None);
    match verdict {
        LogoutVerdict::Completed { permit, ended } => {
            tracing::info!(
                target: SESSION,
                session_id = %ended.session().as_str(),
                "the confirmed session is gone; the requester signs in through the console's own login"
            );
            let ((), present) = while_present(
                socket,
                handover::wait_for_login_route(requester.identity.uid),
            )
            .await;
            if !present {
                // `requester_left` already gave the console back.
                left(registry, connection, requester);
                drop(permit);
                return Admission::Closed;
            }
            let result = AccountSwitchResultMsg::new(offer_id, AccountSwitchOutcomeMsg::SignedOut);
            let sent = match serde_json::to_string(&result) {
                Ok(json) => {
                    super::server::send_critical_control(
                        socket,
                        Message::Text(json),
                        ACCOUNT_SWITCH_RESULT,
                    )
                    .await
                }
                Err(_) => false,
            };
            if !sent {
                login_finished(registry, requester, permit, false);
                return Admission::Closed;
            }
            Admission::Proceed(Some(permit))
        }
        LogoutVerdict::CompletedRequesterGone { .. } => Admission::Closed,
        LogoutVerdict::Blocked(block) => {
            let outcome = match block {
                LogoutBlock::Vetoed => AccountSwitchOutcomeMsg::LogoutVetoed,
                LogoutBlock::TimedOut => AccountSwitchOutcomeMsg::LogoutTimedOut,
                LogoutBlock::Failed | LogoutBlock::ExitUnproven | LogoutBlock::NotCommitting => {
                    AccountSwitchOutcomeMsg::LogoutFailed
                }
            };
            finish(socket, &offer_id, outcome, close).await
        }
    }
}

/// Runs `work` to its end while watching the requester's connection;
/// `false` when the requester left meanwhile (its connection closed or
/// failed). Messages the Deck sends meanwhile are not expected and are
/// dropped.
async fn while_present<S, F>(socket: &mut S, work: F) -> (F::Output, bool)
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    F: std::future::Future,
{
    tokio::pin!(work);
    loop {
        tokio::select! {
            output = &mut work => return (output, true),
            message = socket.next() => {
                if matches!(message, None | Some(Err(_) | Ok(Message::Close(_)))) {
                    return (work.await, false);
                }
            }
        }
    }
}

/// The result a rejected reply gets.
fn rejected(rejection: ReplyRejection) -> AccountSwitchOutcomeMsg {
    match rejection {
        ReplyRejection::Expired => AccountSwitchOutcomeMsg::Expired,
        ReplyRejection::Stale(_) => AccountSwitchOutcomeMsg::Stale,
        ReplyRejection::NoOffer
        | ReplyRejection::Replayed
        | ReplyRejection::WrongConnection
        | ReplyRejection::Mismatched => AccountSwitchOutcomeMsg::Rejected,
    }
}

/// The requester's connection ended while its offer or logout was open.
fn left(registry: &SessionRegistry, connection: ConnectionId, requester: &Requester<'_>) {
    if let Some(reason) = registry.handover().ledger.requester_left(connection) {
        requester.record(reason, None);
    }
}

/// How the requester's sign-in after a completed change of account ended;
/// the console is free again either way.
pub(super) fn login_finished(
    registry: &SessionRegistry,
    requester: &Requester<'_>,
    permit: EntryPermit,
    signed_in: bool,
) {
    if let Some(reason) = registry.handover().ledger.login_finished(permit, signed_in) {
        requester.record(reason, None);
    }
}

/// Sends the result, then closes with `close`'s reason.
async fn finish<S>(
    socket: &mut S,
    offer_id: &str,
    outcome: AccountSwitchOutcomeMsg,
    close: ConsoleRefusal,
) -> Admission
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    let result = AccountSwitchResultMsg::new(offer_id, outcome);
    if let Ok(json) = serde_json::to_string(&result) {
        let _ = super::server::send_critical_control(
            socket,
            Message::Text(json),
            ACCOUNT_SWITCH_RESULT,
        )
        .await;
    }
    let _ = super::server::send_critical_control(
        socket,
        super::server::close_with_reason(close.deck_reason()),
        "account_switch_close",
    )
    .await;
    Admission::Closed
}

/// The Deck's reply to the offer; `None` when the connection ended. Other
/// control messages are not expected before the sign-in finishes and are
/// skipped.
async fn next_reply<S>(socket: &mut S) -> Option<AccountSwitchReplyMsg>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match socket.next().await? {
            Ok(Message::Text(text)) => {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                if value.get("type").and_then(serde_json::Value::as_str)
                    != Some(ACCOUNT_SWITCH_REPLY)
                {
                    continue;
                }
                match serde_json::from_value::<AccountSwitchReplyMsg>(value) {
                    Ok(reply) if reply.validate().is_ok() => return Some(reply),
                    _ => {
                        tracing::warn!(target: SESSION, "a malformed account switch reply; it counts as no answer");
                        return None;
                    }
                }
            }
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(
        messages: Vec<Result<Message, tokio_tungstenite::tungstenite::Error>>,
    ) -> impl futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin
    {
        futures_util::stream::iter(messages).chain(futures_util::stream::pending())
    }

    #[tokio::test]
    async fn a_requester_that_closes_during_the_logout_is_noticed_and_the_logout_finishes() {
        let mut socket = stream(vec![Ok(Message::Close(None))]);
        let work = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            7
        };
        assert_eq!(while_present(&mut socket, work).await, (7, false));
    }

    #[tokio::test]
    async fn a_requester_that_stays_is_present_and_stray_messages_are_dropped() {
        let mut socket = stream(vec![Ok(Message::Text("{}".into()))]);
        let work = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            7
        };
        assert_eq!(while_present(&mut socket, work).await, (7, true));
    }

    #[test]
    fn every_rejection_and_block_has_its_wire_outcome() {
        use arcen_session::session_handover::consent::StaleReason;
        assert_eq!(
            rejected(ReplyRejection::Expired),
            AccountSwitchOutcomeMsg::Expired
        );
        assert_eq!(
            rejected(ReplyRejection::Stale(StaleReason::EpochChanged)),
            AccountSwitchOutcomeMsg::Stale
        );
        assert_eq!(
            rejected(ReplyRejection::Replayed),
            AccountSwitchOutcomeMsg::Rejected
        );
    }
}
