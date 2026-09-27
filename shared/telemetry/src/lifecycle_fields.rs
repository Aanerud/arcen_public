//! Building the fields a canonical lifecycle event carries.
//!
//! Each of these events declares its fields in the schema, and every host has
//! to produce exactly those. Hand-rolling the `insert` calls per host is how
//! two Piers end up disagreeing about a field name that a reader then has to
//! special-case — and it had already happened: the macOS and Windows hosts
//! each built `SESSION_AUTH_OK` themselves, with the same two fields, in two
//! places.
//!
//! These builders return `Option` because the schema rejects a value it does
//! not declare, and a host that cannot build a record should skip it rather
//! than fail a session. Telemetry failing is not the session failing.
//!
//! What is deliberately *not* here is anything that decides what to report.
//! These turn values into fields; choosing the values is the host's business.

use crate::{FieldValue, StructuredFields};

/// Builds the fields for `SERVICE_START`.
///
/// A host that never started and a host that started and accepted nothing are
/// indistinguishable in a log without this.
#[must_use]
pub fn service_start(component: &str, version: &str, pid: u32) -> Option<StructuredFields> {
    let mut fields = StructuredFields::default();
    fields
        .insert("component", FieldValue::String(component.to_owned()))
        .ok()?;
    fields
        .insert("version", FieldValue::String(version.to_owned()))
        .ok()?;
    fields
        .insert("pid", FieldValue::Integer(i64::from(pid)))
        .ok()?;
    Some(fields)
}

/// Builds the fields for `SESSION_AUTH_OK`.
///
/// `identity_binding` says what the authenticated account was tied to — the
/// console user, a Windows logon session — which is the part that explains a
/// later refusal.
#[must_use]
pub fn session_auth_ok(
    auth_method: &str,
    identity_binding: &str,
    os_session_id: Option<u32>,
) -> Option<StructuredFields> {
    let mut fields = StructuredFields::default();
    fields
        .insert("auth_method", FieldValue::String(auth_method.to_owned()))
        .ok()?;
    fields
        .insert(
            "identity_binding",
            FieldValue::String(identity_binding.to_owned()),
        )
        .ok()?;
    if let Some(id) = os_session_id {
        fields
            .insert("os_session_id", FieldValue::Integer(i64::from(id)))
            .ok()?;
    }
    Some(fields)
}

/// Builds the fields for `SESSION_AUTH_FAIL`.
///
/// `reason_class` is a class, never the supplied credential and never the
/// underlying OS error string. An authentication log that quotes what was
/// typed becomes a credential store the first time somebody puts a password in
/// the username box, and both hosts already had a comment saying so.
#[must_use]
pub fn session_auth_fail(
    auth_method: &str,
    stage: &str,
    reason_class: &str,
) -> Option<StructuredFields> {
    let mut fields = StructuredFields::default();
    fields
        .insert("auth_method", FieldValue::String(auth_method.to_owned()))
        .ok()?;
    fields
        .insert("stage", FieldValue::String(stage.to_owned()))
        .ok()?;
    fields
        .insert("reason_class", FieldValue::String(reason_class.to_owned()))
        .ok()?;
    Some(fields)
}

/// Geometry and codec of a starting stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamStart<'a> {
    /// Encoder backend name.
    pub encoder: &'a str,
    /// Negotiated codec token.
    pub codec: &'a str,
    /// Negotiated chroma token.
    pub chroma: &'a str,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Frames per second, when a target exists.
    pub fps: Option<u32>,
    /// The colour identity of the stream being started.
    ///
    /// Optional only so an emitter that genuinely cannot describe its colour
    /// still records something. Both Piers can, and a record without it cannot
    /// answer the first question anyone asks of a session: `chroma` and
    /// `codec` alone describe a stream that might be eight-bit or ten, BT.709
    /// or PQ.
    pub color: Option<ColorIdentity<'a>>,
}

/// What a stream's colour actually is, in wire tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorIdentity<'a> {
    /// Coded bit depth token.
    pub bit_depth: &'a str,
    /// Colour range token.
    pub color_range: &'a str,
    /// Colour matrix token.
    pub color_matrix: &'a str,
    /// Colour primaries token.
    pub color_primaries: &'a str,
    /// Transfer characteristics token.
    pub transfer: &'a str,
}

/// Builds the fields for `SESSION_STREAM_START`.
#[must_use]
pub fn session_stream_start(start: StreamStart<'_>) -> Option<StructuredFields> {
    let mut fields = StructuredFields::default();
    fields
        .insert("encoder", FieldValue::String(start.encoder.to_owned()))
        .ok()?;
    fields
        .insert("codec", FieldValue::String(start.codec.to_owned()))
        .ok()?;
    fields
        .insert("chroma", FieldValue::String(start.chroma.to_owned()))
        .ok()?;
    fields
        .insert("width", FieldValue::Integer(i64::from(start.width)))
        .ok()?;
    fields
        .insert("height", FieldValue::Integer(i64::from(start.height)))
        .ok()?;
    if let Some(fps) = start.fps {
        fields
            .insert("fps", FieldValue::Integer(i64::from(fps)))
            .ok()?;
    }
    if let Some(color) = start.color {
        fields
            .insert("bit_depth", FieldValue::String(color.bit_depth.to_owned()))
            .ok()?;
        fields
            .insert(
                "color_range",
                FieldValue::String(color.color_range.to_owned()),
            )
            .ok()?;
        fields
            .insert(
                "color_matrix",
                FieldValue::String(color.color_matrix.to_owned()),
            )
            .ok()?;
        fields
            .insert(
                "color_primaries",
                FieldValue::String(color.color_primaries.to_owned()),
            )
            .ok()?;
        fields
            .insert("transfer", FieldValue::String(color.transfer.to_owned()))
            .ok()?;
    }
    Some(fields)
}

