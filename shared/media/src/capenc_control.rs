// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
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
    /// Lossless refinement: the Deck asked to start over (`refine_reset`);
    /// the producer starts a new generation behind a barrier.
    RefineReset,
    /// Lossless refinement: the Deck turned it off for the monitor
    /// (`refine_control`, `enabled: false`); fences continue, tiles stop.
    RefinePause,
    /// Lossless refinement: the Deck turned it on again; a new generation.
    RefineResume,
    /// Lossless refinement: the rate the Pier's link budget allows
    /// (`arcen_media::refine::budget::RefineLinkBudget`, ADR 0016 R6). It
    /// replaces the video-target floor from then on; `hold` freezes the
    /// bucket and cancels queued tiles until the next `REFINE-RATE`.
    RefineRate { bps: u64, hold: bool },
    /// Canvas (ADR 0028): a Deck's `canvas_ack`, relayed after the Pier
    /// validated it.
    CanvasAck {
        monitor_id: u16,
        generation: u32,
        fence: u64,
        unprocessed_bytes: u64,
        decode_ms_p95: u32,
    },
    /// Canvas: a Deck's `canvas_reset_request`; the loop answers with a
    /// reset (reason `reset request`).
    CanvasReset { monitor_id: u16, generation: u32 },
    /// Canvas: input the Pier applied at `(x, y)` of monitor `monitor_id`
    /// in topology generation `topology_generation`, in 1/65536ths of the
    /// picture's width and height (the loop knows its own size; the Pier
    /// may not); its neighbourhood is never deferred. A loop serving
    /// another monitor or generation ignores it. Built by
    /// [`crate::canvas::serve::CanvasInputFeed`]; carries no input content.
    CanvasInput {
        monitor_id: u16,
        topology_generation: u64,
        x: u16,
        y: u16,
    },
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
            Self::RefineReset => "REFINE-RESET".to_owned(),
            Self::RefinePause => "REFINE-PAUSE".to_owned(),
            Self::RefineResume => "REFINE-RESUME".to_owned(),
            Self::RefineRate { bps, hold: false } => format!("REFINE-RATE {bps}"),
            Self::RefineRate { bps, hold: true } => format!("REFINE-RATE {bps} HOLD"),
            Self::CanvasAck {
                monitor_id,
                generation,
                fence,
                unprocessed_bytes,
                decode_ms_p95,
            } => format!(
                "CANVAS-ACK {monitor_id} {generation} {fence} {unprocessed_bytes} {decode_ms_p95}"
            ),
            Self::CanvasReset {
                monitor_id,
                generation,
            } => format!("CANVAS-RESET {monitor_id} {generation}"),
            Self::CanvasInput {
                monitor_id,
                topology_generation,
                x,
                y,
            } => format!("CANVAS-INPUT {monitor_id} {topology_generation} {x} {y}"),
        }
    }

    /// The relay line for a Deck's canvas control message, or `None` for
    /// one that fails `CanvasControlMsg::validate`.
    #[must_use]
    pub fn from_canvas_control(message: &arcen_protocol::canvas::CanvasControlMsg) -> Option<Self> {
        use arcen_protocol::canvas::CanvasControlMsg;
        message.validate().ok()?;
        Some(match *message {
            CanvasControlMsg::CanvasAck {
                monitor_id,
                generation,
                fence,
                unprocessed_bytes,
                decode_ms_p95,
            } => Self::CanvasAck {
                monitor_id,
                generation,
                fence,
                unprocessed_bytes,
                decode_ms_p95,
            },
            CanvasControlMsg::CanvasResetRequest {
                monitor_id,
                generation,
                ..
            } => Self::CanvasReset {
                monitor_id,
                generation,
            },
        })
    }

    /// The canvas control message a relayed line stands for.
    #[must_use]
    pub fn canvas_control(self) -> Option<arcen_protocol::canvas::CanvasControlMsg> {
        use arcen_protocol::canvas::CanvasControlMsg;
        match self {
            Self::CanvasAck {
                monitor_id,
                generation,
                fence,
                unprocessed_bytes,
                decode_ms_p95,
            } => Some(CanvasControlMsg::CanvasAck {
                monitor_id,
                generation,
                fence,
                unprocessed_bytes,
                decode_ms_p95,
            }),
            Self::CanvasReset {
                monitor_id,
                generation,
            } => Some(CanvasControlMsg::CanvasResetRequest {
                monitor_id,
                generation,
                reason: "deck".to_owned(),
            }),
            _ => None,
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
    InvalidRefineRate(String),
    InvalidCanvas(String),
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
            Self::InvalidCanvas(line) => write!(formatter, "invalid canvas control line {line:?}"),
            Self::InvalidRefineRate(line) => write!(
                formatter,
                "REFINE-RATE takes bits per second and an optional HOLD: {line:?}"
            ),
        }
    }
}

