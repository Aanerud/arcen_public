// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! A bounded big-endian reader shared by the binary record decoders
//! (`refine`, `canvas`). Every read is checked; nothing here allocates.

/// The record ended before a read could complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Short;

/// Reads fixed-width big-endian fields from a borrowed record.
pub(crate) struct ByteReader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> ByteReader<'a> {
    pub(crate) const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    pub(crate) fn take<const N: usize>(&mut self) -> Result<[u8; N], Short> {
        let slice = self.slice(N)?;
        let mut out = [0_u8; N];
        out.copy_from_slice(slice);
        Ok(out)
    }

    /// Borrows the next `len` bytes.
    pub(crate) fn slice(&mut self, len: usize) -> Result<&'a [u8], Short> {
        let end = self.at.checked_add(len).ok_or(Short)?;
        let slice = self.bytes.get(self.at..end).ok_or(Short)?;
        self.at = end;
        Ok(slice)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, Short> {
        Ok(self.take::<1>()?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, Short> {
        Ok(u16::from_be_bytes(self.take()?))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, Short> {
        Ok(u32::from_be_bytes(self.take()?))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, Short> {
        Ok(u64::from_be_bytes(self.take()?))
    }

    /// The bytes not read yet.
    pub(crate) fn rest(&self) -> &'a [u8] {
        self.bytes.get(self.at..).unwrap_or_default()
    }

    /// How many bytes are not read yet.
    pub(crate) fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.at)
    }
}