/// Builds the fields for `SESSION_END`.
///
/// `frames_sent` is the field that earns its place: a session can authenticate,
/// bind the right desktop, negotiate a codec and end cleanly having carried no
/// picture at all, and every other field reads as success. It separates "a
/// client connected" from "a desktop arrived".
#[must_use]
pub fn session_end(
    reason_class: &str,
    duration_ms: u64,
    frames_sent: u64,
) -> Option<StructuredFields> {
    let mut fields = StructuredFields::default();
    fields
        .insert("reason_class", FieldValue::String(reason_class.to_owned()))
        .ok()?;
    fields
        .insert("duration_ms", FieldValue::Integer(saturating(duration_ms)))
        .ok()?;
    fields
        .insert("frames_sent", FieldValue::Integer(saturating(frames_sent)))
        .ok()?;
    Some(fields)
}

/// Builds the fields for `HEALTH_SNAPSHOT`.
///
/// The measured rate, never the target. A host reporting the rate it aimed for
/// has recorded its intention rather than its behaviour.
#[must_use]
pub fn health_snapshot(
    overall_state: &str,
    fps_actual: Option<u32>,
    fps_target: Option<u32>,
) -> Option<StructuredFields> {
    let mut fields = StructuredFields::default();
    fields
        .insert(
            "overall_state",
            FieldValue::String(overall_state.to_owned()),
        )
        .ok()?;
    if let Some(actual) = fps_actual {
        fields
            .insert("fps_actual", FieldValue::Integer(i64::from(actual)))
            .ok()?;
    }
    if let Some(target) = fps_target {
        fields
            .insert("fps_target", FieldValue::Integer(i64::from(target)))
            .ok()?;
    }
    Some(fields)
}

/// Clamps rather than wrapping.
///
/// A duration or a frame count past `i64::MAX` is not a number anyone will
/// read, but wrapping it to a negative one puts a value in the log that looks
/// deliberate and is not.
fn saturating(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CorrelationId, LifecycleEventKind, ValidatedLifecycleEvent};

    fn accepted(kind: LifecycleEventKind, fields: Option<StructuredFields>) -> bool {
        let Some(fields) = fields else {
            return false;
        };
        ValidatedLifecycleEvent::new(kind, CorrelationId::from_uuid_v4_bytes([5; 16]), fields)
            .is_ok()
    }

    #[test]
    fn every_builder_satisfies_the_schema_it_is_for() {
        // The schema is the real assertion: an event carrying a field it does
        // not declare, or missing a required one, is refused rather than
        // written. These builders exist so that check passes in one place
        // instead of being rediscovered per host.
        assert!(accepted(
            LifecycleEventKind::ServiceStart,
            service_start("arcen-pier", "0.12.0", 1234)
        ));
        assert!(accepted(
            LifecycleEventKind::SessionAuthOk,
            session_auth_ok("pam", "console_user", None)
        ));
        assert!(accepted(
            LifecycleEventKind::SessionAuthFail,
            session_auth_fail("pam", "authenticate", "rejected")
        ));
        assert!(accepted(
            LifecycleEventKind::SessionEnd,
            session_end("completed", 1_483, 51)
        ));
        assert!(accepted(
            LifecycleEventKind::HealthSnapshot,
            health_snapshot("ok", Some(58), Some(60))
        ));
        assert!(accepted(
            LifecycleEventKind::SessionStreamStart,
            session_stream_start(StreamStart {
                encoder: "videotoolbox",
                codec: "h265",
                chroma: "420",
                width: 1920,
                height: 1080,
                fps: Some(60),
                color: None,
            })
        ));
    }

    #[test]
    fn the_colour_identity_reaches_the_record() {
        // The schema carries these five fields because chroma and codec alone
        // describe a stream that could be eight-bit or ten, BT.709 or PQ. A
        // host that omits them leaves the answer only in the client's log.
        let fields = session_stream_start(StreamStart {
            encoder: "videotoolbox",
            codec: "h265",
            chroma: "444",
            width: 1920,
            height: 1080,
            fps: Some(60),
            color: Some(ColorIdentity {
                bit_depth: "10",
                color_range: "limited",
                color_matrix: "bt709",
                color_primaries: "bt709",
                transfer: "bt709",
            }),
        })
        .expect("fields");
        assert!(accepted(
            LifecycleEventKind::SessionStreamStart,
            Some(fields.clone())
        ));
        let rendered = format!("{fields:?}");
        for key in [
            "bit_depth",
            "color_range",
            "color_matrix",
            "color_primaries",
            "transfer",
        ] {
            assert!(rendered.contains(key), "{key} must reach the record");
        }
    }

    #[test]
    fn an_optional_field_may_be_absent_without_failing_the_schema() {
        // A Pier with no OS session id, or no frame-rate target, must still be
        // able to record the event rather than skip it.
        assert!(accepted(
            LifecycleEventKind::SessionAuthOk,
            session_auth_ok("pam", "console_user", Some(3))
        ));
        assert!(accepted(
            LifecycleEventKind::HealthSnapshot,
            health_snapshot("ok", None, None)
        ));
    }

    #[test]
    fn a_count_past_the_signed_range_clamps_rather_than_wrapping() {
        // Wrapping would put a negative frame count in the log, which looks
        // deliberate and is not.
        let fields = session_end("completed", u64::MAX, u64::MAX).expect("fields");
        assert_eq!(fields.len(), 3);
        assert!(accepted(LifecycleEventKind::SessionEnd, Some(fields)));
    }
}
