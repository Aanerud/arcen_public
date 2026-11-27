// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The acceptance matrix of ADR 0025 §13, as far as shared code decides it.

use super::consent::{
    ClientReturn, ConnectionId, ConsoleHold, DEFAULT_OFFER_LIFETIME_SECS, ENTRY_DEADLINE,
    LOGOUT_COMMIT_DEADLINE, Lapse, MAX_OFFER_LIFETIME_SECS, OpenRefusal, ReplyRejection,
    StaleReason, SwitchLedger, SwitchNonce,
};
use super::displays::{
    AttachmentDisplays, AttachmentKind, DisplayAuthority, HeldDesktop, ReattachDisplays,
    attachment_displays, reattach_displays, resume_grant_authority,
};
use super::logout::{LogoutBlock, LogoutOutcome, LogoutVerdict, SessionExitProof, SessionPresence};
use super::*;
use crate::console_session::guest_layout::{
    ForeignClient, GuestLayoutFacts, GuestLayoutSetting, guest_layout_policy,
};
use crate::direct_reconnect::MonotonicMillis;

const ALICE: AccountPrincipal<'static> = AccountPrincipal {
    name: "alice",
    id: 1000,
};
const BOB: AccountPrincipal<'static> = AccountPrincipal {
    name: "bob",
    id: 1001,
};
const T0: MonotonicMillis = MonotonicMillis::new(50_000);
const REQUESTER: ConnectionId = ConnectionId::new(7);
const CONTENDER: ConnectionId = ConnectionId::new(8);

/// [`decide_handover`] with nothing reserved.
fn decide(
    requester: &AuthenticatedPrincipal,
    occupant: &ConsoleOccupant,
    route: SwitchRoute,
) -> HandoverDecision {
    decide_handover(requester, occupant, route, ConsoleHold::Free)
}

fn principal(account: AccountPrincipal<'_>) -> NativePrincipal {
    NativePrincipal::from_account(account).unwrap()
}

fn signed_in(account: AccountPrincipal<'_>) -> AuthenticatedPrincipal {
    AuthenticatedPrincipal::after_authentication(principal(account))
}

fn identity(
    account: AccountPrincipal<'_>,
    session: &str,
    boot: &str,
    generation: u64,
) -> NativeSessionIdentity {
    NativeSessionIdentity::new(
        principal(account),
        NativeSessionId::new(session).unwrap(),
        BootId::new(boot).unwrap(),
        generation,
    )
}

fn alice_session() -> NativeSessionIdentity {
    identity(ALICE, "26", "boot-a", 41)
}

fn occupant(
    identity: NativeSessionIdentity,
    attachment: RemoteAttachment,
    epoch: u64,
    desk: DeskState,
) -> ConsoleOccupant {
    ConsoleOccupant::Session(SessionFacts {
        identity,
        attachment,
        epoch: AttachmentEpoch::new(epoch),
        desk,
    })
}

fn alice_detached() -> ConsoleOccupant {
    occupant(
        alice_session(),
        RemoteAttachment::ProvedDetached,
        3,
        DeskState::Locked,
    )
}

fn nonce(seed: u8) -> SwitchNonce {
    SwitchNonce::from_random([seed; 32]).unwrap()
}

fn candidate(occupant: &ConsoleOccupant) -> SwitchCandidate {
    match decide(&signed_in(BOB), occupant, SwitchRoute::Available) {
        HandoverDecision::OfferSwitch(candidate) => candidate,
        other => panic!("expected an offer, got {other:?}"),
    }
}

/// Bob's open offer to sign Alice out, made on `REQUESTER`.
fn open_offer(ledger: &mut SwitchLedger, seed: u8) -> SwitchNonce {
    let opened = ledger
        .open(
            candidate(&alice_detached()),
            REQUESTER,
            nonce(seed),
            T0,
            DEFAULT_OFFER_LIFETIME_SECS,
        )
        .unwrap();
    assert_eq!(opened.lapsed, None);
    assert_eq!(opened.ticket.expires_in_secs, DEFAULT_OFFER_LIFETIME_SECS);
    opened.ticket.nonce
}

fn later(seconds: u128) -> MonotonicMillis {
    MonotonicMillis::new(T0.get() + seconds * 1_000)
}

// Same account.

#[test]
fn the_same_account_rejoins_its_own_session_from_any_deck() {
    for desk in [DeskState::Locked, DeskState::Unlocked, DeskState::Unknown] {
        for route in [
            SwitchRoute::Available,
            SwitchRoute::DeckCannotConfirm,
            SwitchRoute::PierCannotSignOut,
        ] {
            let facts = occupant(alice_session(), RemoteAttachment::ProvedDetached, 9, desk);
            let decision = decide(&signed_in(ALICE), &facts, route);
            assert_eq!(
                decision,
                HandoverDecision::JoinOwnSession {
                    identity: alice_session(),
                    layout: GuestLayoutPolicy::ApplyOnAttach,
                }
            );
            assert_eq!(decision.reason(), Some(HandoverReason::ReusedDesktop));
        }
    }
}

#[test]
fn the_same_account_with_another_arcen_deck_attached_keeps_the_same_session() {
    let facts = occupant(
        alice_session(),
        RemoteAttachment::Attached(RemoteClient::Arcen),
        4,
        DeskState::Unlocked,
    );
    let decision = decide(&signed_in(ALICE), &facts, SwitchRoute::Available);
    assert_eq!(
        decision,
        HandoverDecision::JoinOwnSessionAfterIncumbent(alice_session())
    );
    assert_eq!(decision.layout(), Some(GuestLayoutPolicy::ApplyOnAttach));
}

