//! The stdout of a Wayland epoch, shared by every pipeline (the NVENC
//! eight-bit runner, the wide Grading/HDR runner and the OpenH264 runner).
//!
//! The parent chooses the framing: framed-v1 (`u32_be length || access
//! unit`) or, for a metadata cursor stream whose cursor the Deck draws,
//! framed-v2 (`arcen_media::capenc_stream`), which also carries the
//! stream's cursor events. The parent reads exactly what it asked for, so a
//! pipeline must never write framed-v1 to a framed-v2 parent.

use std::io::Write;

use arcen_media::capenc_stream::{encode_cursor_event, write_record, CapencRecordKind};
use arcen_media::video::CursorEvidence;

use super::EpochRequest;
use crate::log;
use crate::wayland::cursor::{CursorEmitter, CursorState};

/// Cursor events logged per epoch; a busy (animated) cursor is summarised
/// by the epoch's count.
const LOGGED_CURSOR_EVENTS: u64 = 32;

/// How one epoch writes its stdout.
#[derive(Debug)]
pub(super) struct EpochOutput {
    framed: bool,
    framed_v2: bool,
    cursor: Option<CursorEmitter>,
    cursor_events_sent: u64,
}

impl EpochOutput {
    /// The output `request` asked for. Cursor events go out only on a
    /// metadata stream (`cursor`) whose parent reads framed-v2.
    pub(super) fn new(request: &EpochRequest, cursor: CursorEvidence) -> Self {
        Self {
            framed: request.framed,
            framed_v2: request.framed_v2,
            cursor: (request.framed_v2 && matches!(cursor, CursorEvidence::SeparateMetadata))
                .then(CursorEmitter::default),
            cursor_events_sent: 0,
        }
    }

    /// Whether access units are framed (v1 or v2), for READY's first-unit
    /// validation.
    pub(super) const fn framed(&self) -> bool {
        self.framed || self.framed_v2
    }

    /// Writes one access unit in the parent's framing.
    pub(super) fn access_unit(
        &self,
        writer: &mut impl Write,
        access_unit: &[u8],
    ) -> std::io::Result<()> {
        if self.framed_v2 {
            crate::validate_access_unit(access_unit, true)?;
            write_record(writer, CapencRecordKind::AccessUnit, access_unit)
        } else {
            crate::write_access_unit(writer, access_unit, self.framed)
        }
    }

    /// Writes a cursor event when the stream's cursor changed. Call it only
    /// after READY.
    pub(super) fn cursor(
        &mut self,
        writer: &mut impl Write,
        state: &CursorState,
    ) -> std::io::Result<()> {
        let Some(event) = self.cursor.as_mut().and_then(|emitter| emitter.poll(state)) else {
            return Ok(());
        };
        write_record(
            writer,
            CapencRecordKind::Cursor,
            &encode_cursor_event(&event),
        )?;
        self.cursor_events_sent += 1;
        if self.cursor_events_sent <= LOGGED_CURSOR_EVENTS {
            log(&format!("cursor event {} {event}", self.cursor_events_sent));
        }
        Ok(())
    }

    /// Whether this epoch reports cursor events.
    #[cfg(test)]
    const fn reports_cursor(&self) -> bool {
        self.cursor.is_some()
    }

    /// Cursor events written this epoch.
    pub(super) const fn cursor_events_sent(&self) -> u64 {
        self.cursor_events_sent
    }

    /// Cursor sprites dropped for an unknown pixel format.
    pub(super) fn unknown_cursor_formats(&self) -> u64 {
        self.cursor
            .as_ref()
            .map_or(0, CursorEmitter::unknown_formats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_media::capenc_stream::{
        decode_cursor_event, record_body_length, record_kind, RECORD_LENGTH_BYTES,
    };

    const ACCESS_UNIT: &[u8] = &[0, 0, 0, 1, 0x65, 0x88, 0x84];

    fn request(tokens: &[&str]) -> EpochRequest {
        let mut argv = vec!["0", "h264", "60"];
        argv.extend_from_slice(tokens);
        EpochRequest::from_args(&super::super::tests::args(&argv)).expect("request")
    }

    /// Splits framed-v2 bytes into records the way the Pier reads them.
    fn records(mut bytes: &[u8]) -> Vec<(CapencRecordKind, Vec<u8>)> {
        let mut out = Vec::new();
        while !bytes.is_empty() {
            let length: [u8; RECORD_LENGTH_BYTES] =
                bytes[..RECORD_LENGTH_BYTES].try_into().expect("length");
            let body_length = record_body_length(length).expect("record length");
            let kind = record_kind(bytes[RECORD_LENGTH_BYTES], body_length).expect("kind");
            let start = RECORD_LENGTH_BYTES + 1;
            out.push((kind, bytes[start..start + body_length].to_vec()));
            bytes = &bytes[start + body_length..];
        }
        out
    }

    #[test]
    fn a_framed_v2_parent_gets_records_from_every_pipelines_output() {
        let mut output = EpochOutput::new(
            &request(&["framed-v2", "cursor=local"]),
            CursorEvidence::SeparateMetadata,
        );
        assert!(output.framed() && output.reports_cursor());
        let mut stdout = Vec::new();
        output.access_unit(&mut stdout, ACCESS_UNIT).unwrap();
        let state = CursorState {
            metadata_seen: true,
            visible: false,
            ..CursorState::default()
        };
        output.cursor(&mut stdout, &state).unwrap();
        // Unchanged state: no second event.
        output.cursor(&mut stdout, &state).unwrap();
        output.access_unit(&mut stdout, ACCESS_UNIT).unwrap();
        let records = records(&stdout);
        assert_eq!(records.len(), 3);
        assert_eq!(
            records[0],
            (CapencRecordKind::AccessUnit, ACCESS_UNIT.to_vec())
        );
        assert_eq!(records[1].0, CapencRecordKind::Cursor);
        assert!(!decode_cursor_event(&records[1].1).unwrap().on_output);
        assert_eq!(records[2].0, CapencRecordKind::AccessUnit);
        assert_eq!(output.cursor_events_sent(), 1);
    }

    #[test]
    fn a_framed_v1_parent_gets_length_prefixed_units_and_no_cursor() {
        let mut output = EpochOutput::new(
            &request(&["framed-v1", "cursor=local"]),
            CursorEvidence::SeparateMetadata,
        );
        assert!(!output.reports_cursor());
        let mut stdout = Vec::new();
        output.access_unit(&mut stdout, ACCESS_UNIT).unwrap();
        output
            .cursor(
                &mut stdout,
                &CursorState {
                    metadata_seen: true,
                    ..CursorState::default()
                },
            )
            .unwrap();
        let length = u32::try_from(ACCESS_UNIT.len()).unwrap().to_be_bytes();
        assert_eq!(&stdout[..4], &length);
        assert_eq!(&stdout[4..], ACCESS_UNIT);
    }

    #[test]
    fn a_composited_cursor_on_framed_v2_reports_no_cursor_events() {
        let output = EpochOutput::new(
            &request(&["framed-v2", "cursor=host"]),
            CursorEvidence::Composited,
        );
        assert!(output.framed() && !output.reports_cursor());
    }
}
