// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The contents of a `--password-file`: one Deck rule for every platform.
//!
//! Each Deck opens the file its own way (the macOS Deck refuses a symbolic
//! link, another owner or a mode broader than `0600`; the Windows Deck reads
//! the same promise from the file's security descriptor) and then hands the
//! open file here. The password is the file's text without one trailing line
//! ending; it must be non-empty UTF-8 without a NUL byte and at most
//! [`MAX_PASSWORD_FILE_SIZE`] bytes. Every rejected buffer is zeroed.

use std::io::{self, Read};

use zeroize::Zeroize;

/// The largest password file a Deck reads.
pub const MAX_PASSWORD_FILE_SIZE: usize = 8 * 1024;

/// Why a password file's contents were refused.
#[derive(Debug)]
pub enum PasswordTextError {
    /// The file could not be read.
    Read(io::Error),
    /// Longer than [`MAX_PASSWORD_FILE_SIZE`].
    TooLarge {
        /// The limit, in bytes.
        limit: usize,
    },
    /// Nothing but a line ending.
    Empty,
    /// A NUL byte.
    ContainsNul,
    /// Not UTF-8.
    InvalidUtf8,
}

impl std::fmt::Display for PasswordTextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(error) => write!(f, "cannot read password file: {error}"),
            Self::TooLarge { limit } => {
                write!(f, "password file exceeds the {limit}-byte limit")
            }
            Self::Empty => f.write_str("password file is empty"),
            Self::ContainsNul => f.write_str("password file contains a NUL byte"),
            Self::InvalidUtf8 => f.write_str("password file must contain valid UTF-8"),
        }
    }
}

impl std::error::Error for PasswordTextError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            _ => None,
        }
    }
}

/// Reads the password from an opened password file.
///
/// # Errors
///
/// When the contents break a rule in the module documentation.
pub fn read_password_text(file: impl Read) -> Result<String, PasswordTextError> {
    let mut bytes = Vec::with_capacity(MAX_PASSWORD_FILE_SIZE + 1);
    if let Err(source) = file
        .take((MAX_PASSWORD_FILE_SIZE + 1) as u64)
        .read_to_end(&mut bytes)
    {
        bytes.zeroize();
        return Err(PasswordTextError::Read(source));
    }
    if bytes.len() > MAX_PASSWORD_FILE_SIZE {
        bytes.zeroize();
        return Err(PasswordTextError::TooLarge {
            limit: MAX_PASSWORD_FILE_SIZE,
        });
    }
    strip_one_line_ending(&mut bytes);
    if bytes.is_empty() {
        return Err(PasswordTextError::Empty);
    }
    if bytes.contains(&0) {
        bytes.zeroize();
        return Err(PasswordTextError::ContainsNul);
    }
    String::from_utf8(bytes).map_err(|error| {
        let mut bytes = error.into_bytes();
        bytes.zeroize();
        PasswordTextError::InvalidUtf8
    })
}

/// Removes one trailing `\n`, `\r\n` or `\r`.
pub fn strip_one_line_ending(bytes: &mut Vec<u8>) {
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    } else if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(bytes: &[u8]) -> Result<String, PasswordTextError> {
        read_password_text(bytes)
    }

    #[test]
    fn exactly_one_line_ending_is_removed() {
        assert_eq!(read(b"secret\r\n").unwrap(), "secret");
        assert_eq!(read(b"secret\n").unwrap(), "secret");
        assert_eq!(read(b"secret\r").unwrap(), "secret");
        assert_eq!(read(b"secret\n\n").unwrap(), "secret\n");
        assert_eq!(read(b" spaced secret ").unwrap(), " spaced secret ");
    }

    #[test]
    fn empty_nul_non_utf8_and_oversized_contents_are_refused() {
        assert!(matches!(read(b"\r\n"), Err(PasswordTextError::Empty)));
        assert!(matches!(read(b""), Err(PasswordTextError::Empty)));
        assert!(matches!(
            read(b"se\0cret"),
            Err(PasswordTextError::ContainsNul)
        ));
        assert!(matches!(
            read(&[0xff, 0xfe]),
            Err(PasswordTextError::InvalidUtf8)
        ));
        assert!(read(&vec![b'x'; MAX_PASSWORD_FILE_SIZE]).is_ok());
        assert!(matches!(
            read(&vec![b'x'; MAX_PASSWORD_FILE_SIZE + 1]),
            Err(PasswordTextError::TooLarge { limit }) if limit == MAX_PASSWORD_FILE_SIZE
        ));
    }
}