#[test]
fn the_same_account_joins_its_session_while_another_product_is_attached_or_unknown() {
    for (attachment, because, foreign) in [
        (
            RemoteAttachment::Attached(RemoteClient::Foreign),
            ShownBecause::ClientAttached,
            ForeignClient::Attached,
        ),
        (
            RemoteAttachment::Unknown,
            ShownBecause::ClientUnknown,
            ForeignClient::Unknown,
        ),
    ] {
        for route in [
            SwitchRoute::Available,
            SwitchRoute::DeckCannotConfirm,
            SwitchRoute::PierCannotSignOut,
        ] {
            let facts = occupant(alice_session(), attachment, 4, DeskState::Unlocked);
            let decision = decide(&signed_in(ALICE), &facts, route);
            assert_eq!(
                decision,
                HandoverDecision::JoinOwnSession {
                    identity: alice_session(),
                    layout: GuestLayoutPolicy::Shown(because),
                },
                "{attachment:?} never refuses the account's own session"
            );
            assert_eq!(decision.reason(), Some(HandoverReason::ReusedDesktop));
        }
        // The same rule as another product's desktop on main (ADR 0025 §8).
        assert_eq!(attachment.foreign_client(), foreign);
        assert_eq!(
            guest_layout_policy(GuestLayoutFacts {
                origin: ConsoleOrigin::OtherProduct,
                foreign_client: foreign,
                setting: GuestLayoutSetting::Deck,
            }),
            Some(attachment.own_session_layout())
        );
    }
}

#[test]
fn an_alias_or_a_reused_name_is_not_the_same_account() {
    let renamed = AccountPrincipal {
        name: "alice",
        id: 2000,
    };
    let decision = decide(
        &signed_in(renamed),
        &alice_detached(),
        SwitchRoute::Available,
    );
    assert!(matches!(decision, HandoverDecision::OfferSwitch(_)));
}

// Another account.

#[test]
fn another_account_is_refused_while_arcen_or_another_product_is_attached() {
    for (client, code) in [
        (RemoteClient::Arcen, ConsoleRefusal::OtherUser),
        (RemoteClient::Foreign, ConsoleRefusal::OtherUserOtherProduct),
    ] {
        for desk in [DeskState::Locked, DeskState::Unlocked] {
            let facts = occupant(alice_session(), RemoteAttachment::Attached(client), 2, desk);
            let decision = decide(&signed_in(BOB), &facts, SwitchRoute::Available);
            let HandoverDecision::Refuse(refusal) = decision else {
                panic!("expected a refusal, got {decision:?}");
            };
            assert_eq!(refusal, HandoverRefusal::Busy(client));
            assert_eq!(
                refusal.console_refusal(ConsoleOrigin::DisplayManager, true),
                code
            );
            assert_eq!(refusal.reason(), HandoverReason::RemoteBusy);
        }
    }
}

#[test]
fn unknown_facts_refuse_and_are_never_read_as_detached() {
    let decision = decide(
        &signed_in(BOB),
        &ConsoleOccupant::Unknown,
        SwitchRoute::Available,
    );
    assert_eq!(
        decision,
        HandoverDecision::Refuse(HandoverRefusal::OccupantUnknown)
    );
    let facts = occupant(
        alice_session(),
        RemoteAttachment::Unknown,
        2,
        DeskState::Locked,
    );
    assert_eq!(
        decide(&signed_in(BOB), &facts, SwitchRoute::Available),
        HandoverDecision::Refuse(HandoverRefusal::AttachmentUnknown),
        "unknown blocks only a change of account"
    );
    assert_eq!(
        HandoverRefusal::AttachmentUnknown.console_refusal(ConsoleOrigin::Arcen, true),
        ConsoleRefusal::RemoteStateUnknown
    );
    assert_eq!(
        HandoverRefusal::OccupantUnknown.console_refusal(ConsoleOrigin::Arcen, true),
        ConsoleRefusal::SessionsUnreadable
    );
}

#[test]
fn a_locked_and_an_unlocked_desk_get_the_same_offer() {
    let offers: Vec<_> = [DeskState::Locked, DeskState::Unlocked, DeskState::Unknown]
        .into_iter()
        .map(|desk| {
            let facts = occupant(alice_session(), RemoteAttachment::ProvedDetached, 3, desk);
            decide(&signed_in(BOB), &facts, SwitchRoute::Available)
        })
        .collect();
    assert!(matches!(offers[0], HandoverDecision::OfferSwitch(_)));
    assert!(offers.iter().all(|offer| *offer == offers[0]));
    assert_eq!(offers[0].reason(), Some(HandoverReason::SwitchOffered));
}

#[test]
fn nobody_signed_in_signs_in_afresh() {
    assert_eq!(
        decide(
            &signed_in(BOB),
            &ConsoleOccupant::Nobody,
            SwitchRoute::DeckCannotConfirm
        ),
        HandoverDecision::SignInFresh
    );
}

#[test]
fn an_older_deck_or_a_pier_without_graceful_logout_keeps_todays_refusal() {
    assert_eq!(
        SwitchRoute::from_capabilities(false, true),
        SwitchRoute::DeckCannotConfirm
    );
    assert_eq!(
        SwitchRoute::from_capabilities(true, false),
        SwitchRoute::PierCannotSignOut
    );
    assert_eq!(
        SwitchRoute::from_capabilities(true, true),
        SwitchRoute::Available
    );
    for (route, refusal) in [
        (
            SwitchRoute::DeckCannotConfirm,
            HandoverRefusal::DeckCannotConfirm,
        ),
        (
            SwitchRoute::PierCannotSignOut,
            HandoverRefusal::PierCannotSignOut,
        ),
    ] {
        let decision = decide(&signed_in(BOB), &alice_detached(), route);
        assert_eq!(decision, HandoverDecision::Refuse(refusal));
        for origin in [
            ConsoleOrigin::DisplayManager,
            ConsoleOrigin::Arcen,
            ConsoleOrigin::OtherProduct,
        ] {
            assert_eq!(
                refusal.console_refusal(origin, true),
                ConsoleRefusal::other_user(origin)
            );
        }
    }
}

// The transaction.

