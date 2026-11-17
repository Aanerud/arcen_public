// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! No authenticated setup ends without a record.
//!
//! A sign-in that passed (`SESSION_AUTH_OK`) and then ended before its
//! stream started used to leave nothing at the `critical` log profile: its
//! reasons were `warn!` lines, which that profile drops (field, 0.20.1). Each
//! connection runs inside a [`scope`]; the lifecycle emitters mark it
//! settled ([`settled`]) when they record a stream start, an end or a
//! refusal, and every close the connection sends is remembered
//! ([`note_close`]). A [`SetupWatch`], armed as soon as an authenticated
//! sign-in has its desktop, records,
//! when it is dropped on any exit (a `return`, an error, a panic), a
//! `SESSION_INTERRUPTED` with stage `setup` and the last close the Deck was
//! sent as its reason class, plus an error line, unless the connection was
//! settled first.

use std::cell::Cell;

use arcen_telemetry::{CorrelationId, FieldValue, LifecycleEventKind, StructuredFields};
use tracing::error;

use crate::logging::target::SESSION;
use crate::LifecycleEmitter;

/// Stage of the record a setup that ended silently leaves.
pub(crate) const SETUP_STAGE: &str = "setup";
/// Reason class when the Deck was sent no close at all.
pub(crate) const NO_CLOSE_SENT: &str = "setup_ended_without_reason";

#[derive(Default)]
pub(crate) struct SetupTrace {
    settled: Cell<bool>,
    last_close: Cell<Option<&'static str>>,
}

tokio::task_local! {
    static TRACE: SetupTrace;
}

/// Runs one connection's work with its setup trace.
pub(crate) async fn scope<F: std::future::Future>(work: F) -> F::Output {
    TRACE.scope(SetupTrace::default(), work).await
}

/// The connection recorded a stream start, an end or a refusal.
pub(crate) fn settled() {
    let _ = TRACE.try_with(|trace| trace.settled.set(true));
}

/// The connection sent the Deck a critical control (`kind` names it).
pub(crate) fn note_close(kind: &'static str) {
    let _ = TRACE.try_with(|trace| trace.last_close.set(Some(kind)));
}

/// What the trace says now: `None` once settled, else the reason class.
fn unsettled_reason() -> Option<&'static str> {
    TRACE
        .try_with(|trace| {
            (!trace.settled.get()).then(|| trace.last_close.get().unwrap_or(NO_CLOSE_SENT))
        })
        .ok()
        .flatten()
}

/// Armed once an authenticated sign-in has its desktop; see the module
/// documentation.
pub(crate) struct SetupWatch {
    emitter: LifecycleEmitter,
    session_log_id: CorrelationId,
    user: String,
    peer: String,
}

impl SetupWatch {
    pub(crate) fn arm(
        emitter: &LifecycleEmitter,
        session_log_id: CorrelationId,
        user: &str,
        peer: &str,
    ) -> Self {
        Self {
            emitter: emitter.clone(),
            session_log_id,
            user: user.to_owned(),
            peer: peer.to_owned(),
        }
    }
}

impl Drop for SetupWatch {
    fn drop(&mut self) {
        let Some(reason_class) = unsettled_reason() else {
            return;
        };
        error!(
            target: SESSION,
            username = %self.user,
            sid = %self.session_log_id,
            reason_class,
            "an authenticated sign-in ended during setup, before its stream started"
        );
        let mut fields = StructuredFields::default();
        let _ = fields.insert("stage", FieldValue::String(SETUP_STAGE.to_owned()));
        let _ = fields.insert("reason_class", FieldValue::String(reason_class.to_owned()));
        let context = self.emitter.session_context(
            self.session_log_id.clone(),
            Some(self.user.clone()),
            Some(self.peer.clone()),
            None,
        );
        crate::emit_lifecycle_event_with_context(
            &self.emitter,
            LifecycleEventKind::SessionInterrupted,
            context,
            fields,
        );
        settled();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_setup_is_unsettled_until_an_event_records_it() {
        scope(async {
            assert_eq!(unsettled_reason(), Some(NO_CLOSE_SENT));
            note_close("display_resize_close");
            assert_eq!(unsettled_reason(), Some("display_resize_close"));
            settled();
            assert_eq!(unsettled_reason(), None);
        })
        .await;
        // Outside a connection's scope nothing is recorded.
        assert_eq!(unsettled_reason(), None);
    }
}
