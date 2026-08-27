//! Clipboard policy, sequence, echo-suppression, and raster contracts.

mod image;

pub use image::{
    ClipboardImageError, ImageInfo, ImageLimits, dibv5_to_png, png_to_dibv5, validate_png,
};

use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};

/// Default host-authoritative clipboard transfer limit.
pub const DEFAULT_CLIPBOARD_BYTES: usize = 8 * 1024 * 1024;
/// Absolute encoded transfer ceiling accepted by protocol v1.
pub const HARD_MAX_CLIPBOARD_BYTES: usize = 20 * 1024 * 1024;
/// Absolute decoded RGBA/BGRA raster ceiling.
pub const MAX_DECODED_IMAGE_BYTES: usize = 64 * 1024 * 1024;
/// Maximum accepted width or height.
pub const MAX_IMAGE_DIMENSION: u32 = 8192;
/// Fixed byte length of an encoded echo marker.
pub const ECHO_MARKER_BYTES: usize = 24;

/// Directions permitted by the authoritative host policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardDirection {
    /// Permit both transfer directions.
    #[default]
    Both,
    /// Permit Deck-to-Pier transfers only.
    ClientToHost,
    /// Permit Pier-to-Deck transfers only.
    HostToClient,
    /// Disable clipboard redirection.
    Disabled,
}

/// Content kinds permitted by the authoritative host policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardContent {
    /// Permit text and PNG images.
    #[default]
    All,
    /// Permit UTF-8 text only.
    Text,
    /// Permit PNG images only.
    Image,
}

/// Direction of one clipboard transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardFlow {
    /// Deck to Pier.
    ClientToHost,
    /// Pier to Deck.
    HostToClient,
}

/// Normalized wire content kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardKind {
    /// UTF-8 text bytes.
    TextUtf8,
    /// PNG image bytes.
    ImagePng,
}

/// Validated host-authoritative clipboard policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardPolicy {
    /// Permitted direction.
    pub direction: ClipboardDirection,
    /// Permitted content.
    pub content: ClipboardContent,
    /// Maximum encoded transfer bytes.
    pub max_bytes: usize,
}

impl ClipboardPolicy {
    /// Constructs a policy with a transfer limit in `1..=20 MiB`.
    ///
    /// # Errors
    ///
    /// Returns [`ClipboardPolicyError::InvalidMaximum`] when `max_bytes` is
    /// outside the supported range.
    pub const fn new(
        direction: ClipboardDirection,
        content: ClipboardContent,
        max_bytes: usize,
    ) -> Result<Self, ClipboardPolicyError> {
        if max_bytes == 0 || max_bytes > HARD_MAX_CLIPBOARD_BYTES {
            return Err(ClipboardPolicyError::InvalidMaximum { max_bytes });
        }
        Ok(Self {
            direction,
            content,
            max_bytes,
        })
    }

    /// Returns whether the policy permits this direction and kind.
    #[must_use]
    pub const fn allows(self, flow: ClipboardFlow, kind: ClipboardKind) -> bool {
        let direction_allowed = matches!(
            (self.direction, flow),
            (ClipboardDirection::Both, _)
                | (
                    ClipboardDirection::ClientToHost,
                    ClipboardFlow::ClientToHost
                )
                | (
                    ClipboardDirection::HostToClient,
                    ClipboardFlow::HostToClient
                )
        );
        let content_allowed = matches!(
            (self.content, kind),
            (ClipboardContent::All, _)
                | (ClipboardContent::Text, ClipboardKind::TextUtf8)
                | (ClipboardContent::Image, ClipboardKind::ImagePng)
        );
        direction_allowed && content_allowed
    }

    /// Checks direction, content, and encoded size before a boundary crossing.
    ///
    /// # Errors
    ///
    /// Returns a bounded policy reason when the transfer is not permitted.
    pub const fn check_size(
        self,
        flow: ClipboardFlow,
        kind: ClipboardKind,
        bytes: usize,
    ) -> Result<(), ClipboardPolicyError> {
        if matches!(self.direction, ClipboardDirection::Disabled) {
            return Err(ClipboardPolicyError::Disabled);
        }
        if !matches!(
            (self.direction, flow),
            (ClipboardDirection::Both, _)
                | (
                    ClipboardDirection::ClientToHost,
                    ClipboardFlow::ClientToHost
                )
                | (
                    ClipboardDirection::HostToClient,
                    ClipboardFlow::HostToClient
                )
        ) {
            return Err(ClipboardPolicyError::DirectionNotAllowed);
        }
        if !matches!(
            (self.content, kind),
            (ClipboardContent::All, _)
                | (ClipboardContent::Text, ClipboardKind::TextUtf8)
                | (ClipboardContent::Image, ClipboardKind::ImagePng)
        ) {
            return Err(ClipboardPolicyError::ContentNotAllowed);
        }
        if bytes > self.max_bytes {
            return Err(ClipboardPolicyError::TooLarge {
                bytes,
                maximum: self.max_bytes,
            });
        }
        Ok(())
    }

