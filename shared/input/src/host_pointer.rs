// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Host pointer positions in client-drawn cursor authority.
//!
//! The Deck draws the cursor where its own pointing device is. When the
//! host pointer moves for a reason the Deck did not cause — a USB-bridged
//! tablet that the host's own driver turns into pointer motion, an
//! application warping the pointer, a device plugged into the host — the
//! Pier reports where its pointer is ([`HostPointerReporter`], one per
//! attachment), and the Deck draws the cursor there until its own pointing
//! device moves again ([`PointerAuthority`]). What it draws there is
//! [`led_pointer_mark`]: the host's image, an arrow, or a pen mark when the
//! host hides its pointer while a bridged pen drives it.
//!
//! The Pier's platform code only reads its pointer (in desktop pixels) and
//! says when it injected the Deck's own pointer input; everything else is
//! here:
//!
//! - **Echo suppression.** A position read within [`ECHO_WINDOW`] of the
//!   Deck's own pointer input is that input's echo and never reported
//!   (the Deck already shows it). Unchanged positions are never reported.
//! - **Coalescing.** Only the newest position waits; it supersedes an
//!   unsent one, and Deck input discards it.
//! - **Rate.** At most one record per [`MIN_REPORT_INTERVAL`] (120 Hz).
//! - **Mapping.** Desktop pixels become the stream pixels of the output that
//!   shows them ([`HostPointerLayout`]), with that output's monitor id,
//!   number and generation, as `arcen_protocol::cursor_position` carries.
//! - **Lane discipline.** A prepared record changes nothing until the lane
//!   accepted it ([`HostPointerReporter::commit`]); a refused send keeps the
//!   newest position pending, so the lane is never waited on.

use std::time::{Duration, Instant};

use arcen_protocol::cursor_position::{
    CURSOR_POSITION_SIZE, CursorPosition, CursorPositionError, decode_cursor_position,
    encode_cursor_position,
};

/// How long after the Deck's own pointer input a host pointer position is
/// taken to be its echo.
///
/// Injection reaches the host pointer within milliseconds; the margin covers
/// a host that is briefly busy. A tablet stroke that starts while the Deck's
/// mouse is still moving is reported once the mouse has rested this long.
pub const ECHO_WINDOW: Duration = Duration::from_millis(150);

/// Shortest interval between two reports (120 Hz).
pub const MIN_REPORT_INTERVAL: Duration = Duration::from_micros(8_333);

/// How long after its own pointer moved the Deck keeps showing its own
/// pointer whatever the host reports (a report that was in flight when the
/// user picked up the mouse).
pub const CLIENT_MOTION_GUARD: Duration = Duration::from_millis(150);

/// Whether a client input message type moves the host pointer (directly or
/// by carrying a position).
#[must_use]
pub fn moves_host_pointer(message_type: &str) -> bool {
    use arcen_protocol::messages::{
        MOUSE_MOVE_RELATIVE, PEN_EVENT, REGION_PEN_EVENT, REGION_POINTER_BUTTON,
        REGION_POINTER_ENTER, REGION_POINTER_MOTION, REGION_POINTER_SCROLL,
    };
    matches!(
        message_type,
        "mouse_move" | "mouse_button" | MOUSE_MOVE_RELATIVE | PEN_EVENT
    ) || matches!(
        message_type,
        REGION_POINTER_MOTION
            | REGION_POINTER_BUTTON
            | REGION_POINTER_ENTER
            | REGION_POINTER_SCROLL
            | REGION_PEN_EVENT
    )
}

/// Where a host's pointer was, as its platform reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopPointerSample {
    /// Column in desktop pixels.
    pub x: i32,
    /// Row in desktop pixels.
    pub y: i32,
    /// Width of the whole desktop in pixels.
    pub desktop_width: u32,
    /// Height of the whole desktop in pixels.
    pub desktop_height: u32,
    /// When it was read.
    pub at: Instant,
}