#[test]
fn a_confirmed_offer_revalidates_and_authorizes_exactly_the_target() {
    let mut ledger = SwitchLedger::new();
    let offered = open_offer(&mut ledger, 1);
    let authority = ledger
        .confirm(REQUESTER, &offered, later(30), &alice_detached())
        .unwrap();
    assert_eq!(authority.target(), &alice_session());
    assert_eq!(authority.requester(), &signed_in(BOB));
    assert_eq!(authority.connection(), REQUESTER);
    assert!(ledger.is_signing_out(&alice_session()));
    let proof = SessionExitProof::read_back(&alice_session(), SessionPresence::Gone).unwrap();
    let verdict = ledger.finish_logout(authority, LogoutOutcome::Completed(proof), later(40));
    assert_eq!(verdict.reason(), HandoverReason::LogoutCompleted);
    let LogoutVerdict::Completed { permit, ended } = verdict else {
        panic!("expected a completed logout");
    };
    assert_eq!(ended, alice_session());
    assert_eq!(permit.requester(), &signed_in(BOB));
    assert_eq!(permit.connection(), REQUESTER);
    assert!(
        !ledger.is_idle(),
        "the console is reserved for the requester"
    );
    assert_eq!(ledger.login_finished(permit, true), None);
    assert!(ledger.is_idle());
}

#[test]
fn cancel_expiry_and_a_departed_requester_change_nothing() {
    let mut ledger = SwitchLedger::new();
    let offered = open_offer(&mut ledger, 1);
    ledger.cancel(REQUESTER, &offered, later(5)).unwrap();
    assert!(ledger.is_idle());
    assert_eq!(
        ledger
            .confirm(REQUESTER, &offered, later(6), &alice_detached())
            .err(),
        Some(ReplyRejection::Replayed)
    );

    let offered = open_offer(&mut ledger, 2);
    assert_eq!(
        ledger.expire(later(u128::from(DEFAULT_OFFER_LIFETIME_SECS) - 1)),
        None
    );
    let lapse = ledger.expire(later(u128::from(DEFAULT_OFFER_LIFETIME_SECS)));
    assert_eq!(lapse, Some(Lapse::OfferExpired));
    assert_eq!(lapse.unwrap().reason(), HandoverReason::SwitchExpired);
    assert_eq!(
        ledger
            .confirm(REQUESTER, &offered, later(1), &alice_detached())
            .err(),
        Some(ReplyRejection::Replayed)
    );

    let offered = open_offer(&mut ledger, 3);
    assert_eq!(
        ledger
            .confirm(
                REQUESTER,
                &offered,
                later(u128::from(DEFAULT_OFFER_LIFETIME_SECS)),
                &alice_detached()
            )
            .err(),
        Some(ReplyRejection::Expired)
    );
    assert_eq!(
        ReplyRejection::Expired.reason(),
        HandoverReason::SwitchExpired
    );

    let offered = open_offer(&mut ledger, 4);
    assert_eq!(ledger.requester_left(CONTENDER), None);
    assert_eq!(
        ledger.requester_left(REQUESTER),
        Some(HandoverReason::SwitchCancelled)
    );
    assert!(ledger.is_idle());
    assert!(
        ledger
            .confirm(REQUESTER, &offered, later(1), &alice_detached())
            .is_err()
    );
}

#[test]
fn a_returning_client_makes_an_open_offer_stale() {
    let mut ledger = SwitchLedger::new();
    let offered = open_offer(&mut ledger, 1);
    assert_eq!(
        ledger.remote_client_returned(),
        ClientReturn::OfferWithdrawn
    );
    assert_eq!(
        ledger
            .confirm(REQUESTER, &offered, later(2), &alice_detached())
            .err(),
        Some(ReplyRejection::Replayed)
    );

    let offered = open_offer(&mut ledger, 2);
    let returned = occupant(
        alice_session(),
        RemoteAttachment::Attached(RemoteClient::Arcen),
        4,
        DeskState::Unlocked,
    );
    let rejection = ledger
        .confirm(REQUESTER, &offered, later(2), &returned)
        .err();
    assert_eq!(
        rejection,
        Some(ReplyRejection::Stale(StaleReason::RemoteAttached(
            RemoteClient::Arcen
        )))
    );
    assert_eq!(rejection.unwrap().reason(), HandoverReason::SwitchStale);
    assert!(ledger.is_idle());

    let offered = open_offer(&mut ledger, 3);
    let foreign = occupant(
        alice_session(),
        RemoteAttachment::Attached(RemoteClient::Foreign),
        3,
        DeskState::Locked,
    );
    assert_eq!(
        ledger
            .confirm(REQUESTER, &offered, later(2), &foreign)
            .err(),
        Some(ReplyRejection::Stale(StaleReason::RemoteAttached(
            RemoteClient::Foreign
        )))
    );
}

#[test]
fn a_client_that_came_and_went_since_the_offer_makes_it_stale() {
    let mut ledger = SwitchLedger::new();
    let offered = open_offer(&mut ledger, 1);
    let came_and_went = occupant(
        alice_session(),
        RemoteAttachment::ProvedDetached,
        5,
        DeskState::Locked,
    );
    assert_eq!(
        ledger
            .confirm(REQUESTER, &offered, later(2), &came_and_went)
            .err(),
        Some(ReplyRejection::Stale(StaleReason::EpochChanged))
    );
}

#[test]
fn unknown_facts_at_confirmation_reject() {
    for (current, stale) in [
        (ConsoleOccupant::Unknown, StaleReason::OccupantUnknown),
        (
            occupant(
                alice_session(),
                RemoteAttachment::Unknown,
                3,
                DeskState::Locked,
            ),
            StaleReason::AttachmentUnknown,
        ),
        (ConsoleOccupant::Nobody, StaleReason::TargetChanged),
    ] {
        let mut ledger = SwitchLedger::new();
        let offered = open_offer(&mut ledger, 1);
        assert_eq!(
            ledger
                .confirm(REQUESTER, &offered, later(2), &current)
                .err(),
            Some(ReplyRejection::Stale(stale))
        );
    }
}

#[test]
fn a_reused_session_id_in_another_boot_or_generation_is_another_session() {
    for reused in [
        identity(ALICE, "26", "boot-b", 41),
        identity(ALICE, "26", "boot-a", 42),
        identity(ALICE, "27", "boot-a", 41),
        identity(BOB, "26", "boot-a", 41),
    ] {
        let mut ledger = SwitchLedger::new();
        let offered = open_offer(&mut ledger, 1);
        let current = occupant(
            reused,
            RemoteAttachment::ProvedDetached,
            3,
            DeskState::Locked,
        );
        assert_eq!(
            ledger
                .confirm(REQUESTER, &offered, later(2), &current)
                .err(),
            Some(ReplyRejection::Stale(StaleReason::TargetChanged))
        );
    }
}

