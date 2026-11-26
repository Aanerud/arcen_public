// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The session agent's one `SESSION_END`, including when Windows ends the
//! session under a live Deck: its user signs out, or Windows restarts or
//! shuts down.
//!
//! Windows ends the agent's process as it signs the session out. A process
//! that loads user32 hears of that only through a top-level window's
//! `WM_QUERYENDSESSION`/`WM_ENDSESSION` (its console control handler is not
//! called), so the agent keeps a hidden window for that alone. On
//! `WM_ENDSESSION` it writes the session's end at once, with the frames
//! counted so far, and flushes the log before returning, after which
//! Windows ends the process. Whichever of that and the ordinary end comes
//! first writes the record (the shared `SessionTally`).

use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Instant;

use arcen_session::host_shutdown::{SessionEndedBy, SessionTally, SessionTotals};
use arcen_telemetry::{CorrelationId, FieldValue, LifecycleEventKind, StructuredFields};

use crate::logging::{LogController, SESSION};
use crate::LifecycleEmitter;

struct LiveSession {
    emitter: LifecycleEmitter,
    log_controller: LogController,
    user: String,
    peer: String,
    started_at: Instant,
    attachment: Mutex<CorrelationId>,
    tally: Mutex<SessionTally>,
}

static LIVE: OnceLock<LiveSession> = OnceLock::new();

/// The agent's session began streaming: from now on its end is recorded
/// even if Windows ends the session.
pub(crate) fn begin(
    emitter: LifecycleEmitter,
    log_controller: LogController,
    session_log_id: CorrelationId,
    user: &str,
    peer: &str,
    started_at: Instant,
) {
    let live = LiveSession {
        emitter,
        log_controller,
        user: user.to_owned(),
        peer: peer.to_owned(),
        started_at,
        attachment: Mutex::new(session_log_id),
        tally: Mutex::new(SessionTally::default()),
    };
    if LIVE.set(live).is_ok() {
        watch_for_session_end();
    }
}

/// A resumed attachment's log id, which the end record carries.
pub(crate) fn set_attachment(session_log_id: CorrelationId) {
    if let Some(live) = LIVE.get() {
        *live
            .attachment
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = session_log_id;
    }
}

/// What the current attachment has sent and dropped so far.
pub(crate) fn observe_attachment(frames_sent: u64, frames_dropped: u64) {
    if let Some(live) = LIVE.get() {
        tally(live).observe_attachment(frames_sent, frames_dropped);
    }
}

/// The current attachment ended having sent and dropped these.
pub(crate) fn close_attachment(frames_sent: u64, frames_dropped: u64) {
    if let Some(live) = LIVE.get() {
        tally(live).close_attachment(frames_sent, frames_dropped);
    }
}

/// The totals for the ordinary end record, or `None` when the end was
/// already recorded (or no session began).
pub(crate) fn end() -> Option<SessionTotals> {
    LIVE.get().and_then(|live| tally(live).end())
}

fn tally(live: &LiveSession) -> std::sync::MutexGuard<'_, SessionTally> {
    live.tally.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Windows is ending the session: write its end now.
fn record_ended_by(by: SessionEndedBy) {
    let Some(live) = LIVE.get() else {
        return;
    };
    let Some(totals) = tally(live).end() else {
        return;
    };
    let duration_ms = i64::try_from(live.started_at.elapsed().as_millis()).unwrap_or(i64::MAX);
    let attachment = live
        .attachment
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    tracing::info!(
        target: SESSION,
        reason_class = by.reason_class(),
        frames_sent = totals.frames_sent,
        "Windows is ending the session; its end is recorded now"
    );
    crate::emit_lifecycle_event_with_context(
        &live.emitter,
        LifecycleEventKind::SessionEnd,
        attachment,
        session_end_fields(by, duration_ms, totals),
        Some(live.user.clone()),
        crate::session::local_hostname(),
        Some(live.peer.clone()),
        None,
    );
    if let Err(error) = live.log_controller.flush_log() {
        eprintln!("session end flush failed: {error}");
    }
}

fn session_end_fields(
    by: SessionEndedBy,
    duration_ms: i64,
    totals: SessionTotals,
) -> StructuredFields {
    let mut fields = StructuredFields::default();
    let _ = fields.insert(
        "reason_class",
        FieldValue::String(by.reason_class().to_string()),
    );
    let _ = fields.insert("duration_ms", FieldValue::Integer(duration_ms));
    let _ = fields.insert(
        "frames_sent",
        FieldValue::Integer(i64::try_from(totals.frames_sent).unwrap_or(i64::MAX)),
    );
    let _ = fields.insert(
        "frames_dropped",
        FieldValue::Integer(i64::try_from(totals.frames_dropped).unwrap_or(i64::MAX)),
    );
    fields
}

