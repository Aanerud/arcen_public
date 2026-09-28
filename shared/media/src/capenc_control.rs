//! Shared control lines sent by Piers to the capenc helper.

use std::fmt::{Display, Formatter};
use std::str::FromStr;

/// A single newline-delimited capenc control command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapencControlCommand {
    /// Force an IDR/keyframe.
    Idr,
    /// Wake a static region after local input/focus activity.
    Wake,
    /// Stop the helper gracefully.
    Stop,
    /// Apply a new target average bitrate live.
    Bitrate { bps: u64 },
    /// Apply a new capture/encode frame-rate cap live.
    Framerate { fps: u32 },
}

impl CapencControlCommand {
    /// Formats this command without the trailing newline.
    #[must_use]
    pub fn as_line(self) -> String {
        match self {
            Self::Idr => "IDR".to_owned(),
            Self::Wake => "WAKE".to_owned(),
            Self::Stop => "STOP".to_owned(),
            Self::Bitrate { bps } => format!("BITRATE {bps}"),
            Self::Framerate { fps } => format!("FRAMERATE {fps}"),
        }
    }

    /// Formats this command with the trailing newline expected on stdin.
    #[must_use]
    pub fn as_wire_line(self) -> String {
        let mut line = self.as_line();
        line.push('\n');
        line
    }
}

/// Why a capenc control line was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapencControlParseError {
    Empty,
    Unknown(String),
    MissingBitrate,
    InvalidBitrate(String),
    ZeroBitrate,
    MissingFramerate,
    InvalidFramerate(String),
    ZeroFramerate,
}

impl Display for CapencControlParseError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => formatter.write_str("empty control line"),
            Self::Unknown(command) => write!(formatter, "unknown control command {command:?}"),
            Self::MissingBitrate => formatter.write_str("BITRATE requires a bits-per-second value"),
            Self::InvalidBitrate(value) => write!(formatter, "invalid BITRATE value {value:?}"),
            Self::ZeroBitrate => formatter.write_str("BITRATE must be non-zero"),
            Self::MissingFramerate => formatter.write_str("FRAMERATE requires an fps value"),
            Self::InvalidFramerate(value) => write!(formatter, "invalid FRAMERATE value {value:?}"),
            Self::ZeroFramerate => formatter.write_str("FRAMERATE must be non-zero"),
        }
    }
}

impl std::error::Error for CapencControlParseError {}

impl FromStr for CapencControlCommand {
    type Err = CapencControlParseError;

    fn from_str(line: &str) -> Result<Self, Self::Err> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Err(CapencControlParseError::Empty);
        }
        let mut parts = trimmed.split_whitespace();
        let Some(command) = parts.next() else {
            return Err(CapencControlParseError::Empty);
        };
        if command.eq_ignore_ascii_case("IDR") {
            return Ok(Self::Idr);
        }
        if command.eq_ignore_ascii_case("WAKE") {
            return Ok(Self::Wake);
        }
        if command.eq_ignore_ascii_case("STOP") {
            return Ok(Self::Stop);
        }
        if command.eq_ignore_ascii_case("FRAMERATE") {
            let Some(value) = parts.next() else {
                return Err(CapencControlParseError::MissingFramerate);
            };
            if parts.next().is_some() {
                return Err(CapencControlParseError::InvalidFramerate(
                    trimmed.to_owned(),
                ));
            }
            let fps = value
                .parse::<u32>()
                .map_err(|_| CapencControlParseError::InvalidFramerate(value.to_owned()))?;
            if fps == 0 {
                return Err(CapencControlParseError::ZeroFramerate);
            }
            return Ok(Self::Framerate { fps });
        }
        if command.eq_ignore_ascii_case("BITRATE") {
            let Some(value) = parts.next() else {
                return Err(CapencControlParseError::MissingBitrate);
            };
            if parts.next().is_some() {
                return Err(CapencControlParseError::InvalidBitrate(trimmed.to_owned()));
            }
            let bps = value
                .parse::<u64>()
                .map_err(|_| CapencControlParseError::InvalidBitrate(value.to_owned()))?;
            if bps == 0 {
                return Err(CapencControlParseError::ZeroBitrate);
            }
            return Ok(Self::Bitrate { bps });
        }
        Err(CapencControlParseError::Unknown(command.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_legacy_and_bitrate_commands() {
        assert_eq!("idr".parse(), Ok(CapencControlCommand::Idr));
        assert_eq!("WAKE".parse(), Ok(CapencControlCommand::Wake));
        assert_eq!("stop".parse(), Ok(CapencControlCommand::Stop));
        assert_eq!(
            "BITRATE 6000000".parse(),
            Ok(CapencControlCommand::Bitrate { bps: 6_000_000 })
        );
        assert_eq!(
            "FRAMERATE 24".parse(),
            Ok(CapencControlCommand::Framerate { fps: 24 })
        );
    }

    #[test]
    fn formats_commands_for_stdin() {
        assert_eq!(
            CapencControlCommand::Bitrate { bps: 123 }.as_wire_line(),
            "BITRATE 123\n"
        );
    }
}