#[test]
fn two_contenders_get_at_most_one_transaction() {
    let mut ledger = SwitchLedger::new();
    let offered = open_offer(&mut ledger, 1);
    let carol = AccountPrincipal {
        name: "carol",
        id: 1002,
    };
    let HandoverDecision::OfferSwitch(contender) =
        decide(&signed_in(carol), &alice_detached(), SwitchRoute::Available)
    else {
        panic!("the facts alone offer to the contender too");
    };
    assert_eq!(
        ledger
            .open(contender.clone(), CONTENDER, nonce(9), T0, 60)
            .err(),
        Some(OpenRefusal::InProgress)
    );
    assert_eq!(
        OpenRefusal::InProgress.reason(),
        HandoverReason::SwitchInProgress
    );
    let refusal = OpenRefusal::InProgress.handover_refusal().unwrap();
    assert_eq!(refusal, HandoverRefusal::SwitchInProgress);
    assert_eq!(
        refusal.console_refusal(ConsoleOrigin::DisplayManager, true),
        ConsoleRefusal::SwitchInProgress,
        "a contender is told a change is in progress, not that someone is signed in"
    );
    assert_eq!(refusal.reason(), HandoverReason::SwitchInProgress);
    for internal in [OpenRefusal::NonceReused, OpenRefusal::Lifetime] {
        assert_eq!(internal.handover_refusal(), None);
    }
    assert_eq!(
        ledger
            .confirm(CONTENDER, &offered, later(1), &alice_detached())
            .err(),
        Some(ReplyRejection::WrongConnection)
    );
    assert!(ledger.cancel(CONTENDER, &offered, later(1)).is_err());
    let authority = ledger
        .confirm(REQUESTER, &offered, later(2), &alice_detached())
        .unwrap();
    assert_eq!(
        ledger.open(contender, CONTENDER, nonce(9), T0, 60).err(),
        Some(OpenRefusal::InProgress),
        "a contender waits while the logout is in flight"
    );
    assert_eq!(
        ledger.remote_client_returned(),
        ClientReturn::LogoutInFlight
    );
    drop(ledger.finish_logout(authority, LogoutOutcome::Vetoed, later(3)));
    assert!(ledger.is_idle());
}

#[test]
fn a_replayed_or_mismatched_nonce_is_rejected() {
    let mut ledger = SwitchLedger::new();
    let offered = open_offer(&mut ledger, 1);
    let authority = ledger
        .confirm(REQUESTER, &offered, later(1), &alice_detached())
        .unwrap();
    assert_eq!(
        ledger
            .confirm(REQUESTER, &offered, later(2), &alice_detached())
            .err(),
        Some(ReplyRejection::Replayed)
    );
    drop(ledger.finish_logout(authority, LogoutOutcome::Failed, later(2)));
    assert_eq!(
        ledger
            .confirm(REQUESTER, &offered, later(3), &alice_detached())
            .err(),
        Some(ReplyRejection::Replayed)
    );
    assert_eq!(
        ledger
            .open(candidate(&alice_detached()), REQUESTER, nonce(1), T0, 60)
            .err(),
        Some(OpenRefusal::NonceReused)
    );

    let _offered = open_offer(&mut ledger, 2);
    assert_eq!(
        ledger
            .confirm(REQUESTER, &nonce(3), later(1), &alice_detached())
            .err(),
        Some(ReplyRejection::Mismatched)
    );
    assert!(ledger.is_idle(), "a mismatched reply withdraws the offer");
}

#[test]
fn a_restarted_pier_knows_no_earlier_offer() {
    let mut before = SwitchLedger::new();
    let offered = open_offer(&mut before, 1);
    let mut after = SwitchLedger::new();
    assert_eq!(
        after
            .confirm(REQUESTER, &offered, later(1), &alice_detached())
            .err(),
        Some(ReplyRejection::NoOffer)
    );
}

#[test]
fn an_offer_lifetime_is_bounded() {
    let mut ledger = SwitchLedger::new();
    for lifetime in [0, MAX_OFFER_LIFETIME_SECS + 1] {
        assert_eq!(
            ledger
                .open(
                    candidate(&alice_detached()),
                    REQUESTER,
                    nonce(1),
                    T0,
                    lifetime
                )
                .err(),
            Some(OpenRefusal::Lifetime)
        );
    }
}

// The logout.

#[test]
fn only_a_proved_end_of_the_exact_session_lets_the_next_account_in() {
    let blocked = [
        (
            LogoutOutcome::Vetoed,
            LogoutBlock::Vetoed,
            HandoverReason::LogoutVetoed,
        ),
        (
            LogoutOutcome::TimedOut,
            LogoutBlock::TimedOut,
            HandoverReason::LogoutTimedOut,
        ),
        (
            LogoutOutcome::Failed,
            LogoutBlock::Failed,
            HandoverReason::LogoutFailed,
        ),
        (
            LogoutOutcome::Completed(
                SessionExitProof::read_back(
                    &identity(ALICE, "26", "boot-a", 40),
                    SessionPresence::Gone,
                )
                .unwrap(),
            ),
            LogoutBlock::ExitUnproven,
            HandoverReason::LogoutFailed,
        ),
    ];
    for (seed, (outcome, block, reason)) in (1..).zip(blocked) {
        let mut ledger = SwitchLedger::new();
        let offered = open_offer(&mut ledger, seed);
        let authority = ledger
            .confirm(REQUESTER, &offered, later(1), &alice_detached())
            .unwrap();
        let verdict = ledger.finish_logout(authority, outcome, later(2));
        assert_eq!(verdict.reason(), reason);
        assert!(matches!(verdict, LogoutVerdict::Blocked(found) if found == block));
        assert!(ledger.is_idle());
    }
    for presence in [SessionPresence::Present, SessionPresence::Unknown] {
        assert_eq!(
            SessionExitProof::read_back(&alice_session(), presence),
            None
        );
    }
}

