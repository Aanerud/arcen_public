//! macOS pasteboard access for the Pier.
//!
//! This is the native adapter only. Direction, content limits, size caps,
//! sequence gating and echo suppression are host-authoritative policy and live
//! in `arcen-media`; this module only reads and writes the general pasteboard
//! and reports when it changed.
//!
//! Change detection uses the pasteboard's own change counter rather than
//! comparing contents. Comparing contents would miss a copy that produced the
//! same bytes, and would mean reading every payload on every poll just to find
//! out nothing happened.

use objc2::rc::Retained;
use objc2_app_kit::NSPasteboard;
use objc2_foundation::{NSData, NSString};
use serde::Serialize;

/// Uniform type identifier for plain UTF-8 text on the pasteboard.
const UTI_PLAIN_TEXT: &str = "public.utf8-plain-text";
/// Uniform type identifier for PNG image data.
const UTI_PNG: &str = "public.png";

/// A pasteboard payload the Pier can carry.
///
/// These are exactly the two kinds the clipboard subprotocol defines. Anything
/// else on the pasteboard is ignored rather than guessed at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "data")]
pub enum ClipboardPayload {
    /// UTF-8 text.
    Text(String),
    /// PNG image bytes.
    ImagePng(Vec<u8>),
}

impl ClipboardPayload {
    /// Returns the payload size in bytes, for policy checks.
    #[must_use]
    pub fn size_bytes(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::ImagePng(bytes) => bytes.len(),
        }
    }

    /// Returns whether the payload carries no content.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.size_bytes() == 0
    }
}

/// Why the pasteboard could not be read or written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub enum ClipboardError {
    /// `AppKit` would not return a general pasteboard. A daemon with no window
    /// server session sees this.
    PasteboardUnavailable,
    /// The pasteboard rejected the write.
    WriteRejected,
}

impl std::fmt::Display for ClipboardError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PasteboardUnavailable => {
                formatter.write_str("no general pasteboard is available in this session")
            }
            Self::WriteRejected => formatter.write_str("the pasteboard rejected the write"),
        }
    }
}

impl std::error::Error for ClipboardError {}

/// Reads and writes the general pasteboard, tracking when it changes.
pub struct Pasteboard {
    inner: Retained<NSPasteboard>,
    last_change_count: isize,
}

impl std::fmt::Debug for Pasteboard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Pasteboard")
            .field("last_change_count", &self.last_change_count)
            .finish_non_exhaustive()
    }
}

impl Pasteboard {
    /// Opens the general pasteboard.
    ///
    /// # Errors
    ///
    /// Returns [`ClipboardError::PasteboardUnavailable`] when `AppKit` has no
    /// pasteboard for this session.
    pub fn general() -> Result<Self, ClipboardError> {
        let inner = NSPasteboard::generalPasteboard();
        let last_change_count = inner.changeCount();
        Ok(Self {
            inner,
            last_change_count,
        })
    }

    /// Returns the pasteboard's current change counter.
    #[must_use]
    pub fn change_count(&self) -> isize {
        self.inner.changeCount()
    }

    /// Returns whether the pasteboard changed since the last observation, and
    /// records the new value.
    ///
    /// Calling this twice for one change reports `true` then `false`, which is
    /// what keeps a poll loop from re-sending the same copy forever.
    pub fn take_changed(&mut self) -> bool {
        let current = self.change_count();
        if current == self.last_change_count {
            return false;
        }
        self.last_change_count = current;
        true
    }

    /// Reads the current pasteboard payload, preferring text over images.
    ///
    /// Returns `None` when the pasteboard holds nothing the clipboard
    /// subprotocol carries.
    #[must_use]
    pub fn read(&self) -> Option<ClipboardPayload> {
        if let Some(text) = self.read_text() {
            return Some(ClipboardPayload::Text(text));
        }
        self.read_png().map(ClipboardPayload::ImagePng)
    }

    fn read_text(&self) -> Option<String> {
        let uti = NSString::from_str(UTI_PLAIN_TEXT);
        self.inner
            .stringForType(&uti)
            .map(|string| string.to_string())
    }

    fn read_png(&self) -> Option<Vec<u8>> {
        let uti = NSString::from_str(UTI_PNG);
        let data = self.inner.dataForType(&uti)?;
        Some(data.to_vec())
    }

    /// Replaces the pasteboard contents.
    ///
    /// The change counter advances, so the caller must record the new value to
    /// avoid treating its own write as a remote copy. [`Self::write_owned`]
    /// does that in one step.
    ///
    /// # Errors
    ///
    /// Returns [`ClipboardError::WriteRejected`] when `AppKit` refuses the write.
    pub fn write(&self, payload: &ClipboardPayload) -> Result<(), ClipboardError> {
        self.inner.clearContents();
        let accepted = match payload {
            ClipboardPayload::Text(text) => {
                let uti = NSString::from_str(UTI_PLAIN_TEXT);
                let value = NSString::from_str(text);
                self.inner.setString_forType(&value, &uti)
            }
            ClipboardPayload::ImagePng(bytes) => {
                let uti = NSString::from_str(UTI_PNG);
                let data = NSData::with_bytes(bytes);
                self.inner.setData_forType(Some(&data), &uti)
            }
        };
        if accepted {
            Ok(())
        } else {
            Err(ClipboardError::WriteRejected)
        }
    }