    /// Borrows the largest valid UTF-8 prefix within the configured limit.
    ///
    /// Direction and content authorization remain explicit through
    /// [`Self::allows`] or [`Self::check_size`].
    #[must_use]
    pub fn prepare_text(self, _flow: ClipboardFlow, text: &str) -> ClipboardText<'_> {
        if text.len() <= self.max_bytes {
            return ClipboardText {
                text,
                truncated: false,
            };
        }
        let mut end = self.max_bytes;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        ClipboardText {
            text: &text[..end],
            truncated: true,
        }
    }
}

impl Default for ClipboardPolicy {
    fn default() -> Self {
        Self {
            direction: ClipboardDirection::Both,
            content: ClipboardContent::All,
            max_bytes: DEFAULT_CLIPBOARD_BYTES,
        }
    }
}

/// Borrowed policy-prepared text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipboardText<'a> {
    /// Borrowed UTF-8 prefix.
    pub text: &'a str,
    /// Whether bytes were removed.
    pub truncated: bool,
}

/// Clipboard policy validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardPolicyError {
    /// Configured maximum is outside `1..=20 MiB`.
    InvalidMaximum { max_bytes: usize },
    /// Clipboard redirection is disabled.
    Disabled,
    /// Transfer direction is disallowed.
    DirectionNotAllowed,
    /// Content kind is disallowed.
    ContentNotAllowed,
    /// Encoded transfer exceeds policy.
    TooLarge { bytes: usize, maximum: usize },
}

impl Display for ClipboardPolicyError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidMaximum { max_bytes } => {
                write!(formatter, "invalid clipboard maximum: {max_bytes}")
            }
            Self::Disabled => formatter.write_str("clipboard redirection disabled"),
            Self::DirectionNotAllowed => formatter.write_str("clipboard direction not allowed"),
            Self::ContentNotAllowed => formatter.write_str("clipboard content not allowed"),
            Self::TooLarge { bytes, maximum } => {
                write!(
                    formatter,
                    "clipboard item has {bytes} bytes; maximum is {maximum}"
                )
            }
        }
    }
}

impl Error for ClipboardPolicyError {}

/// Monotonic nonzero sequence gate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClipboardSequenceGate {
    last: u64,
}

impl ClipboardSequenceGate {
    /// Accepts only a nonzero sequence newer than every previously accepted one.
    pub const fn accept(&mut self, sequence: u64) -> bool {
        if sequence == 0 || sequence <= self.last {
            return false;
        }
        self.last = sequence;
        true
    }

    /// Returns the latest accepted sequence, or zero before the first acceptance.
    #[must_use]
    pub const fn last(self) -> u64 {
        self.last
    }
}

/// Session-scoped loop-suppression token.
///
/// This value is not a credential and provides no authentication. Its `Debug`
/// output is always redacted to prevent accidental logging.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct EchoToken(pub [u8; 16]);

impl Debug for EchoToken {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("EchoToken([redacted])")
    }
}

/// Fixed endpoint marker attached to an injected clipboard item.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct EchoMarker {
    /// Session token.
    pub token: EchoToken,
    /// Nonzero injected sequence.
    pub sequence: u64,
}

impl EchoMarker {
    /// Encodes the fixed 24-byte token-plus-big-endian-sequence marker.
    #[must_use]
    pub fn encode(self) -> [u8; ECHO_MARKER_BYTES] {
        let mut encoded = [0u8; ECHO_MARKER_BYTES];
        encoded[..16].copy_from_slice(&self.token.0);
        encoded[16..].copy_from_slice(&self.sequence.to_be_bytes());
        encoded
    }

    /// Decodes a fixed marker and rejects zero sequences.
    #[must_use]
    pub fn decode(encoded: &[u8]) -> Option<Self> {
        if encoded.len() != ECHO_MARKER_BYTES {
            return None;
        }
        let mut token = [0u8; 16];
        token.copy_from_slice(&encoded[..16]);
        let sequence = u64::from_be_bytes(encoded[16..].try_into().ok()?);
        (sequence != 0).then_some(Self {
            token: EchoToken(token),
            sequence,
        })
    }
}

impl Debug for EchoMarker {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EchoMarker")
            .field("token", &"[redacted]")
            .field("sequence", &self.sequence)
            .finish()
    }
}

/// Tracks the one local injection eligible for echo suppression.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct EchoSuppressor {
    token: EchoToken,
    injected_sequence: Option<u64>,
}

