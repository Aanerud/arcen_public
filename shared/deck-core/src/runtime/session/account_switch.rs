// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The session engine's side of a confirmed change of account
//! (`account_switch_v1`): the offer that arrives in place of `auth_result`,
//! the user's one answer, and the Pier's result. The rules and the words
//! are [`crate::account_switch`]; this module only moves messages.

use std::time::Duration;

use arcen_protocol::messages::{
    ACCOUNT_SWITCH_OFFER, ACCOUNT_SWITCH_RESULT, AccountSwitchChoiceMsg, AccountSwitchOfferMsg,
    AccountSwitchResultMsg, msg_type,
};
use arcen_session::session_handover::consent::{ENTRY_DEADLINE, LOGOUT_COMMIT_DEADLINE};
use futures_util::{Sink, SinkExt, Stream};
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::{Error as WebSocketError, Message};

use super::{
    ConnectSmokeError, SessionCancellation, SessionCommand, SessionEvent, bounded_message,
    cancellable_setup, close_bounded, recv_json, send_setup_message,
};
use crate::account_switch::{
    AccountSwitchExchange, AccountSwitchMode, AccountSwitchNews, AccountSwitchProtocolError,
    AccountSwitchStep, AccountSwitchStop,
};

/// Whether this answer to the Deck's `auth_response` is an offer to sign
/// another account out.
pub(super) fn is_offer(value: &Value) -> bool {
    msg_type(value) == Some(ACCOUNT_SWITCH_OFFER)
}

/// Asks the user about the Pier's offer and follows it to its end. `Ok`
/// only after the Pier reported the other account signed out: the Pier's
/// next message, its `auth_result` for this Deck's account. Every other end
/// is an error, the connection closed.
///
/// Closing the session (the window, Disconnect) and the offer's expiry
/// answer Cancel; the Pier also cancels when the Deck leaves without one.
pub(super) async fn follow_offer<S>(
    ws: &mut S,
    offer: Value,
    mode: AccountSwitchMode,
    tx: &mpsc::UnboundedSender<SessionEvent>,
    commands: &mut mpsc::UnboundedReceiver<SessionCommand>,
    cancellation: &SessionCancellation,
    close_receiver: &mut watch::Receiver<bool>,
    wait: Duration,
) -> Result<Value, ConnectSmokeError>
where
    S: Sink<Message> + Stream<Item = Result<Message, WebSocketError>> + Unpin,
    S::Error: Into<WebSocketError>,
{
    let offer: AccountSwitchOfferMsg = serde_json::from_value(offer)?;
    let mut exchange = AccountSwitchExchange::offered(offer, mode.advertises())
        .map_err(ConnectSmokeError::AccountSwitchProtocol)?;
    tracing::info!(
        target: crate::log_target::SESSION,
        event = "account_switch_offered",
        expires_in_secs = exchange.expires_in().as_secs(),
        "another account holds the host; asking the user whether to sign it out",
    );
    let _ = tx.send(SessionEvent::AccountSwitch(AccountSwitchNews::Offered {
        expires_in: exchange.expires_in(),
    }));
    let deadline = tokio::time::Instant::now() + exchange.expires_in();

    let choice = loop {
        if let AccountSwitchMode::Scripted(choice) = mode {
            // A test tool's answer, given on its command line.
            break choice;
        }
        tokio::select! {
            biased;
            () = cancellation.cancelled(close_receiver) => {
                let close_by = cancellation.close_deadline();
                cancel_and_close(ws, &mut exchange, close_by).await;
                return Err(ConnectSmokeError::SessionClosed);
            }
            () = tokio::time::sleep_until(deadline) => {
                let close_by = cancellation.cleanup_deadline();
                cancel_and_close(ws, &mut exchange, close_by).await;
                return Err(stopped(AccountSwitchStop::Expired, String::new()));
            }
            message = recv_json(ws, deadline.saturating_duration_since(tokio::time::Instant::now()) + wait) => {
                // The Pier ended the offer first (it expired, a client
                // returned) or closed the connection.
                let stop = stop_of(&exchange, message?)?;
                close_bounded(ws, cancellation.cleanup_deadline()).await;
                return Err(stopped(stop, String::new()));
            }
            command = commands.recv() => match command {
                Some(SessionCommand::AccountSwitch(choice)) => break choice,
                Some(
                    SessionCommand::Json(_)
                    | SessionCommand::Binary(_)
                    | SessionCommand::HardUsbPen(_),
                ) => {}
                Some(SessionCommand::Close) | None => {
                    let close_by = cancellation.close_deadline();
                    cancel_and_close(ws, &mut exchange, close_by).await;
                    return Err(ConnectSmokeError::SessionClosed);
                }
                // Anything else is not a confirmation.
                Some(
                    SessionCommand::SubmitAuth(_)
                    | SessionCommand::AcceptAuthentication
                    | SessionCommand::DeclineDisclaimer,
                ) => break AccountSwitchChoiceMsg::Cancel,
            },
        }
    };

    let reply = exchange
        .reply(choice)
        .map_err(ConnectSmokeError::AccountSwitchProtocol)?;
    tracing::info!(
        target: crate::log_target::SESSION,
        event = "account_switch_answered",
        choice = ?choice,
        "answered the host's offer to sign another account out",
    );
    send_setup_message(
        ws,
        Message::Text(serde_json::to_string(&reply)?),
        Some(cancellation),
        Some(&mut *close_receiver),
    )
    .await?;

    if choice == AccountSwitchChoiceMsg::Cancel {
        // The Pier confirms with `cancelled` and closes; nothing changed,
        // whatever it says.
        let _ = cancellable_setup(recv_json(ws, wait), cancellation, close_receiver).await;
        close_bounded(ws, cancellation.cleanup_deadline()).await;
        return Err(stopped(AccountSwitchStop::Cancelled, String::new()));
    }

    let _ = tx.send(SessionEvent::AccountSwitch(AccountSwitchNews::SigningOut));
    let result = cancellable_setup(
        recv_json(ws, LOGOUT_COMMIT_DEADLINE + wait),
        cancellation,
        close_receiver,
    )
    .await
    .map_err(|error| switch_lost(error, AccountSwitchStop::Unknown))?;
    match step_of(&exchange, result)? {
        AccountSwitchStep::Stop(stop) => {
            close_bounded(ws, cancellation.cleanup_deadline()).await;
            Err(stopped(stop, String::new()))
        }
        AccountSwitchStep::SignIn => {
            tracing::info!(
                target: crate::log_target::SESSION,
                event = "account_switch_signed_out",
                "the host signed the other account out; signing in",
            );
            let _ = tx.send(SessionEvent::AccountSwitch(AccountSwitchNews::SignedOut));
            cancellable_setup(
                recv_json(ws, ENTRY_DEADLINE + wait),
                cancellation,
                close_receiver,
            )
            .await
            .map_err(|error| switch_lost(error, AccountSwitchStop::NewLoginFailed))
        }
    }
}