/// One streamed output's place on the host desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostPointerOutput {
    /// Video monitor id of the stream that shows it (0: primary or only).
    pub monitor_id: u16,
    /// The host's output number (as in `CursorUpdate`; 0: single output).
    pub output: u32,
    /// Output generation of its pixel space.
    pub generation: u32,
    /// Left edge in desktop pixels.
    pub desktop_x: i32,
    /// Top edge in desktop pixels.
    pub desktop_y: i32,
    /// Width in desktop pixels.
    pub desktop_width: u32,
    /// Height in desktop pixels.
    pub desktop_height: u32,
    /// Width of its stream in pixels.
    pub stream_width: u16,
    /// Height of its stream in pixels.
    pub stream_height: u16,
}

impl HostPointerOutput {
    fn contains(&self, x: i32, y: i32) -> bool {
        let dx = i64::from(x) - i64::from(self.desktop_x);
        let dy = i64::from(y) - i64::from(self.desktop_y);
        (0..i64::from(self.desktop_width)).contains(&dx)
            && (0..i64::from(self.desktop_height)).contains(&dy)
    }

    fn usable(&self) -> bool {
        self.desktop_width > 0
            && self.desktop_height > 0
            && self.stream_width > 0
            && self.stream_height > 0
    }
}

/// A host pointer position located on the streams (no sequence yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocatedPointer {
    /// Whether the pointer is on a streamed output.
    pub on_stream: bool,
    /// See [`HostPointerOutput::output`].
    pub output: u32,
    /// See [`HostPointerOutput::generation`].
    pub generation: u32,
    /// See [`HostPointerOutput::monitor_id`].
    pub monitor_id: u16,
    /// Column in stream pixels.
    pub x: i32,
    /// Row in stream pixels.
    pub y: i32,
    /// Stream width.
    pub space_width: u16,
    /// Stream height.
    pub space_height: u16,
}

impl LocatedPointer {
    const fn record(self, sequence: u64) -> CursorPosition {
        CursorPosition {
            sequence,
            on_stream: self.on_stream,
            output: self.output,
            generation: self.generation,
            monitor_id: self.monitor_id,
            x: self.x,
            y: self.y,
            space_width: self.space_width,
            space_height: self.space_height,
        }
    }
}

/// Where the streamed outputs are on the host desktop.
///
/// Outputs whose stream is rotated relative to the desktop are left out by
/// the platform (their pixels do not map by scale alone); a pointer on one
/// of them is reported as off the streams, and the Deck keeps its own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostPointerLayout {
    outputs: Vec<HostPointerOutput>,
}

impl HostPointerLayout {
    /// A desktop shown whole by one stream (monitor 0, output 0).
    #[must_use]
    pub fn single(
        desktop_width: u32,
        desktop_height: u32,
        stream_width: u16,
        stream_height: u16,
    ) -> Self {
        Self::new(vec![HostPointerOutput {
            monitor_id: 0,
            output: 0,
            generation: 0,
            desktop_x: 0,
            desktop_y: 0,
            desktop_width,
            desktop_height,
            stream_width,
            stream_height,
        }])
    }

    /// A desktop shown by these outputs; empty ones are dropped.
    #[must_use]
    pub fn new(outputs: Vec<HostPointerOutput>) -> Self {
        Self {
            outputs: outputs
                .into_iter()
                .filter(HostPointerOutput::usable)
                .collect(),
        }
    }

    /// Whether no output can be located.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.outputs.is_empty()
    }

    /// The streamed position of desktop pixel `(x, y)`; `None` when the
    /// layout is empty.
    #[must_use]
    pub fn locate(&self, x: i32, y: i32) -> Option<LocatedPointer> {
        let Some(hit) = self.outputs.iter().find(|output| output.contains(x, y)) else {
            let first = self.outputs.first()?;
            return Some(LocatedPointer {
                on_stream: false,
                output: first.output,
                generation: first.generation,
                monitor_id: first.monitor_id,
                x: 0,
                y: 0,
                space_width: first.stream_width,
                space_height: first.stream_height,
            });
        };
        let scale = |offset: i64, desktop: u32, stream: u16| {
            let scaled = offset * i64::from(stream) / i64::from(desktop);
            i32::try_from(scaled).unwrap_or(i32::MAX)
        };
        Some(LocatedPointer {
            on_stream: true,
            output: hit.output,
            generation: hit.generation,
            monitor_id: hit.monitor_id,
            x: scale(
                i64::from(x) - i64::from(hit.desktop_x),
                hit.desktop_width,
                hit.stream_width,
            ),
            y: scale(
                i64::from(y) - i64::from(hit.desktop_y),
                hit.desktop_height,
                hit.stream_height,
            ),
            space_width: hit.stream_width,
            space_height: hit.stream_height,
        })
    }
}