#[test]
fn an_authority_from_another_ledger_lets_nobody_in() {
    let mut first = SwitchLedger::new();
    let offered = open_offer(&mut first, 1);
    let authority = first
        .confirm(REQUESTER, &offered, later(1), &alice_detached())
        .unwrap();
    let mut other = SwitchLedger::new();
    let proof = SessionExitProof::read_back(&alice_session(), SessionPresence::Gone).unwrap();
    assert!(matches!(
        other.finish_logout(authority, LogoutOutcome::Completed(proof), later(2)),
        LogoutVerdict::Blocked(LogoutBlock::NotCommitting)
    ));
    assert!(first.is_signing_out(&alice_session()));
}

// Displays.

#[test]
fn every_attachment_negotiates_the_decks_current_displays() {
    let one = [(1920_u32, 1080_u32)];
    let two = [(1920_u32, 1080_u32), (2560, 1440)];
    let apply = GuestLayoutPolicy::ApplyOnAttach;
    for kind in AttachmentKind::ALL {
        let plan = attachment_displays(kind, apply, &two[..], Some(&two[..]));
        assert!(
            matches!(plan, AttachmentDisplays::Negotiate { .. }) && plan.may_change_layout(),
            "{kind:?} keeps a stale layout"
        );
    }
    for kind in [
        AttachmentKind::Connect,
        AttachmentKind::AccountSwitch,
        AttachmentKind::LoginHandover,
    ] {
        assert_eq!(
            attachment_displays(kind, apply, &two[..], Some(&one[..])),
            AttachmentDisplays::Negotiate {
                authority: DisplayAuthority::Authentication,
                layout: apply,
            },
            "an earlier connection's layout never decides {kind:?}"
        );
    }
}

#[test]
fn a_desktop_another_product_uses_is_negotiated_but_never_laid_out() {
    let two = [(1920_u32, 1080_u32), (2560, 1440)];
    for attachment in [
        RemoteAttachment::Attached(RemoteClient::Foreign),
        RemoteAttachment::Unknown,
    ] {
        let facts = occupant(alice_session(), attachment, 4, DeskState::Unlocked);
        let layout = decide(&signed_in(ALICE), &facts, SwitchRoute::Available)
            .layout()
            .unwrap();
        for kind in AttachmentKind::ALL {
            let plan = attachment_displays(kind, layout, &two[..], Some(&two[..]));
            assert!(
                matches!(plan, AttachmentDisplays::Negotiate { layout: found, .. } if found == layout),
                "{kind:?} still negotiates and reports"
            );
            assert!(!plan.may_change_layout(), "{attachment:?} keeps its layout");
        }
    }
}

#[test]
fn a_resume_whose_layout_changed_authenticates_afresh_into_the_same_session() {
    let one = [(1920_u32, 1080_u32)];
    let four = [
        (1920_u32, 1080_u32),
        (1920, 1080),
        (2560, 1440),
        (1080, 1920),
    ];
    let apply = GuestLayoutPolicy::ApplyOnAttach;
    assert_eq!(
        attachment_displays(
            AttachmentKind::TransportResume,
            apply,
            &four[..],
            Some(&four[..])
        ),
        AttachmentDisplays::Negotiate {
            authority: DisplayAuthority::ResumeGrant,
            layout: apply,
        }
    );
    assert_eq!(
        attachment_displays(
            AttachmentKind::TransportResume,
            apply,
            &four[..],
            Some(&one[..])
        ),
        AttachmentDisplays::AuthenticateAfresh
    );
    assert_eq!(
        attachment_displays::<[(u32, u32)]>(AttachmentKind::TransportResume, apply, &one[..], None),
        AttachmentDisplays::AuthenticateAfresh
    );
    // A resume registry decides with the same rule before it knows the
    // desktop's layout policy.
    assert_eq!(
        resume_grant_authority(&four[..], Some(&four[..])),
        Some(DisplayAuthority::ResumeGrant)
    );
    assert_eq!(resume_grant_authority(&four[..], Some(&one[..])), None);
    assert_eq!(resume_grant_authority::<[(u32, u32)]>(&one[..], None), None);
    assert!(
        matches!(
            decide(&signed_in(ALICE), &alice_detached(), SwitchRoute::Available),
            HandoverDecision::JoinOwnSession { identity, .. } if identity == alice_session()
        ),
        "the fresh sign-in joins the same OS session"
    );
}

#[test]
fn a_held_desktop_takes_a_changed_request_without_ending_the_session() {
    let two = [(2560_u32, 1440_u32), (1920, 1080)];
    let one = [(1920_u32, 1080_u32)];
    for held in [
        HeldDesktop::JoinedConsole,
        HeldDesktop::OwnReconfigurable,
        HeldDesktop::OwnFixed,
    ] {
        assert_eq!(
            reattach_displays(held, &two[..], &two[..], true),
            ReattachDisplays::Reattach,
            "{held:?}"
        );
    }
    // An earlier connection's two displays never answer a one-display
    // request: the console is attached afresh in the same session, even
    // where no desktop may be created.
    for refused in [false, true] {
        assert_eq!(
            reattach_displays(HeldDesktop::JoinedConsole, &two[..], &one[..], refused),
            ReattachDisplays::FreshAttachment
        );
        assert_eq!(
            reattach_displays(HeldDesktop::OwnReconfigurable, &two[..], &one[..], refused),
            ReattachDisplays::ReconfigureInPlace
        );
    }
    assert_eq!(
        reattach_displays(HeldDesktop::OwnFixed, &two[..], &one[..], false),
        ReattachDisplays::ReplaceDesktop
    );
    assert_eq!(
        reattach_displays(HeldDesktop::OwnFixed, &two[..], &one[..], true),
        ReattachDisplays::KeepAndRefuse
    );
}

// Vocabulary.

