// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The last session's Stream Statistics and truth, kept for a support
//! bundle made after the session ended.
//!
//! Shells record a snapshot while a session streams
//! ([`record_session_snapshot`]); the closure runs at most once every
//! [`SNAPSHOT_INTERVAL`], so calling it every UI pass costs one clock read.
//! Only text the Deck already shows on screen is kept: no clipboard, no
//! typed text, no credentials.

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How often a running session's snapshot is refreshed.
pub const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(2);
/// Lines kept per section (the overlays are a few dozen lines).
pub const MAX_SNAPSHOT_LINES: usize = 200;
/// Bytes kept per line.
pub const MAX_SNAPSHOT_LINE_BYTES: usize = 512;

/// What the Deck showed about its last session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSnapshot {
    /// When it was taken (filled by [`record_session_snapshot`]).
    pub captured_unix_seconds: u64,
    /// The Stream Statistics overlay's lines.
    pub stream_statistics: Vec<String>,
    /// The Negotiated Truth summary (requested vs served contract).
    pub negotiated_truth: Vec<String>,
    /// The connection health overlay (decode, wire, input, audio counters).
    pub health: Vec<String>,
    /// The decoder's refusal of the stream, when it refused one.
    pub decoder_refusal: Option<String>,
    /// Session telemetry counters (`client_telemetry`), as `name=value`.
    pub telemetry: Vec<String>,
}

impl SessionSnapshot {
    fn bounded(mut self) -> Self {
        for section in [
            &mut self.stream_statistics,
            &mut self.negotiated_truth,
            &mut self.health,
            &mut self.telemetry,
        ] {
            section.truncate(MAX_SNAPSHOT_LINES);
            for line in section.iter_mut() {
                truncate_on_char_boundary(line, MAX_SNAPSHOT_LINE_BYTES);
            }
        }
        if let Some(refusal) = self.decoder_refusal.as_mut() {
            truncate_on_char_boundary(refusal, MAX_SNAPSHOT_LINE_BYTES);
        }
        self
    }

    /// The snapshot as the support bundle records it.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "captured_unix_seconds": self.captured_unix_seconds,
            "stream_statistics": self.stream_statistics,
            "negotiated_truth": self.negotiated_truth,
            "health": self.health,
            "decoder_refusal": self.decoder_refusal,
            "telemetry": self.telemetry,
        })
    }
}

fn truncate_on_char_boundary(text: &mut String, max: usize) {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
}

/// The session telemetry counters as `name=value` lines, for
/// [`SessionSnapshot::telemetry`].
#[must_use]
pub fn telemetry_lines(telemetry: &crate::client_telemetry::ClientTelemetry) -> Vec<String> {
    let (received, decoded, presented, dropped) = telemetry.counters();
    let duration = telemetry.session_duration();
    let summary = telemetry.summary(duration);
    vec![
        format!("session_seconds={}", duration.as_secs()),
        format!("frames_received={received}"),
        format!("frames_decoded={decoded}"),
        format!("frames_presented={presented}"),
        format!("frames_dropped={dropped}"),
        format!("average_fps={}", summary.avg_fps),
        format!("average_rtt_ms={}", summary.avg_rtt_ms),
        format!(
            "worst_health={}",
            summary
                .worst_health
                .map_or_else(|| "none".to_owned(), |state| format!("{state:?}"))
        ),
        format!("reconnects={}", summary.reconnects),
    ]
}

struct Recorder {
    taken_at: Option<Instant>,
    last: Option<SessionSnapshot>,
}

static RECORDER: Mutex<Recorder> = Mutex::new(Recorder {
    taken_at: None,
    last: None,
});

/// Records the running session's snapshot, building it with `build` only
/// when [`SNAPSHOT_INTERVAL`] has passed since the last one.
pub fn record_session_snapshot(build: impl FnOnce() -> SessionSnapshot) {
    let now = Instant::now();
    let Ok(mut recorder) = RECORDER.lock() else {
        return;
    };
    if recorder
        .taken_at
        .is_some_and(|taken| now.duration_since(taken) < SNAPSHOT_INTERVAL)
    {
        return;
    }
    let mut snapshot = build().bounded();
    snapshot.captured_unix_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    recorder.taken_at = Some(now);
    recorder.last = Some(snapshot);
}

/// Makes the next [`record_session_snapshot`] call take a snapshot (for
/// example when a session ends, so its final counters are kept).
pub fn refresh_session_snapshot_now() {
    if let Ok(mut recorder) = RECORDER.lock() {
        recorder.taken_at = None;
    }
}

/// The host the last session in this process was made to.
static LAST_TARGET: Mutex<Option<(String, u16)>> = Mutex::new(None);

/// Records the host a session is being made to. The session engine calls
/// it for every attempt, saved connection or not, so a bundle made later
/// reports the network path to that host (`diagnostics/network.json`).
pub fn record_session_target(host: &str, port: u16) {
    let host = host.trim();
    if host.is_empty() {
        return;
    }
    let mut host = host.to_owned();
    truncate_on_char_boundary(&mut host, MAX_SNAPSHOT_LINE_BYTES);
    if let Ok(mut target) = LAST_TARGET.lock() {
        *target = Some((host, port));
    }
}

/// The host of the last session attempt in this process, if any.
#[must_use]
pub fn last_session_target() -> Option<(String, u16)> {
    LAST_TARGET.lock().ok().and_then(|target| target.clone())
}

/// The last recorded snapshot, if any session streamed in this process.
#[must_use]
pub fn last_session_snapshot() -> Option<SessionSnapshot> {
    RECORDER
        .lock()
        .ok()
        .and_then(|recorder| recorder.last.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_are_rate_limited_and_bounded() {
        refresh_session_snapshot_now();
        record_session_snapshot(|| SessionSnapshot {
            stream_statistics: vec!["é".repeat(MAX_SNAPSHOT_LINE_BYTES)],
            ..SessionSnapshot::default()
        });
        let mut built = false;
        record_session_snapshot(|| {
            built = true;
            SessionSnapshot::default()
        });
        assert!(!built, "a second snapshot within the interval is not built");
        let last = last_session_snapshot().expect("recorded");
        assert!(last.stream_statistics[0].len() <= MAX_SNAPSHOT_LINE_BYTES);
        assert!(last.captured_unix_seconds > 0);
        refresh_session_snapshot_now();
        record_session_snapshot(|| {
            built = true;
            SessionSnapshot::default()
        });
        assert!(built);
    }
}