/// A record ready to send, and what sending it commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedHostPointer {
    /// The encoded `CursorPosition`.
    pub bytes: [u8; CURSOR_POSITION_SIZE],
    /// The record.
    pub position: CursorPosition,
    located: LocatedPointer,
}

/// The Pier's side, one per attachment: which host pointer positions the
/// Deck must hear about, and when.
#[derive(Debug, Default)]
pub struct HostPointerReporter {
    layout: HostPointerLayout,
    last_client_input: Option<Instant>,
    last_observed: Option<(i32, i32)>,
    pending: Option<LocatedPointer>,
    sent: Option<LocatedPointer>,
    last_sent_at: Option<Instant>,
    sequence: u64,
    echoes: u64,
    reports: u64,
}

impl HostPointerReporter {
    /// A reporter for this layout.
    #[must_use]
    pub fn new(layout: HostPointerLayout) -> Self {
        Self {
            layout,
            ..Self::default()
        }
    }

    /// The streamed outputs changed (a resize or a new topology).
    pub fn set_layout(&mut self, layout: HostPointerLayout) {
        if self.layout != layout {
            self.layout = layout;
            self.last_observed = None;
            self.pending = None;
        }
    }

    /// The layout in use.
    #[must_use]
    pub const fn layout(&self) -> &HostPointerLayout {
        &self.layout
    }

    /// The Deck's own pointer input was injected at `at`. Whatever host
    /// position waits is obsolete: the Deck's pointer leads again.
    pub fn note_client_pointer(&mut self, at: Instant) {
        if self.last_client_input.is_none_or(|last| at > last) {
            self.last_client_input = Some(at);
        }
        self.pending = None;
        self.sent = None;
    }

    /// The host pointer was at desktop pixel `(x, y)` at `at`. Returns
    /// whether a report now waits.
    ///
    /// The first position (of the attachment, or after a layout change) is
    /// only where the pointer rests, not a move: it is never reported, so a
    /// Deck keeps its own pointer until the host pointer actually moves.
    pub fn observe(&mut self, x: i32, y: i32, at: Instant) -> bool {
        let Some(last) = self.last_observed.replace((x, y)) else {
            return false;
        };
        if last == (x, y) {
            return false;
        }
        let echo = self
            .last_client_input
            .is_some_and(|input| at <= input + ECHO_WINDOW);
        if echo {
            self.echoes += 1;
            self.pending = None;
            return false;
        }
        let Some(located) = self.layout.locate(x, y) else {
            return false;
        };
        if self.sent == Some(located) {
            self.pending = None;
            return false;
        }
        self.pending = Some(located);
        true
    }

    /// Whether a report waits.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// When the waiting report may be sent; `None` when none waits.
    #[must_use]
    pub fn due_at(&self) -> Option<Instant> {
        self.pending?;
        Some(
            self.last_sent_at
                .map_or_else(Instant::now, |last| last + MIN_REPORT_INTERVAL),
        )
    }

    /// The record for the waiting position, when one waits and the rate
    /// allows it at `now`.
    ///
    /// # Errors
    ///
    /// [`CursorPositionError`] only when the layout holds an empty pixel
    /// space, which [`HostPointerLayout::new`] prevents.
    pub fn prepare(
        &self,
        now: Instant,
    ) -> Result<Option<PreparedHostPointer>, CursorPositionError> {
        let Some(located) = self.pending else {
            return Ok(None);
        };
        if self
            .last_sent_at
            .is_some_and(|last| now < last + MIN_REPORT_INTERVAL)
        {
            return Ok(None);
        }
        let position = located.record(self.sequence + 1);
        Ok(Some(PreparedHostPointer {
            bytes: encode_cursor_position(&position)?,
            position,
            located,
        }))
    }