#[test]
fn identity_parts_are_bounded_and_redacted() {
    assert_eq!(NativePrincipal::new("").err(), Some(IdentityError::Empty));
    assert_eq!(
        NativePrincipal::new("a".repeat(MAX_IDENTITY_PART_BYTES + 1)).err(),
        Some(IdentityError::TooLong)
    );
    assert_eq!(
        NativeSessionId::new("2\n6").err(),
        Some(IdentityError::Control)
    );
    assert_eq!(
        NativePrincipal::from_account(AccountPrincipal { name: "", id: 1 }).err(),
        Some(IdentityError::Empty)
    );
    assert_eq!(principal(ALICE).as_str(), "1000:alice");
    assert!(!format!("{:?}", alice_session()).contains("alice"));
    assert!(!format!("{:?}", nonce(0xab)).contains("abab"));
}

#[test]
fn a_nonce_round_trips_through_its_wire_form_only() {
    let mut bytes = [0_u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::try_from(index).unwrap() * 7;
    }
    bytes[0] = 0xfe;
    let original = SwitchNonce::from_random(bytes).unwrap();
    let text = original.to_hex();
    assert_eq!(text.len(), 64);
    let parsed = SwitchNonce::parse_hex(&text).unwrap();
    assert_eq!(parsed.to_hex(), text);
    assert!(SwitchNonce::parse_hex(&text.to_uppercase()).is_none());
    assert!(SwitchNonce::parse_hex(&text[..62]).is_none());
    assert!(SwitchNonce::parse_hex(&"0".repeat(64)).is_none());
    assert!(SwitchNonce::from_random([0; 32]).is_none());
}

#[test]
fn every_reason_builds_the_session_handover_record() {
    let mut tokens: Vec<_> = HandoverReason::ALL
        .iter()
        .map(|reason| reason.token())
        .collect();
    tokens.sort_unstable();
    tokens.dedup();
    assert_eq!(tokens.len(), HandoverReason::ALL.len());
    let validate = |fields: Option<arcen_telemetry::StructuredFields>| {
        arcen_telemetry::ValidatedLifecycleEvent::new(
            arcen_telemetry::LifecycleEventKind::SessionHandover,
            arcen_telemetry::CorrelationId::parse_uuid("01234567-89ab-4def-8123-456789abcdef")
                .unwrap(),
            fields.unwrap(),
        )
        .unwrap()
    };
    for reason in HandoverReason::ALL {
        for kind in AttachmentKind::ALL {
            validate(reason.fields(Some(kind), Some(RemoteClient::Foreign)));
        }
        let record = validate(reason.fields(None, None));
        assert_eq!(record.fields().as_map().len(), 1);
    }
}

// Every stage is bounded, and the ledger's reservation reaches decisions.

#[test]
fn a_contender_is_not_blocked_by_an_expired_but_unswept_offer() {
    let mut ledger = SwitchLedger::new();
    let _ = open_offer(&mut ledger, 1);
    let carol = AccountPrincipal {
        name: "carol",
        id: 1002,
    };
    let HandoverDecision::OfferSwitch(contender) =
        decide(&signed_in(carol), &alice_detached(), SwitchRoute::Available)
    else {
        panic!("expected an offer");
    };
    let opened = ledger
        .open(
            contender,
            CONTENDER,
            nonce(2),
            later(u128::from(DEFAULT_OFFER_LIFETIME_SECS)),
            60,
        )
        .unwrap();
    assert_eq!(opened.lapsed, Some(Lapse::OfferExpired));
    assert_eq!(
        opened.lapsed.unwrap().reason(),
        HandoverReason::SwitchExpired
    );
}

#[test]
fn a_lost_logout_authority_frees_the_desktop_at_its_deadline() {
    let mut ledger = SwitchLedger::new();
    let offered = open_offer(&mut ledger, 1);
    let confirmed_at = later(10);
    let authority = ledger
        .confirm(REQUESTER, &offered, confirmed_at, &alice_detached())
        .unwrap();
    drop(authority);
    let deadline = MonotonicMillis::new(confirmed_at.get() + LOGOUT_COMMIT_DEADLINE.as_millis());
    assert_eq!(
        ledger.expire(MonotonicMillis::new(deadline.get() - 1)),
        None
    );
    assert_eq!(
        ledger
            .open(
                candidate(&alice_detached()),
                CONTENDER,
                nonce(2),
                confirmed_at,
                60
            )
            .err(),
        Some(OpenRefusal::InProgress)
    );
    let lapse = ledger.expire(deadline);
    assert_eq!(lapse, Some(Lapse::LogoutOverdue));
    assert_eq!(lapse.unwrap().reason(), HandoverReason::LogoutTimedOut);
    assert!(ledger.is_idle());
    assert_eq!(
        ledger
            .confirm(REQUESTER, &offered, deadline, &alice_detached())
            .err(),
        Some(ReplyRejection::Replayed),
        "the abandoned nonce stays spent"
    );
    // The same through `open` without a sweep first.
    let mut ledger = SwitchLedger::new();
    let offered = open_offer(&mut ledger, 3);
    drop(
        ledger
            .confirm(REQUESTER, &offered, T0, &alice_detached())
            .unwrap(),
    );
    let at = MonotonicMillis::new(T0.get() + LOGOUT_COMMIT_DEADLINE.as_millis());
    let opened = ledger
        .open(candidate(&alice_detached()), CONTENDER, nonce(4), at, 60)
        .unwrap();
    assert_eq!(opened.lapsed, Some(Lapse::LogoutOverdue));
}

/// Bob's completed change of account at `later(20)`: Alice's session
/// ended, and the console is reserved for Bob's sign-in on `REQUESTER`.
fn completed_switch(ledger: &mut SwitchLedger) -> super::logout::EntryPermit {
    let offered = open_offer(ledger, 1);
    let authority = ledger
        .confirm(REQUESTER, &offered, later(10), &alice_detached())
        .unwrap();
    let proof = SessionExitProof::read_back(&alice_session(), SessionPresence::Gone).unwrap();
    match ledger.finish_logout(authority, LogoutOutcome::Completed(proof), later(20)) {
        LogoutVerdict::Completed { permit, .. } => permit,
        other => panic!("expected a completed logout, got {other:?}"),
    }
}

