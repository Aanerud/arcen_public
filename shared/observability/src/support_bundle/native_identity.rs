// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! What the operating system says about a source path: its folders, opened
//! one at a time without following a link; the file as inspected; and the
//! file as opened. Every decision made from these facts
//! ([`super::check_source`], [`super::verify_same_source`],
//! [`super::link_is_trusted`]) is portable and lives in `source`.

use std::fs::File;
use std::path::PathBuf;

use super::source::{LinkFacts, SourceFacts, VolumeFacts};

#[cfg(unix)]
mod unix;
#[cfg(not(any(unix, windows)))]
mod unsupported;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub(crate) use unix::{Folder, child, finish, inspect, location, open, open_root};
#[cfg(not(any(unix, windows)))]
pub(crate) use unsupported::{Folder, child, finish, inspect, location, open, open_root};
#[cfg(windows)]
pub(crate) use windows::{Folder, child, finish, inspect, location, open, open_root};

/// One entry of a folder on the way to a source.
pub(crate) enum Child {
    /// A folder (not a link), opened.
    Directory(Folder),
    /// A link or reparse point, not followed.
    Link(LinkFacts),
}

/// A source as inspected before it is opened for reading.
pub(crate) struct Inspection {
    /// What it is.
    pub(crate) facts: SourceFacts,
    /// Windows: the handle the inspection read through. Held until the
    /// identities are compared, so the inspected file stays allocated and
    /// its file ID cannot pass to a file created in its place.
    pub(crate) _held: Option<File>,
}

/// A source opened for reading.
pub(crate) struct OpenedLeaf {
    /// The file (Unix: still non-blocking until [`finish`]).
    pub(crate) file: File,
    /// What it is, from the open handle.
    pub(crate) facts: SourceFacts,
    /// What its volume says about file identities.
    pub(crate) volume: VolumeFacts,
    /// Windows: its real location, from the handle (`GetFinalPathNameByHandleW`).
    pub(crate) location: Option<PathBuf>,
}