    /// Commits a record the lane accepted at `now`.
    pub fn commit(&mut self, prepared: &PreparedHostPointer, now: Instant) {
        self.sequence = prepared.position.sequence;
        self.sent = Some(prepared.located);
        self.last_sent_at = Some(now);
        self.reports += 1;
        if self.pending == Some(prepared.located) {
            self.pending = None;
        }
    }

    /// The lane was (re)opened: its sequence restarts, and nothing old is
    /// replayed (the Deck's own pointer leads until the host moves again).
    pub fn reset_lane(&mut self) {
        self.sequence = 0;
        self.sent = None;
        self.pending = None;
        self.last_sent_at = None;
    }

    /// Positions suppressed as echoes of the Deck's own input.
    #[must_use]
    pub const fn echoes(&self) -> u64 {
        self.echoes
    }

    /// Records committed.
    #[must_use]
    pub const fn reports(&self) -> u64 {
        self.reports
    }
}

/// The one feed from a desktop's pointer samples into a
/// [`HostPointerReporter`], for every Pier: a fixed layout (a multi-output
/// desktop's committed streams, or one captured display), or a single
/// stream whose size follows the samples.
#[derive(Debug)]
pub struct HostPointerFeed {
    reporter: HostPointerReporter,
    fixed_layout: bool,
}

impl HostPointerFeed {
    /// A feed for `layout`, or for one stream that shows the whole desktop
    /// at the size the samples carry (`None`).
    #[must_use]
    pub fn new(layout: Option<HostPointerLayout>) -> Self {
        Self {
            fixed_layout: layout.is_some(),
            reporter: HostPointerReporter::new(layout.unwrap_or_default()),
        }
    }

    /// One sample of where the desktop's pointer is. Returns whether a
    /// report now waits.
    pub fn observe(&mut self, sample: &DesktopPointerSample) -> bool {
        if !self.fixed_layout {
            let side = |value: u32| u16::try_from(value).unwrap_or(u16::MAX);
            self.reporter.set_layout(HostPointerLayout::single(
                sample.desktop_width,
                sample.desktop_height,
                side(sample.desktop_width),
                side(sample.desktop_height),
            ));
        }
        self.reporter.observe(sample.x, sample.y, sample.at)
    }

    /// The record to send now, when one waits and the rate allows it; it
    /// counts as sent (for a sender that cannot refuse it).
    pub fn take_record(&mut self, now: Instant) -> Option<[u8; CURSOR_POSITION_SIZE]> {
        let prepared = self.reporter.prepare(now).ok().flatten()?;
        self.reporter.commit(&prepared, now);
        Some(prepared.bytes)
    }

    /// The reporter, for echo notes, lane discipline and counters.
    pub fn reporter(&mut self) -> &mut HostPointerReporter {
        &mut self.reporter
    }

    /// When the waiting report may be sent; `None` when none waits.
    #[must_use]
    pub fn due_at(&self) -> Option<Instant> {
        self.reporter.due_at()
    }

    /// Records committed.
    #[must_use]
    pub const fn reports(&self) -> u64 {
        self.reporter.reports()
    }
}

/// The Deck's side of the records: sequence order across one lane.
#[derive(Debug, Default)]
pub struct DeckHostPointer {
    last_sequence: u64,
    dropped: u64,
}

impl DeckHostPointer {
    /// Applies one record; `Some` when it is newer than the last applied.
    ///
    /// # Errors
    ///
    /// [`CursorPositionError`] for a malformed record, which is dropped.
    pub fn apply(&mut self, bytes: &[u8]) -> Result<Option<CursorPosition>, CursorPositionError> {
        let position = decode_cursor_position(bytes).inspect_err(|_| self.dropped += 1)?;
        if position.sequence <= self.last_sequence {
            self.dropped += 1;
            return Ok(None);
        }
        self.last_sequence = position.sequence;
        Ok(Some(position))
    }

