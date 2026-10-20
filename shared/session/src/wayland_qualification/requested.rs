//! Displays Decks asked Speed for that no qualification covered.
//!
//! When a Speed request finds no measured or inferred rate, the Pier notes
//! the display here (a root-owned file beside the record, written only by
//! the Pier as root). The next qualification measures these displays too,
//! under the trusted temporary account, so Speed reaches them without an
//! operator naming sizes. The file carries sizes, never evidence: nothing
//! here admits anything.

use serde::{Deserialize, Serialize};

use super::speed::QualifyOutput;

/// Schema of the requested-displays file.
pub const REQUESTED_SCHEMA_VERSION: u32 = 1;

/// Most displays kept; the least recently requested is forgotten first.
pub const MAX_REQUESTED_OUTPUTS: usize = 16;

/// One requested display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestedSpeedOutput {
    /// The display.
    pub output: QualifyOutput,
    /// Codec token the session used.
    pub codec: String,
    /// When it was last requested, seconds since the Unix epoch.
    pub last_requested_unix_s: u64,
}

/// The file's contents.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestedSpeedOutputs {
    /// [`REQUESTED_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Most recent first.
    pub entries: Vec<RequestedSpeedOutput>,
}

impl RequestedSpeedOutputs {
    /// An empty list of this schema.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            schema_version: REQUESTED_SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }

    /// Notes a request; returns whether the list changed in a way worth
    /// writing (a new display or codec, or a refresh of an entry older than
    /// a day).
    pub fn note(&mut self, output: QualifyOutput, codec: &str, now_unix_s: u64) -> bool {
        const REFRESH_AFTER_S: u64 = 24 * 60 * 60;
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.output == output && entry.codec == codec)
        {
            let mut entry = self.entries.remove(index);
            let stale = now_unix_s.saturating_sub(entry.last_requested_unix_s) >= REFRESH_AFTER_S;
            if stale {
                entry.last_requested_unix_s = now_unix_s;
            }
            self.entries.insert(0, entry);
            return stale || index != 0;
        }
        self.entries.insert(
            0,
            RequestedSpeedOutput {
                output,
                codec: codec.to_owned(),
                last_requested_unix_s: now_unix_s,
            },
        );
        self.entries.truncate(MAX_REQUESTED_OUTPUTS);
        true
    }

    /// The displays, most recent first, each once.
    #[must_use]
    pub fn outputs(&self) -> Vec<QualifyOutput> {
        let mut outputs: Vec<QualifyOutput> = Vec::new();
        for entry in &self.entries {
            if !outputs.contains(&entry.output) {
                outputs.push(entry.output);
            }
        }
        outputs
    }

    /// Drops entries whose display is not one a compositor can be asked
    /// for (a hand-edited or corrupted file).
    #[must_use]
    pub fn validated(mut self) -> Self {
        self.entries.retain(|entry| {
            QualifyOutput::new(
                entry.output.width,
                entry.output.height,
                entry.output.display_refresh_hz,
            )
            .is_some()
                && matches!(entry.codec.as_str(), "h264" | "h265" | "av1")
        });
        self.entries.truncate(MAX_REQUESTED_OUTPUTS);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(width: u32, height: u32) -> QualifyOutput {
        QualifyOutput::new(width, height, 60).unwrap()
    }

    #[test]
    fn requests_are_kept_newest_first_bounded_and_written_only_when_new() {
        let mut list = RequestedSpeedOutputs::new();
        assert!(list.note(o(1800, 1130), "h265", 1_000));
        assert!(!list.note(o(1800, 1130), "h265", 2_000));
        assert!(list.note(o(1470, 956), "h265", 3_000));
        assert_eq!(list.outputs(), vec![o(1470, 956), o(1800, 1130)]);
        // Moving to the front is a change; a day later the timestamp is too.
        assert!(list.note(o(1800, 1130), "h265", 4_000));
        assert!(!list.note(o(1800, 1130), "h265", 5_000));
        assert!(list.note(o(1800, 1130), "h265", 5_000 + 86_400));
        assert_eq!(list.entries[0].last_requested_unix_s, 5_000 + 86_400);
        // Another codec is another entry, one display.
        assert!(list.note(o(1800, 1130), "h264", 6_000));
        assert_eq!(list.outputs(), vec![o(1800, 1130), o(1470, 956)]);
        for width in 0..40 {
            list.note(o(1000 + width * 2, 700), "h265", 7_000);
        }
        assert_eq!(list.entries.len(), MAX_REQUESTED_OUTPUTS);
        assert_eq!(list.entries[0].output, o(1078, 700));
    }

    #[test]
    fn the_file_round_trips_and_drops_what_no_compositor_takes() {
        let mut list = RequestedSpeedOutputs::new();
        list.note(o(1800, 1130), "h265", 1);
        let json = serde_json::to_string(&list).unwrap();
        assert!(json.contains("\"output\":{\"width\":1800"));
        assert!(json.contains("\"display_refresh_hz\":60"));
        let parsed: RequestedSpeedOutputs = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, list);
        let hand_edited: RequestedSpeedOutputs = serde_json::from_str(
            r#"{"schema_version":1,"entries":[
                {"output":{"width":0,"height":10,"display_refresh_hz":60},"codec":"h265","last_requested_unix_s":1},
                {"output":{"width":1920,"height":1080,"display_refresh_hz":60},"codec":"mjpeg","last_requested_unix_s":1},
                {"output":{"width":1920,"height":1080,"display_refresh_hz":60},"codec":"h264","last_requested_unix_s":1}]}"#,
        )
        .unwrap();
        assert_eq!(hand_edited.validated().outputs(), vec![o(1920, 1080)]);
    }
}
