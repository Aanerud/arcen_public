// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! H.264 and H.265 bitstream syntax: reading, writing and synthesising RBSP.
//!
//! Everything here is portable and reads untrusted peer input: every read is
//! bounds-checked, values are range-checked against the specification, and a
//! failure is a [`BitstreamError`], never a panic.

// Field names follow the specifications (`pic_num`/`pic_nums`, `sps`/`pps`).
#![allow(clippy::similar_names)]

pub mod h264;
pub mod hevc;
pub mod picture;
pub mod reader;
pub mod synth;
pub mod writer;

pub use reader::{BitReader, unescape_rbsp};
pub use writer::{BitWriter, escape_emulation_prevention};

use std::fmt::{Display, Formatter};

/// Why a unit of a bitstream could not be read or used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitstreamError {
    /// The unit ended before a field the syntax requires.
    Truncated,
    /// A field held a value the specification does not allow.
    Invalid(&'static str),
    /// Valid syntax this decoder deliberately does not handle (for example
    /// interlaced coding); Arcen Piers never emit it.
    Unsupported(&'static str),
    /// A slice or parameter set named a parameter set that has not arrived.
    MissingParameterSet { kind: &'static str, id: u32 },
    /// A picture references a picture the decoded picture buffer does not
    /// hold (lost or never sent). Never guessed: the caller asks for a
    /// keyframe.
    MissingReference,
    /// No free picture slot was available for the current picture.
    NoFreeSlot,
}

impl Display for BitstreamError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => formatter.write_str("bitstream unit ended early"),
            Self::Invalid(field) => write!(formatter, "bitstream field {field} is out of range"),
            Self::Unsupported(what) => write!(formatter, "unsupported bitstream feature: {what}"),
            Self::MissingParameterSet { kind, id } => {
                write!(formatter, "{kind} {id} has not been received")
            }
            Self::MissingReference => formatter
                .write_str("a reference picture is missing from the decoded picture buffer"),
            Self::NoFreeSlot => formatter.write_str("no free decoded-picture slot"),
        }
    }
}

impl std::error::Error for BitstreamError {}