#[test]
fn after_the_logout_only_the_permit_holder_may_sign_in() {
    let carol = AccountPrincipal {
        name: "carol",
        id: 1002,
    };
    let mut ledger = SwitchLedger::new();
    let permit = completed_switch(&mut ledger);
    let now = later(30);
    for (connection, account) in [(CONTENDER, carol), (CONTENDER, ALICE), (CONTENDER, BOB)] {
        let decided = ledger.decide(
            connection,
            &signed_in(account),
            &ConsoleOccupant::Nobody,
            SwitchRoute::Available,
            now,
        );
        assert_eq!(
            decided.decision,
            HandoverDecision::Refuse(HandoverRefusal::SwitchInProgress),
            "{account:?} on another connection"
        );
    }
    assert_eq!(ledger.remote_client_returned(), ClientReturn::EntryPending);
    assert_eq!(
        ledger
            .decide(
                REQUESTER,
                &signed_in(BOB),
                &ConsoleOccupant::Nobody,
                SwitchRoute::Available,
                now
            )
            .decision,
        HandoverDecision::SignInFresh,
        "the permit holder signs in"
    );
    assert_eq!(
        ledger
            .open(candidate(&alice_detached()), CONTENDER, nonce(9), now, 60)
            .err(),
        Some(OpenRefusal::InProgress)
    );
    assert_eq!(ledger.login_finished(permit, true), None);
    assert!(ledger.is_idle());
}

#[test]
fn a_failed_late_or_abandoned_sign_in_releases_the_console() {
    let mut ledger = SwitchLedger::new();
    let permit = completed_switch(&mut ledger);
    assert_eq!(
        ledger.login_finished(permit, false),
        Some(HandoverReason::NewLoginFailed)
    );
    assert!(ledger.is_idle());

    let mut ledger = SwitchLedger::new();
    let _permit = completed_switch(&mut ledger);
    let deadline = MonotonicMillis::new(later(20).get() + ENTRY_DEADLINE.as_millis());
    assert_eq!(
        ledger.expire(MonotonicMillis::new(deadline.get() - 1)),
        None
    );
    let lapse = ledger.expire(deadline);
    assert_eq!(lapse, Some(Lapse::EntryOverdue));
    assert_eq!(lapse.unwrap().reason(), HandoverReason::NewLoginFailed);
    assert!(ledger.is_idle());

    let mut ledger = SwitchLedger::new();
    let _permit = completed_switch(&mut ledger);
    assert_eq!(ledger.requester_left(CONTENDER), None);
    assert_eq!(
        ledger.requester_left(REQUESTER),
        Some(HandoverReason::NewLoginFailed)
    );
    assert!(ledger.is_idle());
}

#[test]
fn the_owner_returning_during_its_logout_is_refused_not_attached() {
    let mut ledger = SwitchLedger::new();
    let offered = open_offer(&mut ledger, 1);
    let _authority = ledger
        .confirm(REQUESTER, &offered, later(5), &alice_detached())
        .unwrap();
    assert_eq!(
        ledger.remote_client_returned(),
        ClientReturn::LogoutInFlight
    );
    let decided = ledger.decide(
        ConnectionId::new(9),
        &signed_in(ALICE),
        &alice_detached(),
        SwitchRoute::Available,
        later(6),
    );
    assert_eq!(
        decided.decision,
        HandoverDecision::Refuse(HandoverRefusal::SigningOut)
    );
    assert_eq!(decided.decision.layout(), None, "nothing is joined");
    assert_eq!(
        HandoverRefusal::SigningOut.console_refusal(ConsoleOrigin::DisplayManager, true),
        ConsoleRefusal::SigningOut
    );
    let carol = AccountPrincipal {
        name: "carol",
        id: 1002,
    };
    assert_eq!(
        ledger
            .decide(
                CONTENDER,
                &signed_in(carol),
                &alice_detached(),
                SwitchRoute::Available,
                later(6)
            )
            .decision,
        HandoverDecision::Refuse(HandoverRefusal::SwitchInProgress)
    );
}

#[test]
fn a_deck_without_the_capability_never_sees_a_new_code() {
    let origins = [
        ConsoleOrigin::DisplayManager,
        ConsoleOrigin::Arcen,
        ConsoleOrigin::OtherProduct,
    ];
    let attachments = [
        RemoteAttachment::ProvedDetached,
        RemoteAttachment::Attached(RemoteClient::Arcen),
        RemoteAttachment::Attached(RemoteClient::Foreign),
        RemoteAttachment::Unknown,
    ];
    let target = alice_session();
    let entering = signed_in(BOB);
    let holds = [
        ConsoleHold::Free,
        ConsoleHold::Offered(&target),
        ConsoleHold::SigningOut(&target),
        ConsoleHold::Entering(&entering),
    ];
    let mut occupants = vec![ConsoleOccupant::Nobody, ConsoleOccupant::Unknown];
    occupants.extend(
        attachments
            .iter()
            .map(|attachment| occupant(alice_session(), *attachment, 3, DeskState::Unlocked)),
    );
    let mut refusals = Vec::new();
    for account in [ALICE, BOB] {
        for occupant in &occupants {
            for hold in holds {
                for pier in [true, false] {
                    let route = SwitchRoute::from_capabilities(false, pier);
                    if let HandoverDecision::Refuse(refusal) =
                        decide_handover(&signed_in(account), occupant, route, hold)
                    {
                        refusals.push(refusal);
                    }
                }
            }
        }
    }
    refusals.push(OpenRefusal::InProgress.handover_refusal().unwrap());
    for refusal in &refusals {
        for origin in origins {
            let code = refusal.console_refusal(origin, false);
            assert!(
                !code.since_account_switch_v1(),
                "{refusal:?} reached an older Deck as {code:?}"
            );
            if matches!(
                refusal,
                HandoverRefusal::AttachmentUnknown | HandoverRefusal::SwitchInProgress
            ) {
                assert_eq!(code, ConsoleRefusal::other_user(origin));
            }
        }
    }
    for refusal in [
        HandoverRefusal::AttachmentUnknown,
        HandoverRefusal::SwitchInProgress,
        HandoverRefusal::SigningOut,
    ] {
        assert!(refusals.contains(&refusal), "{refusal:?} was not reached");
        assert!(
            refusal
                .console_refusal(ConsoleOrigin::DisplayManager, true)
                .since_account_switch_v1()
        );
        assert!(
            !refusal
                .console_refusal(ConsoleOrigin::DisplayManager, false)
                .since_account_switch_v1()
        );
    }
    assert_eq!(
        HandoverRefusal::SigningOut.console_refusal(ConsoleOrigin::Arcen, false),
        ConsoleRefusal::SessionChanged
    );
}