impl std::error::Error for CapencControlParseError {}

impl FromStr for CapencControlCommand {
    type Err = CapencControlParseError;

    #[allow(clippy::too_many_lines)]
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
        if command.eq_ignore_ascii_case("REFINE-RESET") {
            return Ok(Self::RefineReset);
        }
        if command.eq_ignore_ascii_case("REFINE-PAUSE") {
            return Ok(Self::RefinePause);
        }
        if command.eq_ignore_ascii_case("REFINE-RESUME") {
            return Ok(Self::RefineResume);
        }
        if command.eq_ignore_ascii_case("CANVAS-ACK")
            || command.eq_ignore_ascii_case("CANVAS-RESET")
            || command.eq_ignore_ascii_case("CANVAS-INPUT")
        {
            let invalid = || CapencControlParseError::InvalidCanvas(trimmed.to_owned());
            let values: Vec<u64> = parts
                .map(|value| value.parse::<u64>().map_err(|_| invalid()))
                .collect::<Result<_, _>>()?;
            let narrow16 = |value: u64| u16::try_from(value).map_err(|_| invalid());
            let narrow32 = |value: u64| u32::try_from(value).map_err(|_| invalid());
            return match (command.to_ascii_uppercase().as_str(), values.as_slice()) {
                ("CANVAS-ACK", [monitor, generation, fence, unprocessed, decode]) => {
                    Ok(Self::CanvasAck {
                        monitor_id: narrow16(*monitor)?,
                        generation: narrow32(*generation)?,
                        fence: *fence,
                        unprocessed_bytes: *unprocessed,
                        decode_ms_p95: narrow32(*decode)?,
                    })
                }
                ("CANVAS-RESET", [monitor, generation]) => Ok(Self::CanvasReset {
                    monitor_id: narrow16(*monitor)?,
                    generation: narrow32(*generation)?,
                }),
                ("CANVAS-INPUT", [monitor, topology_generation, x, y]) => Ok(Self::CanvasInput {
                    monitor_id: narrow16(*monitor)?,
                    topology_generation: *topology_generation,
                    x: narrow16(*x)?,
                    y: narrow16(*y)?,
                }),
                _ => Err(invalid()),
            };
        }
        if command.eq_ignore_ascii_case("REFINE-RATE") {
            let invalid = || CapencControlParseError::InvalidRefineRate(trimmed.to_owned());
            let bps = parts
                .next()
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(invalid)?;
            let hold = match parts.next() {
                None => false,
                Some(flag) if flag.eq_ignore_ascii_case("HOLD") => true,
                Some(_) => return Err(invalid()),
            };
            if parts.next().is_some() {
                return Err(invalid());
            }
            return Ok(Self::RefineRate { bps, hold });
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

/// Canvas control lines a capenc keeps between two encode-loop ticks: at
/// most this many acknowledgements and reset requests.
pub const MAX_CANVAS_CONTROLS: usize = 256;

/// Canvas control lines waiting for the encode loop (ADR 0028): the Deck's
/// acknowledgements and reset requests in arrival order, bounded at
/// [`MAX_CANVAS_CONTROLS`], and only the newest input position, so input
/// can never grow the inbox or push out a reset request. When full, the
/// oldest acknowledgement goes first (a later one supersedes it); a reset
/// request goes only when nothing but reset requests is left (each asks for
/// the same thing).
#[derive(Debug, Default)]
pub struct CanvasControlInbox {
    ordered: std::collections::VecDeque<CapencControlCommand>,
    input: Option<CapencControlCommand>,
}

impl CanvasControlInbox {
    /// Keeps one canvas line; any other command is ignored.
    pub fn push(&mut self, command: CapencControlCommand) {
        match command {
            CapencControlCommand::CanvasInput { .. } => self.input = Some(command),
            CapencControlCommand::CanvasAck { .. } | CapencControlCommand::CanvasReset { .. } => {
                if self.ordered.len() >= MAX_CANVAS_CONTROLS {
                    let ack = self.ordered.iter().position(|queued| {
                        matches!(queued, CapencControlCommand::CanvasAck { .. })
                    });
                    self.ordered.remove(ack.unwrap_or(0));
                }
                self.ordered.push_back(command);
            }
            _ => {}
        }
    }

    /// Everything kept since the last call: the ordered lines, then the
    /// newest input position.
    pub fn take(&mut self) -> Vec<CapencControlCommand> {
        let mut taken: Vec<_> = self.ordered.drain(..).collect();
        taken.extend(self.input.take());
        taken
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flood of input positions while the loop is busy keeps one, the
    /// newest, and never pushes a reset request out; a full inbox gives up
    /// acknowledgements first.
    #[test]
    fn input_never_displaces_a_reset_request() {
        let mut inbox = CanvasControlInbox::default();
        let reset = CapencControlCommand::CanvasReset {
            monitor_id: 0,
            generation: 4,
        };
        inbox.push(reset);
        for x in 0..10_000_u16 {
            inbox.push(CapencControlCommand::CanvasInput {
                monitor_id: 0,
                topology_generation: 0,
                x,
                y: 7,
            });
        }
        for fence in 0..(MAX_CANVAS_CONTROLS as u64 * 2) {
            inbox.push(CapencControlCommand::CanvasAck {
                monitor_id: 0,
                generation: 4,
                fence,
                unprocessed_bytes: 0,
                decode_ms_p95: 0,
            });
        }
        let taken = inbox.take();
        assert_eq!(taken.len(), MAX_CANVAS_CONTROLS + 1);
        assert_eq!(taken[0], reset, "the reset request survived");
        assert_eq!(
            taken.last(),
            Some(&CapencControlCommand::CanvasInput {
                monitor_id: 0,
                topology_generation: 0,
                x: 9_999,
                y: 7,
            }),
            "only the newest input position"
        );
        assert!(inbox.take().is_empty());
    }

    #[test]
    fn canvas_lines_round_trip_and_bad_ones_are_refused() {
        use arcen_protocol::canvas::CanvasControlMsg;
        for command in [
            CapencControlCommand::CanvasAck {
                monitor_id: 0,
                generation: 3,
                fence: 99,
                unprocessed_bytes: 4096,
                decode_ms_p95: 2,
            },
            CapencControlCommand::CanvasReset {
                monitor_id: 1,
                generation: 2,
            },
            CapencControlCommand::CanvasInput {
                monitor_id: 0,
                topology_generation: 0,
                x: 10,
                y: 65_535,
            },
        ] {
            assert_eq!(command.as_line().parse(), Ok(command));
        }
        for bad in [
            "CANVAS-ACK 1 2 3 4",
            "CANVAS-ACK 70000 2 3 4 5",
            "CANVAS-RESET 1",
            "CANVAS-RESET 1 x",
            "CANVAS-INPUT 1 2 3",
            "CANVAS-INPUT 10 20",
            "CANVAS-INPUT 0 0 65536 1",
        ] {
            assert!(bad.parse::<CapencControlCommand>().is_err(), "{bad}");
        }
        let ack = CanvasControlMsg::CanvasAck {
            monitor_id: 0,
            generation: 1,
            fence: 5,
            unprocessed_bytes: 0,
            decode_ms_p95: 1,
        };
        let relayed = CapencControlCommand::from_canvas_control(&ack).unwrap();
        assert_eq!(relayed.canvas_control(), Some(ack));
        let bad_reason = CanvasControlMsg::CanvasResetRequest {
            monitor_id: 0,
            generation: 1,
            reason: "NOT A TOKEN".to_owned(),
        };
        assert_eq!(CapencControlCommand::from_canvas_control(&bad_reason), None);
    }

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
        for command in [
            CapencControlCommand::RefineReset,
            CapencControlCommand::RefinePause,
            CapencControlCommand::RefineResume,
            CapencControlCommand::RefineRate {
                bps: 12_000_000,
                hold: false,
            },
            CapencControlCommand::RefineRate { bps: 0, hold: true },
        ] {
            assert_eq!(command.as_line().parse(), Ok(command));
        }
        for refused in [
            "REFINE-RATE",
            "REFINE-RATE x",
            "REFINE-RATE 1 SOON",
            "REFINE-RATE 1 HOLD 2",
        ] {
            assert!(matches!(
                refused.parse::<CapencControlCommand>(),
                Err(CapencControlParseError::InvalidRefineRate(_))
            ));
        }
    }

    #[test]
    fn formats_commands_for_stdin() {
        assert_eq!(
            CapencControlCommand::Bitrate { bps: 123 }.as_wire_line(),
            "BITRATE 123\n"
        );
    }
}