    /// The input/cursor lane was reopened: its sequence restarts.
    pub fn reset_lane(&mut self) {
        self.last_sequence = 0;
    }

    /// Records dropped as malformed or stale.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// Which pointer the Deck's cursor follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PointerSource {
    /// The Deck's own pointing device (its system cursor).
    #[default]
    Client,
    /// The host's reported pointer position.
    Host,
}

/// The Deck's arbitration between its own pointer and the host's reports.
///
/// The most recent mover leads: a host report takes the cursor unless the
/// Deck's own pointer moved within [`CLIENT_MOTION_GUARD`] (a report that was
/// in flight when the user picked up the mouse), and any motion of the
/// Deck's own pointer takes it back at once. A report that the pointer is
/// off the streams gives the cursor back too.
#[derive(Debug, Clone, Default)]
pub struct PointerAuthority {
    source: PointerSource,
    last_client_motion: Option<Instant>,
    host: Option<CursorPosition>,
}

impl PointerAuthority {
    /// The Deck's own pointer moved at `at`.
    pub fn client_moved(&mut self, at: Instant) {
        self.last_client_motion = Some(at);
        self.source = PointerSource::Client;
    }

    /// The host reported `position` at `at`; returns whether what the Deck
    /// shows changed.
    pub fn host_reported(&mut self, position: CursorPosition, at: Instant) -> bool {
        let before = self.host_position();
        self.host = Some(position);
        let guarded = self
            .last_client_motion
            .is_some_and(|motion| at < motion + CLIENT_MOTION_GUARD);
        if !position.on_stream {
            self.source = PointerSource::Client;
        } else if !guarded {
            self.source = PointerSource::Host;
        }
        before != self.host_position()
    }

    /// Who leads now.
    #[must_use]
    pub const fn source(&self) -> PointerSource {
        self.source
    }

    /// The host position to draw the cursor at, while the host leads.
    #[must_use]
    pub const fn host_position(&self) -> Option<CursorPosition> {
        match (self.source, self.host) {
            (PointerSource::Host, Some(position)) => Some(position),
            _ => None,
        }
    }

    /// A new connection or lane: the Deck's own pointer leads.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// What the Deck draws at a host-reported position while the host leads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedPointerMark {
    /// The host's own cursor image.
    Image,
    /// The client's ordinary arrow (no image, or a named shape).
    Arrow,
    /// A small pen mark: a bridged pen drives the host pointer.
    PenMark,
    /// Nothing: the host hides its pointer and no bridged pen drives it.
    Nothing,
}