#[test]
fn a_requester_that_left_during_its_logout_reserves_nothing() {
    let carol = AccountPrincipal {
        name: "carol",
        id: 1002,
    };
    let mut ledger = SwitchLedger::new();
    let offered = open_offer(&mut ledger, 1);
    let authority = ledger
        .confirm(REQUESTER, &offered, later(10), &alice_detached())
        .unwrap();
    assert_eq!(ledger.requester_left(CONTENDER), None);
    assert_eq!(
        ledger.requester_left(REQUESTER),
        None,
        "the logout still runs"
    );
    assert!(ledger.is_signing_out(&alice_session()));
    let proof = SessionExitProof::read_back(&alice_session(), SessionPresence::Gone).unwrap();
    let verdict = ledger.finish_logout(authority, LogoutOutcome::Completed(proof), later(20));
    assert_eq!(verdict.reason(), HandoverReason::NewLoginFailed);
    assert!(matches!(
        verdict,
        LogoutVerdict::CompletedRequesterGone { ended } if ended == alice_session()
    ));
    assert!(
        ledger.is_idle(),
        "nothing is reserved for a closed connection"
    );
    assert_eq!(
        ledger
            .decide(
                CONTENDER,
                &signed_in(carol),
                &ConsoleOccupant::Nobody,
                SwitchRoute::Available,
                later(21)
            )
            .decision,
        HandoverDecision::SignInFresh
    );

    // A requester that stays still gets its reservation.
    let mut ledger = SwitchLedger::new();
    let _permit = completed_switch(&mut ledger);
    assert!(!ledger.is_idle());
}

#[test]
fn an_account_whose_own_session_holds_the_desktop_joins_it_during_a_reserved_sign_in() {
    let carol = AccountPrincipal {
        name: "carol",
        id: 1002,
    };
    let dave = AccountPrincipal {
        name: "dave",
        id: 1003,
    };
    let mut ledger = SwitchLedger::new();
    let _permit = completed_switch(&mut ledger);
    let now = later(30);
    let bob_new = identity(BOB, "31", "boot-a", 77);
    let carol_desk = identity(carol, "32", "boot-a", 78);
    for (account, session) in [(BOB, &bob_new), (carol, &carol_desk)] {
        let facts = occupant(
            session.clone(),
            RemoteAttachment::ProvedDetached,
            1,
            DeskState::Unlocked,
        );
        assert!(
            matches!(
                ledger
                    .decide(CONTENDER, &signed_in(account), &facts, SwitchRoute::Available, now)
                    .decision,
                HandoverDecision::JoinOwnSession { identity, .. } if identity == *session
            ),
            "{account:?} joins its own session"
        );
    }
    let carol_holds = occupant(
        carol_desk,
        RemoteAttachment::ProvedDetached,
        1,
        DeskState::Unlocked,
    );
    for current in [
        carol_holds,
        ConsoleOccupant::Nobody,
        ConsoleOccupant::Unknown,
    ] {
        assert_eq!(
            ledger
                .decide(
                    CONTENDER,
                    &signed_in(dave),
                    &current,
                    SwitchRoute::Available,
                    now
                )
                .decision,
            HandoverDecision::Refuse(HandoverRefusal::SwitchInProgress),
            "{current:?}"
        );
    }
}

#[test]
fn during_a_logout_only_the_target_session_is_off_limits() {
    let carol = AccountPrincipal {
        name: "carol",
        id: 1002,
    };
    let mut ledger = SwitchLedger::new();
    let offered = open_offer(&mut ledger, 1);
    let _authority = ledger
        .confirm(REQUESTER, &offered, later(5), &alice_detached())
        .unwrap();
    let now = later(6);
    let carol_desk = identity(carol, "32", "boot-a", 78);
    let alice_other = identity(ALICE, "33", "boot-a", 79);
    // The target already gone; another account signed in at the desk.
    let carol_holds = occupant(
        carol_desk.clone(),
        RemoteAttachment::ProvedDetached,
        1,
        DeskState::Unlocked,
    );
    assert!(matches!(
        ledger
            .decide(CONTENDER, &signed_in(carol), &carol_holds, SwitchRoute::Available, now)
            .decision,
        HandoverDecision::JoinOwnSession { identity, .. } if identity == carol_desk
    ));
    assert_eq!(
        ledger
            .decide(
                CONTENDER,
                &signed_in(ALICE),
                &carol_holds,
                SwitchRoute::Available,
                now
            )
            .decision,
        HandoverDecision::Refuse(HandoverRefusal::SigningOut)
    );
    assert_eq!(
        ledger
            .decide(
                CONTENDER,
                &signed_in(carol),
                &ConsoleOccupant::Nobody,
                SwitchRoute::Available,
                now
            )
            .decision,
        HandoverDecision::Refuse(HandoverRefusal::SwitchInProgress)
    );
    // The target account's other session is not the one being signed out.
    let alice_holds_other = occupant(
        alice_other.clone(),
        RemoteAttachment::ProvedDetached,
        1,
        DeskState::Locked,
    );
    assert!(matches!(
        ledger
            .decide(CONTENDER, &signed_in(ALICE), &alice_holds_other, SwitchRoute::Available, now)
            .decision,
        HandoverDecision::JoinOwnSession { identity, .. } if identity == alice_other
    ));
    // The target itself still holds the desktop.
    assert_eq!(
        ledger
            .decide(
                CONTENDER,
                &signed_in(ALICE),
                &alice_detached(),
                SwitchRoute::Available,
                now
            )
            .decision,
        HandoverDecision::Refuse(HandoverRefusal::SigningOut)
    );
}