impl EchoSuppressor {
    /// Creates a suppressor for one session.
    #[must_use]
    pub const fn new(token: EchoToken) -> Self {
        Self {
            token,
            injected_sequence: None,
        }
    }

    /// Records a nonzero injected sequence and returns its marker.
    #[must_use]
    pub fn mark_injected(&mut self, sequence: u64) -> Option<EchoMarker> {
        if sequence == 0 {
            return None;
        }
        self.injected_sequence = Some(sequence);
        Some(EchoMarker {
            token: self.token,
            sequence,
        })
    }

    /// Returns true only for this session's currently injected item.
    #[must_use]
    pub fn should_suppress(self, marker: EchoMarker) -> bool {
        marker.token == self.token && self.injected_sequence == Some(marker.sequence)
    }

    /// Clears suppression state when ownership changes or the session ends.
    pub fn clear(&mut self) {
        self.injected_sequence = None;
    }
}

impl Debug for EchoSuppressor {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EchoSuppressor")
            .field("token", &"[redacted]")
            .field("injected_sequence", &self.injected_sequence)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_matrix_is_exact() {
        let directions = [
            ClipboardDirection::Both,
            ClipboardDirection::ClientToHost,
            ClipboardDirection::HostToClient,
            ClipboardDirection::Disabled,
        ];
        let contents = [
            ClipboardContent::All,
            ClipboardContent::Text,
            ClipboardContent::Image,
        ];
        let flows = [ClipboardFlow::ClientToHost, ClipboardFlow::HostToClient];
        let kinds = [ClipboardKind::TextUtf8, ClipboardKind::ImagePng];

        for direction in directions {
            for content in contents {
                let policy = ClipboardPolicy::new(direction, content, 64).expect("valid policy");
                for flow in flows {
                    for kind in kinds {
                        let direction_allowed = matches!(
                            (direction, flow),
                            (ClipboardDirection::Both, _)
                                | (
                                    ClipboardDirection::ClientToHost,
                                    ClipboardFlow::ClientToHost
                                )
                                | (
                                    ClipboardDirection::HostToClient,
                                    ClipboardFlow::HostToClient
                                )
                        );
                        let content_allowed = matches!(
                            (content, kind),
                            (ClipboardContent::All, _)
                                | (ClipboardContent::Text, ClipboardKind::TextUtf8)
                                | (ClipboardContent::Image, ClipboardKind::ImagePng)
                        );
                        assert_eq!(
                            policy.allows(flow, kind),
                            direction_allowed && content_allowed
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn policy_limits_and_utf8_prefix_are_bounded() {
        assert_eq!(
            ClipboardPolicy::new(ClipboardDirection::Both, ClipboardContent::All, 0),
            Err(ClipboardPolicyError::InvalidMaximum { max_bytes: 0 })
        );
        assert_eq!(
            ClipboardPolicy::new(
                ClipboardDirection::Both,
                ClipboardContent::All,
                HARD_MAX_CLIPBOARD_BYTES + 1
            ),
            Err(ClipboardPolicyError::InvalidMaximum {
                max_bytes: HARD_MAX_CLIPBOARD_BYTES + 1
            })
        );
        let policy = ClipboardPolicy::new(ClipboardDirection::Both, ClipboardContent::Text, 4)
            .expect("valid policy");
        assert_eq!(
            policy.prepare_text(ClipboardFlow::ClientToHost, "a\u{20ac}z"),
            ClipboardText {
                text: "a\u{20ac}",
                truncated: true
            }
        );
        assert_eq!(
            policy.prepare_text(ClipboardFlow::HostToClient, "abcd"),
            ClipboardText {
                text: "abcd",
                truncated: false
            }
        );
    }

    #[test]
    fn sequence_and_echo_are_latest_only() {
        let mut gate = ClipboardSequenceGate::default();
        assert!(!gate.accept(0));
        assert!(gate.accept(4));
        assert!(!gate.accept(4));
        assert!(!gate.accept(3));
        assert!(gate.accept(5));

        let token = EchoToken([7; 16]);
        let other = EchoToken([8; 16]);
        let mut suppressor = EchoSuppressor::new(token);
        let marker = suppressor.mark_injected(9).expect("nonzero sequence");
        assert_eq!(EchoMarker::decode(&marker.encode()), Some(marker));
        assert!(suppressor.should_suppress(marker));
        assert!(!suppressor.should_suppress(EchoMarker {
            token: other,
            sequence: 9
        }));
        assert!(!suppressor.should_suppress(EchoMarker { token, sequence: 8 }));
        suppressor.clear();
        assert!(!suppressor.should_suppress(marker));
        assert_eq!(format!("{token:?}"), "EchoToken([redacted])");
        assert!(!format!("{marker:?}").contains("7, 7"));
    }
}