/// How Windows is ending the session, from `WM_ENDSESSION`'s `lParam`;
/// `None` when it is not: Restart Manager closing the agent's process so an
/// installer can replace a file it holds (`ENDSESSION_CLOSEAPP`).
const fn ended_by(lparam: u32) -> Option<SessionEndedBy> {
    const ENDSESSION_CLOSEAPP: u32 = 0x0000_0001;
    const ENDSESSION_LOGOFF: u32 = 0x8000_0000;
    if lparam & ENDSESSION_CLOSEAPP != 0 {
        None
    } else if lparam & ENDSESSION_LOGOFF != 0 {
        Some(SessionEndedBy::SignOut)
    } else {
        Some(SessionEndedBy::HostShutdown)
    }
}

#[cfg(windows)]
fn watch_for_session_end() {
    let spawned = std::thread::Builder::new()
        .name("session-end-watch".to_string())
        .spawn(|| {
            if let Err(error) = run_watch_window() {
                tracing::warn!(
                    target: SESSION,
                    %error,
                    "the session's end cannot be watched; a sign-out or restart leaves no end record"
                );
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(target: SESSION, %error, "the session-end watch did not start");
    }
}

#[cfg(not(windows))]
fn watch_for_session_end() {}

#[cfg(windows)]
fn run_watch_window() -> Result<(), String> {
    use windows::core::w;
    use windows::Win32::Foundation::HINSTANCE;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DispatchMessageW, GetMessageW, RegisterClassW, TranslateMessage, MSG,
        WINDOW_EX_STYLE, WNDCLASSW, WS_OVERLAPPED,
    };

    // SAFETY: None requests the current process module.
    let module =
        unsafe { GetModuleHandleW(None) }.map_err(|error| format!("module lookup: {error}"))?;
    let instance = HINSTANCE(module.0);
    let class = WNDCLASSW {
        lpfnWndProc: Some(watch_window_proc),
        hInstance: instance,
        lpszClassName: w!("ArcenSessionEndWatch"),
        ..Default::default()
    };
    // SAFETY: class points to static names and a valid callback.
    if unsafe { RegisterClassW(&raw const class) } == 0 {
        return Err(format!(
            "RegisterClassW: {}",
            std::io::Error::last_os_error()
        ));
    }
    // A hidden top-level window: a message-only window is not told the
    // session is ending.
    // SAFETY: the class and instance are valid; no application pointer is
    // passed.
    let _window = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("ArcenSessionEndWatch"),
            w!("Arcen session end"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            instance,
            None,
        )
    }
    .map_err(|error| format!("CreateWindowExW: {error}"))?;
    let mut message = MSG::default();
    // SAFETY: message is a live MSG owned by this thread, which owns the
    // window whose messages it dispatches.
    unsafe {
        while GetMessageW(&raw mut message, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
    }
    Ok(())
}

#[cfg(windows)]
unsafe extern "system" fn watch_window_proc(
    hwnd: windows::Win32::Foundation::HWND,
    message: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::Foundation::LRESULT;
    use windows::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, WM_ENDSESSION, WM_QUERYENDSESSION,
    };

    match message {
        // Never stand in the way of a sign-out or restart.
        WM_QUERYENDSESSION => LRESULT(1),
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let flags = lparam.0 as u32;
                if let Some(by) = ended_by(flags) {
                    let _ = std::panic::catch_unwind(|| record_ended_by(by));
                }
            }
            LRESULT(0)
        }
        // SAFETY: forwarded unchanged from the system's call.
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sign_out_and_a_restart_are_told_apart() {
        assert_eq!(ended_by(0x8000_0000), Some(SessionEndedBy::SignOut));
        assert_eq!(ended_by(0), Some(SessionEndedBy::HostShutdown));
        // ENDSESSION_CRITICAL alone: a forced shutdown.
        assert_eq!(ended_by(0x4000_0000), Some(SessionEndedBy::HostShutdown));
    }

    #[test]
    fn an_installer_closing_the_agent_does_not_end_the_session() {
        assert_eq!(ended_by(0x0000_0001), None);
        assert_eq!(ended_by(0x8000_0001), None);
    }

    #[test]
    fn the_end_record_has_the_session_end_schema() {
        let fields = session_end_fields(
            SessionEndedBy::HostShutdown,
            1_200,
            SessionTotals {
                frames_sent: 36,
                frames_dropped: 1,
            },
        );
        assert!(arcen_telemetry::ValidatedLifecycleEvent::new(
            LifecycleEventKind::SessionEnd,
            CorrelationId::from_uuid_v4_bytes([7; 16]),
            fields,
        )
        .is_ok());
    }
}
