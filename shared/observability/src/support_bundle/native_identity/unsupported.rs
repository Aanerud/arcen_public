// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! A platform with neither Unix nor Windows file facts cannot prove which
//! file it opened, so it opens no source.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::path::Path;

use super::{Child, Inspection, OpenedLeaf};

/// Never constructed: [`open_root`] always fails.
pub(crate) struct Folder;

fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "file identities are unavailable on this platform",
    )
}

pub(crate) fn open_root(_root: Option<&Path>) -> io::Result<Folder> {
    Err(unsupported())
}

pub(crate) fn child(_parent: &Folder, _name: &OsStr) -> io::Result<Child> {
    Err(unsupported())
}

pub(crate) fn location(_folder: &Folder) -> Option<&Path> {
    None
}

pub(crate) fn inspect(_parent: &Folder, _name: &OsStr) -> io::Result<Inspection> {
    Err(unsupported())
}

pub(crate) fn open(_parent: &Folder, _name: &OsStr) -> io::Result<Option<OpenedLeaf>> {
    Err(unsupported())
}

pub(crate) fn finish(_file: &File) -> io::Result<()> {
    Err(unsupported())
}