/// The mark for a host-led cursor, from the cursor the Deck shows
/// (`shown`) and whether a USB-bridged tablet (Native tablet) drives the
/// host pointer.
///
/// One rule for every host: while Native tablet is active the host leads
/// only because the bridged pen moved its pointer (the Deck's own mouse
/// takes the cursor back the moment it moves), so the Deck draws the pen
/// mark there, whatever the host's cursor image is. GNOME on Wayland hides
/// its pointer while a tablet tool is in proximity, Xorg and macOS keep
/// showing the application's cursor; the user sees the same mark on all of
/// them (ADR 0023). Without a bridged pen the
/// host's own cursor is drawn.
#[must_use]
pub const fn led_pointer_mark(
    shown: &crate::cursor::DeckCursor,
    bridged_pen: bool,
) -> LedPointerMark {
    use crate::cursor::DeckCursor;
    if bridged_pen {
        return LedPointerMark::PenMark;
    }
    match shown {
        DeckCursor::Image { .. } => LedPointerMark::Image,
        DeckCursor::Default | DeckCursor::Named(_) => LedPointerMark::Arrow,
        DeckCursor::Hidden => LedPointerMark::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> HostPointerLayout {
        HostPointerLayout::single(1920, 1080, 1920, 1080)
    }

    fn send(reporter: &mut HostPointerReporter, now: Instant) -> Option<CursorPosition> {
        let prepared = reporter.prepare(now).unwrap()?;
        reporter.commit(&prepared, now);
        Some(decode_cursor_position(&prepared.bytes).unwrap())
    }

    #[test]
    fn host_motion_is_reported_and_echoes_of_client_input_are_not() {
        let start = Instant::now();
        let mut reporter = HostPointerReporter::new(layout());
        // Where the pointer rests at the start is not a move.
        assert!(!reporter.observe(50, 50, start));
        // The Deck's own motion and its echo.
        reporter.note_client_pointer(start);
        assert!(!reporter.observe(100, 100, start + Duration::from_millis(2)));
        assert!(!reporter.observe(101, 100, start + ECHO_WINDOW));
        assert_eq!(reporter.echoes(), 2);
        assert!(send(&mut reporter, start + ECHO_WINDOW).is_none());
        // The same position after the window is not news either.
        assert!(!reporter.observe(101, 100, start + Duration::from_secs(1)));
        // A bridged tablet moves the host pointer.
        let at = start + Duration::from_secs(1);
        assert!(reporter.observe(500, 400, at));
        let sent = send(&mut reporter, at).unwrap();
        assert_eq!((sent.x, sent.y, sent.sequence), (500, 400, 1));
        assert!(sent.on_stream);
        // Unchanged: nothing more.
        assert!(!reporter.observe(500, 400, at + Duration::from_millis(20)));
        assert!(send(&mut reporter, at + Duration::from_millis(20)).is_none());
    }

    #[test]
    fn a_sample_taken_before_late_client_input_is_still_an_echo() {
        let start = Instant::now();
        let mut reporter = HostPointerReporter::new(layout());
        reporter.observe(0, 0, start);
        reporter.note_client_pointer(start + Duration::from_millis(50));
        assert!(!reporter.observe(10, 10, start));
        // Client input discards a waiting host report.
        let mut reporter = HostPointerReporter::new(layout());
        reporter.observe(0, 0, start);
        assert!(reporter.observe(10, 10, start));
        reporter.note_client_pointer(start + Duration::from_millis(1));
        assert!(!reporter.is_pending());
        assert!(send(&mut reporter, start + Duration::from_millis(1)).is_none());
        // After the Deck led, the host's return to its last reported spot is
        // news again.
        let later = start + Duration::from_secs(1);
        assert!(reporter.observe(11, 11, later));
        send(&mut reporter, later).unwrap();
        reporter.note_client_pointer(later + Duration::from_millis(1));
        assert!(!reporter.observe(40, 40, later + Duration::from_millis(5)));
        assert!(reporter.observe(11, 11, later + Duration::from_secs(1)));
    }

    #[test]
    fn reports_are_coalesced_rate_limited_and_strictly_sequenced() {
        let start = Instant::now();
        let mut reporter = HostPointerReporter::new(layout());
        reporter.observe(0, 0, start);
        assert!(reporter.observe(1, 1, start));
        assert_eq!(send(&mut reporter, start).unwrap().sequence, 1);
        for step in 2..10 {
            reporter.observe(step, step, start + Duration::from_millis(1));
        }
        let early = start + Duration::from_millis(4);
        assert!(send(&mut reporter, early).is_none(), "rate limit");
        assert_eq!(
            reporter.due_at(),
            Some(start + MIN_REPORT_INTERVAL),
            "next slot"
        );
        let later = start + MIN_REPORT_INTERVAL;
        let sent = send(&mut reporter, later).unwrap();
        assert_eq!((sent.x, sent.sequence), (9, 2), "newest wins");
        // A refused send commits nothing and keeps the newest pending.
        reporter.observe(20, 20, later + MIN_REPORT_INTERVAL);
        let refused = reporter.prepare(later + MIN_REPORT_INTERVAL).unwrap();
        assert!(refused.is_some());
        reporter.observe(21, 21, later + MIN_REPORT_INTERVAL);
        let sent = send(&mut reporter, later + MIN_REPORT_INTERVAL).unwrap();
        assert_eq!((sent.x, sent.sequence), (21, 3));
        // A reopened lane restarts the sequence and replays nothing.
        reporter.reset_lane();
        assert!(!reporter.is_pending());
        reporter.observe(30, 30, later + Duration::from_secs(1));
        assert_eq!(
            send(&mut reporter, later + Duration::from_secs(1))
                .unwrap()
                .sequence,
            1
        );
    }

    #[test]
    fn multi_output_layouts_map_to_each_streams_pixels() {
        let layout = HostPointerLayout::new(vec![
            HostPointerOutput {
                monitor_id: 1,
                output: 1,
                generation: 4,
                desktop_x: 0,
                desktop_y: 0,
                desktop_width: 2560,
                desktop_height: 1440,
                stream_width: 2560,
                stream_height: 1440,
            },
            HostPointerOutput {
                monitor_id: 2,
                output: 2,
                generation: 4,
                desktop_x: 2560,
                desktop_y: 200,
                desktop_width: 1920,
                desktop_height: 1080,
                stream_width: 960,
                stream_height: 540,
            },
            HostPointerOutput {
                monitor_id: 3,
                output: 3,
                generation: 4,
                desktop_x: 0,
                desktop_y: 1440,
                desktop_width: 0,
                desktop_height: 0,
                stream_width: 10,
                stream_height: 10,
            },
        ]);
        let first = layout.locate(2559, 1439).unwrap();
        assert_eq!((first.monitor_id, first.x, first.y), (1, 2559, 1439));
        let second = layout.locate(2560 + 1000, 200 + 500).unwrap();
        assert_eq!(
            (second.monitor_id, second.output, second.x, second.y),
            (2, 2, 500, 250)
        );
        assert_eq!((second.space_width, second.space_height), (960, 540));
        // The gap below the second output is on no stream.
        let off = layout.locate(3000, 100).unwrap();
        assert!(!off.on_stream);
        assert!(layout.locate(-1, 0).is_some_and(|p| !p.on_stream));
        assert!(HostPointerLayout::default().locate(0, 0).is_none());
    }

    #[test]
    fn a_changed_layout_forgets_old_positions() {
        let start = Instant::now();
        let mut reporter = HostPointerReporter::new(layout());
        reporter.observe(0, 0, start);
        reporter.observe(100, 100, start);
        reporter.set_layout(HostPointerLayout::single(3840, 2160, 1920, 1080));
        assert!(!reporter.is_pending());
        assert!(!reporter.observe(90, 90, start), "a new baseline");
        assert!(reporter.observe(100, 100, start));
        let sent = send(&mut reporter, start).unwrap();
        assert_eq!((sent.x, sent.y, sent.space_width), (50, 50, 1920));
    }

    #[test]
    fn pointer_message_types_are_classified() {
        for kind in [
            "mouse_move",
            "mouse_move_relative",
            "mouse_button",
            "pen_event",
            "region_pointer_motion",
            "region_pointer_button",
            "region_pointer_enter",
            "region_pointer_scroll",
            "region_pen_event",
        ] {
            assert!(moves_host_pointer(kind), "{kind}");
        }
        for kind in [
            "key_event",
            "text_commit",
            "mouse_scroll",
            "gesture_magnify",
        ] {
            assert!(!moves_host_pointer(kind), "{kind}");
        }
    }

    fn position(sequence: u64, on_stream: bool) -> CursorPosition {
        CursorPosition {
            sequence,
            on_stream,
            output: 0,
            generation: 0,
            monitor_id: 0,
            x: 10,
            y: 20,
            space_width: 100,
            space_height: 100,
        }
    }

    #[test]
    fn the_deck_applies_records_in_sequence_per_lane() {
        let mut deck = DeckHostPointer::default();
        let bytes = |sequence| encode_cursor_position(&position(sequence, true)).unwrap();
        assert!(deck.apply(&bytes(2)).unwrap().is_some());
        assert!(deck.apply(&bytes(2)).unwrap().is_none());
        assert!(deck.apply(&bytes(1)).unwrap().is_none());
        assert!(deck.apply(&[0x51, 1]).is_err());
        assert_eq!(deck.dropped(), 3);
        deck.reset_lane();
        assert!(deck.apply(&bytes(1)).unwrap().is_some());
    }

    #[test]
    fn the_most_recent_mover_leads_the_decks_cursor() {
        let start = Instant::now();
        let mut authority = PointerAuthority::default();
        assert_eq!(authority.source(), PointerSource::Client);
        // A pen stroke on a bridged tablet: the host leads.
        assert!(authority.host_reported(position(1, true), start));
        assert_eq!(authority.source(), PointerSource::Host);
        assert_eq!(authority.host_position(), Some(position(1, true)));
        // The Mac mouse moves: the Mac leads at once ...
        let moved = start + Duration::from_secs(1);
        authority.client_moved(moved);
        assert_eq!(authority.host_position(), None);
        // ... and a report that was in flight does not take it back.
        assert!(!authority.host_reported(position(2, true), moved + Duration::from_millis(20)));
        assert_eq!(authority.source(), PointerSource::Client);
        // Once the mouse rests, the next host move leads again.
        assert!(authority.host_reported(position(3, true), moved + CLIENT_MOTION_GUARD));
        // Off the streams: the Mac's own pointer again.
        assert!(authority.host_reported(position(4, false), moved + Duration::from_secs(1)));
        assert_eq!(authority.source(), PointerSource::Client);
        authority.reset();
        assert_eq!(authority.host_position(), None);
    }

    /// One mark for a bridged pen on every host (GNOME hides its pointer,
    /// Xorg and macOS show the application's); the host's cursor otherwise.
    #[test]
    fn a_bridged_pen_always_gets_the_pen_mark_and_anything_else_the_host_cursor() {
        use crate::cursor::DeckCursor;
        let image = DeckCursor::Image {
            id: arcen_protocol::cursor::CursorImageId::new(7).unwrap(),
            image: arcen_media::cursor::CursorImage::from_straight_rgba(
                2,
                2,
                (0, 0),
                [0, 0, 0, 255].repeat(4),
            )
            .unwrap(),
            output: 0,
            generation: 1,
        };
        for shown in [&DeckCursor::Hidden, &DeckCursor::Default, &image] {
            assert_eq!(led_pointer_mark(shown, true), LedPointerMark::PenMark);
        }
        assert_eq!(
            led_pointer_mark(&DeckCursor::Hidden, false),
            LedPointerMark::Nothing
        );
        assert_eq!(led_pointer_mark(&image, false), LedPointerMark::Image);
        assert_eq!(
            led_pointer_mark(&DeckCursor::Default, false),
            LedPointerMark::Arrow
        );
    }

    #[test]
    fn the_feed_follows_the_sample_size_unless_its_layout_is_fixed() {
        let start = Instant::now();
        let sample = |x, width| DesktopPointerSample {
            x,
            y: 10,
            desktop_width: width,
            desktop_height: 1080,
            at: start + Duration::from_secs(1),
        };
        let mut follows = HostPointerFeed::new(None);
        assert!(!follows.observe(&sample(100, 1920)), "baseline");
        assert!(follows.observe(&sample(200, 1920)));
        let record = decode_cursor_position(&follows.take_record(start).unwrap()).unwrap();
        assert_eq!((record.x, record.space_width), (200, 1920));
        assert_eq!(follows.reports(), 1);
        // A resize is a new baseline, then reports in the new size.
        assert!(!follows.observe(&sample(300, 2560)));
        let mut fixed = HostPointerFeed::new(Some(layout()));
        fixed.observe(&sample(100, 2560));
        assert!(fixed.observe(&sample(1919, 2560)));
        let due = fixed.due_at().unwrap();
        let record = decode_cursor_position(&fixed.take_record(due).unwrap()).unwrap();
        assert_eq!((record.x, record.space_width), (1919, 1920));
    }
}
