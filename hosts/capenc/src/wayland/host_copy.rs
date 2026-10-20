//! The `host-copy` import route: shared-memory BGRx/BGRA into the NVENC
//! CUDA input slot.
//!
//! This is the route GNOME 40 provides (Mutter offers only `MemFd` BGRx when
//! it renders surfaceless). Mutter has already read the frame back into
//! host memory; capenc copies its rows once host-to-device into the current
//! NVENC slot, dropping row padding, and NVENC converts BGRA to YCbCr exactly
//! as it does for the NvFBC eight-bit path. The lease is held for the
//! duration of the synchronous copy and released by the caller afterwards.

use std::time::{Duration, Instant};

use arcen_media::video::{CaptureMemory, CapturePixelFormat, CopyPath};

use super::pipewire::{BufferToken, LeaseRevoked, PlaneView};
use arcen_media::video::CaptureLease;

/// Why a frame was not staged. The encoder slot was not written, so the
/// caller must not encode it (no stale pixels).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportError {
    /// The buffer was removed (renegotiation or shutdown) after leasing.
    Revoked,
    /// The frame is not in host memory.
    NotHostMemory,
    /// The importer has no staging for this format.
    Format(CapturePixelFormat),
    /// The plane is not mapped.
    Unmapped,
    /// The encoder refused or CUDA failed.
    Stage(String),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Revoked => write!(formatter, "lease revoked before import"),
            Self::NotHostMemory => write!(formatter, "frame is not in host memory"),
            Self::Format(format) => write!(formatter, "no host staging for {}", format.token()),
            Self::Unmapped => write!(formatter, "host plane is not mapped"),
            Self::Stage(detail) => write!(formatter, "staging failed: {detail}"),
        }
    }
}

impl std::error::Error for ImportError {}

impl From<LeaseRevoked> for ImportError {
    fn from(_: LeaseRevoked) -> Self {
        Self::Revoked
    }
}

/// What one import cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportReport {
    /// Truthful copy path.
    pub copy_path: CopyPath,
    /// Bytes read from the producer buffer.
    pub bytes: usize,
    /// Time spent copying.
    pub elapsed: Duration,
}

/// Something that accepts one host-memory BGRA frame. Implemented by the
/// CUDA/NVENC encoder; tests and the harness may supply their own sink.
pub trait HostBgraSink {
    /// Copies `height` rows of `row_bytes` from `rows` (rows `stride` apart).
    ///
    /// # Errors
    ///
    /// A description of the failure.
    fn stage_host_bgra(&mut self, rows: &[u8], stride: usize) -> Result<(), String>;
}

/// The `host-copy` importer.
#[derive(Debug, Default, Clone, Copy)]
pub struct HostCopyImporter;

impl HostCopyImporter {
    /// The route's copy path.
    #[must_use]
    pub const fn copy_path(self) -> CopyPath {
        CopyPath::HostCopy
    }

    /// Stages the leased frame into `sink`. Release the lease after this
    /// returns: the copy is complete either way.
    ///
    /// # Errors
    ///
    /// [`ImportError`]; on any error the sink holds no new pixels.
    pub fn import(
        self,
        lease: &CaptureLease<BufferToken>,
        sink: &mut impl HostBgraSink,
    ) -> Result<ImportReport, ImportError> {
        let frame = lease.frame();
        let surface = frame.surface.surface();
        if surface.memory != CaptureMemory::HostShared {
            return Err(ImportError::NotHostMemory);
        }
        if !matches!(
            surface.format,
            CapturePixelFormat::Bgrx8 | CapturePixelFormat::Bgra8
        ) {
            return Err(ImportError::Format(surface.format));
        }
        let plane = surface.planes[0];
        let started = Instant::now();
        let bytes = frame.token.read(|views| -> Result<usize, ImportError> {
            let PlaneView::Host(buffer) = views[usize::from(plane.buffer)] else {
                return Err(ImportError::Unmapped);
            };
            let rows =
                plane_bytes(buffer, plane.offset, plane.size).ok_or(ImportError::Unmapped)?;
            sink.stage_host_bgra(rows, plane.stride as usize)
                .map_err(ImportError::Stage)?;
            Ok(rows.len())
        })??;
        Ok(ImportReport {
            copy_path: self.copy_path(),
            bytes,
            elapsed: started.elapsed(),
        })
    }
}

/// The validated plane's bytes within its mapped buffer object.
#[must_use]
pub fn plane_bytes(buffer: &[u8], offset: u64, size: u64) -> Option<&[u8]> {
    let start = usize::try_from(offset).ok()?;
    let end = start.checked_add(usize::try_from(size).ok()?)?;
    buffer.get(start..end)
}

impl HostBgraSink for crate::nvenc_cuda::Encoder {
    fn stage_host_bgra(&mut self, rows: &[u8], stride: usize) -> Result<(), String> {
        // SAFETY: the encoder is only used on the thread that created it
        // with its CUDA context current (the import worker), which is the
        // contract `Encoder::stage_host_bgra` requires.
        unsafe { crate::nvenc_cuda::Encoder::stage_host_bgra(self, rows, stride) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plane_bytes_stay_inside_the_mapping() {
        let buffer = [0u8; 32];
        assert_eq!(plane_bytes(&buffer, 8, 16).map(<[u8]>::len), Some(16));
        assert_eq!(plane_bytes(&buffer, 24, 16), None);
        assert_eq!(plane_bytes(&buffer, u64::MAX, 1), None);
    }
}