/// The sign-in after a completed sign-out was refused: the host's words go
/// under Details.
pub(super) fn new_login_failed(message: &str) -> ConnectSmokeError {
    stopped(AccountSwitchStop::NewLoginFailed, bounded_message(message))
}

fn stopped(stop: AccountSwitchStop, detail: String) -> ConnectSmokeError {
    tracing::info!(
        target: crate::log_target::SESSION,
        event = "account_switch_stopped",
        reason_class = stop.reason_class(),
        "the change of account stopped",
    );
    ConnectSmokeError::AccountSwitch { stop, detail }
}

/// A connection that failed while the Pier acted on a confirmation ends
/// the change of account, never as a plain network failure a Deck would
/// retry into a second question. Closing the session stays a close.
fn switch_lost(error: ConnectSmokeError, stop: AccountSwitchStop) -> ConnectSmokeError {
    match error {
        ConnectSmokeError::SessionClosed => error,
        error => stopped(stop, bounded_message(&error.to_string())),
    }
}

fn step_of(
    exchange: &AccountSwitchExchange,
    value: Value,
) -> Result<AccountSwitchStep, ConnectSmokeError> {
    if msg_type(&value) != Some(ACCOUNT_SWITCH_RESULT) {
        return Err(ConnectSmokeError::UnexpectedFirstMessage(
            msg_type(&value).unwrap_or("<missing>").to_owned(),
        ));
    }
    let result: AccountSwitchResultMsg = serde_json::from_value(value)?;
    exchange
        .result(&result)
        .map_err(ConnectSmokeError::AccountSwitchProtocol)
}

fn stop_of(
    exchange: &AccountSwitchExchange,
    value: Value,
) -> Result<AccountSwitchStop, ConnectSmokeError> {
    match step_of(exchange, value)? {
        AccountSwitchStep::Stop(stop) => Ok(stop),
        AccountSwitchStep::SignIn => Err(ConnectSmokeError::AccountSwitchProtocol(
            AccountSwitchProtocolError::SignedOutWithoutConfirmation,
        )),
    }
}

/// Answers Cancel if the offer is still unanswered, then closes, both by
/// `deadline`. Best effort: a Pier that does not hear it cancels when the
/// connection goes.
async fn cancel_and_close<S>(
    ws: &mut S,
    exchange: &mut AccountSwitchExchange,
    deadline: tokio::time::Instant,
) where
    S: Sink<Message> + Unpin,
    S::Error: Into<WebSocketError>,
{
    if let Ok(reply) = exchange.reply(AccountSwitchChoiceMsg::Cancel)
        && let Ok(text) = serde_json::to_string(&reply)
    {
        let _ = tokio::time::timeout_at(deadline, ws.send(Message::Text(text))).await;
    }
    close_bounded(ws, deadline).await;
}