    /// Writes a payload and records the resulting change counter as our own.
    ///
    /// Without this, injecting a remote copy would look like a fresh local copy
    /// on the next poll and be sent straight back to the client.
    ///
    /// # Errors
    ///
    /// Returns [`ClipboardError::WriteRejected`] when `AppKit` refuses the write.
    pub fn write_owned(&mut self, payload: &ClipboardPayload) -> Result<(), ClipboardError> {
        self.write(payload)?;
        self.last_change_count = self.change_count();
        Ok(())
    }
}

/// What a clipboard probe observed.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClipboardProbeReport {
    /// Whether a general pasteboard was available at all.
    pub pasteboard_available: bool,
    /// The change counter before the probe wrote anything.
    pub initial_change_count: isize,
    /// Whether writing text advanced the change counter.
    pub write_advanced_change_count: bool,
    /// Whether the written text read back byte for byte.
    pub round_tripped: bool,
    /// Whether the probe's own write was correctly not reported as a change.
    pub own_write_not_seen_as_remote: bool,
    /// Whether the previous pasteboard contents were put back.
    pub restored: bool,
    /// Whether this run proves usable clipboard access.
    pub usable: bool,
    /// Why the run is not usable, when it is not.
    pub refusal: Option<String>,
}

/// Proves pasteboard read, write and change detection work.
///
/// The probe saves whatever was on the pasteboard, round-trips a marker, and
/// puts the original back. It refuses to report success unless the value read
/// back matches what was written.
///
/// # Errors
///
/// Returns [`ClipboardError`] when the pasteboard is unavailable.
pub fn probe() -> Result<ClipboardProbeReport, ClipboardError> {
    let mut pasteboard = match Pasteboard::general() {
        Ok(pasteboard) => pasteboard,
        Err(error) => {
            return Ok(ClipboardProbeReport {
                pasteboard_available: false,
                initial_change_count: 0,
                write_advanced_change_count: false,
                round_tripped: false,
                own_write_not_seen_as_remote: false,
                restored: false,
                usable: false,
                refusal: Some(error.to_string()),
            });
        }
    };

    let initial_change_count = pasteboard.change_count();
    let saved = pasteboard.read();

    let marker = ClipboardPayload::Text("arcen-pier-clipboard-probe".to_owned());
    pasteboard.write_owned(&marker)?;
    let write_advanced = pasteboard.change_count() != initial_change_count;
    let read_back = pasteboard.read();
    let round_tripped = read_back.as_ref() == Some(&marker);
    // Our own write must not look like someone copying on the host, or the
    // Pier would echo every injected clipboard back to the client.
    let own_write_not_seen_as_remote = !pasteboard.take_changed();

    let restored = match saved {
        Some(previous) => pasteboard.write_owned(&previous).is_ok(),
        None => {
            // Nothing was there to begin with; leave it empty rather than
            // leaving our marker behind.
            pasteboard
                .write_owned(&ClipboardPayload::Text(String::new()))
                .is_ok()
        }
    };

    let refusal = if !write_advanced {
        Some("writing the pasteboard did not advance its change counter".to_owned())
    } else if !round_tripped {
        Some("pasteboard did not return the bytes that were written".to_owned())
    } else if !own_write_not_seen_as_remote {
        Some("the Pier's own write was reported as a remote change".to_owned())
    } else {
        None
    };

    Ok(ClipboardProbeReport {
        pasteboard_available: true,
        initial_change_count,
        write_advanced_change_count: write_advanced,
        round_tripped,
        own_write_not_seen_as_remote,
        restored,
        usable: refusal.is_none(),
        refusal,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_sizes_reflect_their_bytes() {
        assert_eq!(ClipboardPayload::Text("hello".to_owned()).size_bytes(), 5);
        assert_eq!(ClipboardPayload::ImagePng(vec![0, 1, 2, 3]).size_bytes(), 4);
    }

    #[test]
    fn empty_payloads_are_reported_as_empty() {
        assert!(ClipboardPayload::Text(String::new()).is_empty());
        assert!(ClipboardPayload::ImagePng(Vec::new()).is_empty());
        assert!(!ClipboardPayload::Text("x".to_owned()).is_empty());
    }

    #[test]
    fn multibyte_text_is_measured_in_bytes_not_characters() {
        // Size checks feed the shared byte cap, so a character count would
        // let an oversized payload through.
        let payload = ClipboardPayload::Text("\u{1F600}".to_owned());
        assert_eq!(payload.size_bytes(), 4);
    }

    #[test]
    fn the_two_carried_kinds_are_distinct() {
        let text = ClipboardPayload::Text("a".to_owned());
        let image = ClipboardPayload::ImagePng(b"a".to_vec());
        assert_ne!(text, image);
    }
}
