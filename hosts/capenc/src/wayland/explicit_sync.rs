//! `PipeWire` explicit synchronisation (`SPA_META_SyncTimeline`) through DRM
//! timeline syncobjs.
//!
//! A producer that negotiates explicit sync attaches two extra data blocks
//! of type `SPA_DATA_SyncObj` to each DMA-BUF buffer (acquire, then release;
//! Mutter 49 uses one syncobj for both) and a `spa_meta_sync_timeline` with
//! two points on them. The consumer:
//!
//! 1. waits for the **acquire** point before any GPU reads the buffer
//!    (the producer's rendering is complete; [`wait_point`], in the import
//!    worker, never in the `PipeWire` callback);
//! 2. signals the **release** point once nothing reads the buffer any more
//!    ([`signal_point`], on the loop thread, immediately before the buffer is
//!    queued back), and never earlier: the producer may render into the
//!    buffer as soon as the point signals.
//!
//! The ioctl layouts are the kernel's stable UAPI (`include/uapi/drm/drm.h`)
//! with the original (shortest) sizes; newer kernels zero-extend them.

/// `SPA_DATA_SyncObj` (spa/buffer/buffer.h), defined here because the build
/// host's SPA headers may predate it.
pub const SPA_DATA_SYNC_OBJ: u32 = 5;
/// `SPA_META_SyncTimeline` (spa/buffer/meta.h).
pub const SPA_META_SYNC_TIMELINE: u32 = 9;
/// `SPA_PARAM_BUFFERS_metaType` (spa/param/buffers.h).
pub const SPA_PARAM_BUFFERS_META_TYPE: u32 = 7;
/// `SPA_PARAM_BUFFERS_blocks`.
pub const SPA_PARAM_BUFFERS_BLOCKS: u32 = 2;
/// `sizeof(struct spa_meta_sync_timeline)`.
pub const SPA_META_SYNC_TIMELINE_BYTES: usize = 24;
/// `SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE`: set by the producer, cleared
/// by a consumer that promises to signal the release point.
pub const SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE: u32 = 1;
/// Extra data blocks an explicitly synchronised buffer carries.
pub const SYNC_OBJ_BLOCKS: usize = 2;

/// The timeline points of one buffer, read while it is dequeued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncPoints {
    /// Syncobj the acquire point is on.
    pub acquire_fd: i32,
    /// Point that signals when the producer's writes are complete.
    pub acquire_point: u64,
    /// Syncobj the release point is on.
    pub release_fd: i32,
    /// Point the consumer signals when it no longer reads the buffer.
    pub release_point: u64,
}

/// `spa_meta_sync_timeline` as plain data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncTimelineMeta {
    /// `flags`.
    pub flags: u32,
    /// `acquire_point`.
    pub acquire_point: u64,
    /// `release_point`.
    pub release_point: u64,
}

impl SyncTimelineMeta {
    /// Parses the meta's bytes (native endianness, as the producer wrote
    /// them); `None` when shorter than the structure.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let word = |range: std::ops::Range<usize>| bytes.get(range);
        let flags = u32::from_ne_bytes(word(0..4)?.try_into().ok()?);
        let acquire_point = u64::from_ne_bytes(word(8..16)?.try_into().ok()?);
        let release_point = u64::from_ne_bytes(word(16..24)?.try_into().ok()?);
        Some(Self {
            flags,
            acquire_point,
            release_point,
        })
    }
}

/// Consecutive release points that could not be signalled before the
/// stream withdraws DMA-BUF: a producer that never sees its release points
/// may stop reusing those buffers, so the epoch falls back to shared memory
/// rather than let the pool drain.
pub const RELEASE_FAILURES_BEFORE_FALLBACK: u32 = 3;

/// Writes the consumer's answer into a buffer's `spa_meta_sync_timeline`
/// bytes when the buffer goes back to the producer: the unscheduled-release
/// flag is cleared only once the release point was actually `signalled`.
/// Otherwise the flag stays as the producer set it, so a producer that
/// honours it releases the buffer itself. Returns the flags written (or
/// left); `None` when `meta` is shorter than the flags field.
pub fn hand_back_release(meta: &mut [u8], signalled: bool) -> Option<u32> {
    let field = meta.get_mut(0..4)?;
    let flags = u32::from_ne_bytes(field.try_into().ok()?);
    if !signalled {
        return Some(flags);
    }
    let cleared = flags & !SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE;
    field.copy_from_slice(&cleared.to_ne_bytes());
    Some(cleared)
}

/// Counts consecutive release-signal failures; [`Self::record`] answers
/// `true` exactly once, when the run reaches
/// [`RELEASE_FAILURES_BEFORE_FALLBACK`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReleaseFailures {
    consecutive: u32,
}

impl ReleaseFailures {
    /// Records one release; returns whether DMA-BUF must now be withdrawn.
    pub fn record(&mut self, signalled: bool) -> bool {
        if signalled {
            self.consecutive = 0;
            return false;
        }
        self.consecutive = self.consecutive.saturating_add(1);
        self.consecutive == RELEASE_FAILURES_BEFORE_FALLBACK
    }
}

/// Where the sync blocks sit in a buffer of `n_datas` data blocks:
/// `(acquire, release)` indices, the last two blocks as Mutter and the
/// `PipeWire` documentation place them.
#[must_use]
pub const fn sync_block_indices(n_datas: usize) -> Option<(usize, usize)> {
    if n_datas < SYNC_OBJ_BLOCKS + 1 {
        None
    } else {
        Some((n_datas - 2, n_datas - 1))
    }
}

/// `_IOWR('d', nr, size)`.
#[must_use]
pub const fn drm_iowr(nr: u32, size: u32) -> u64 {
    const IOC_READ_WRITE: u64 = 3;
    (IOC_READ_WRITE << 30) | ((size as u64) << 16) | (0x64 << 8) | nr as u64
}

/// `DRM_IOCTL_SYNCOBJ_DESTROY` (`struct drm_syncobj_destroy`, 8 bytes).
pub const DRM_IOCTL_SYNCOBJ_DESTROY: u64 = drm_iowr(0xC0, 8);
/// `DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE` (`struct drm_syncobj_handle`, original 16 bytes).
pub const DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE: u64 = drm_iowr(0xC2, 16);
/// `DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT` (`struct drm_syncobj_timeline_wait`, original 40 bytes).
pub const DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT: u64 = drm_iowr(0xCA, 40);
/// `DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL` (`struct drm_syncobj_timeline_array`, 24 bytes).
pub const DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL: u64 = drm_iowr(0xCD, 24);
/// `DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL`.
pub const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL: u32 = 1;
/// `DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT`.
pub const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT: u32 = 2;

/// Why a syncobj operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncError {
    /// The DRM device could not be opened.
    Open(String),
    /// The syncobj descriptor could not be imported.
    Import(i32),
    /// The point did not signal within the bound.
    TimedOut,
    /// The wait or signal ioctl failed.
    Ioctl(&'static str, i32),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open(detail) => write!(formatter, "DRM device: {detail}"),
            Self::Import(errno) => write!(formatter, "syncobj import failed (errno {errno})"),
            Self::TimedOut => write!(formatter, "acquire point did not signal in time"),
            Self::Ioctl(call, errno) => write!(formatter, "{call} failed (errno {errno})"),
        }
    }
}

impl std::error::Error for SyncError {}

#[cfg(target_os = "linux")]
pub use device::SyncobjDevice;

#[cfg(target_os = "linux")]
mod device {
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::path::Path;
    use std::time::Duration;

    use super::{
        SyncError, DRM_IOCTL_SYNCOBJ_DESTROY, DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE,
        DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT,
        DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL, DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT,
    };

    #[repr(C)]
    struct SyncobjHandle {
        handle: u32,
        flags: u32,
        fd: i32,
        pad: u32,
    }

    #[repr(C)]
    struct SyncobjDestroy {
        handle: u32,
        pad: u32,
    }

    #[repr(C)]
    struct TimelineWait {
        handles: u64,
        points: u64,
        timeout_nsec: i64,
        count_handles: u32,
        flags: u32,
        first_signaled: u32,
        pad: u32,
    }

    #[repr(C)]
    struct TimelineArray {
        handles: u64,
        points: u64,
        count_handles: u32,
        flags: u32,
    }

    const _: () = {
        assert!(std::mem::size_of::<SyncobjHandle>() == 16);
        assert!(std::mem::size_of::<SyncobjDestroy>() == 8);
        assert!(std::mem::size_of::<TimelineWait>() == 40);
        assert!(std::mem::size_of::<TimelineArray>() == 24);
    };

    fn errno() -> i32 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    }

    /// A DRM device file used only for syncobj operations. Syncobj
    /// descriptors are not tied to the device they came from, so any DRM
    /// node the process can open works; capenc uses the encoder GPU's render
    /// node. `Send + Sync`: every operation is one self-contained ioctl.
    #[derive(Debug)]
    pub struct SyncobjDevice {
        fd: OwnedFd,
    }

    /// A syncobj handle imported into the device, destroyed on drop.
    struct Handle<'device> {
        device: &'device SyncobjDevice,
        handle: u32,
    }

    impl Drop for Handle<'_> {
        fn drop(&mut self) {
            let mut destroy = SyncobjDestroy {
                handle: self.handle,
                pad: 0,
            };
            // SAFETY: the handle was created on this device by
            // `FD_TO_HANDLE` and is destroyed exactly once; `destroy` is a
            // correctly sized `drm_syncobj_destroy`.
            unsafe {
                libc::ioctl(
                    self.device.fd.as_raw_fd(),
                    DRM_IOCTL_SYNCOBJ_DESTROY as libc::c_ulong,
                    &raw mut destroy,
                );
            }
        }
    }

    impl SyncobjDevice {
        /// Opens `node` (normally `/dev/dri/renderD<minor>`).
        ///
        /// # Errors
        ///
        /// [`SyncError::Open`] when the node cannot be opened read-write.
        pub fn open(node: &Path) -> Result<Self, SyncError> {
            use std::os::unix::fs::OpenOptionsExt;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_CLOEXEC)
                .open(node)
                .map_err(|error| SyncError::Open(format!("{}: {error}", node.display())))?;
            Ok(Self { fd: file.into() })
        }

        fn import(&self, syncobj_fd: i32) -> Result<Handle<'_>, SyncError> {
            let mut request = SyncobjHandle {
                handle: 0,
                flags: 0,
                fd: syncobj_fd,
                pad: 0,
            };
            // SAFETY: `request` is a correctly sized `drm_syncobj_handle`; the
            // kernel only reads `fd`/`flags` and writes `handle`. The caller
            // keeps `syncobj_fd` open for the call.
            let status = unsafe {
                libc::ioctl(
                    self.fd.as_raw_fd(),
                    DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE as libc::c_ulong,
                    &raw mut request,
                )
            };
            if status != 0 {
                return Err(SyncError::Import(errno()));
            }
            Ok(Handle {
                device: self,
                handle: request.handle,
            })
        }

        /// Waits up to `timeout` for `point` on the syncobj `syncobj_fd` to
        /// be submitted and signalled.
        ///
        /// # Errors
        ///
        /// [`SyncError`]; the buffer must then not be read.
        pub fn wait_point(
            &self,
            syncobj_fd: i32,
            point: u64,
            timeout: Duration,
        ) -> Result<(), SyncError> {
            let handle = self.import(syncobj_fd)?;
            let mut handles = [handle.handle];
            let mut points = [point];
            let mut now = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: `now` is a valid `timespec` for the duration of the call.
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut now) };
            // `time_t` and `c_long` are narrower than `i64` on 32-bit ABIs.
            #[allow(clippy::useless_conversion)]
            let deadline = i64::from(now.tv_sec)
                .saturating_mul(1_000_000_000)
                .saturating_add(i64::from(now.tv_nsec))
                .saturating_add(i64::try_from(timeout.as_nanos()).unwrap_or(i64::MAX));
            let mut wait = TimelineWait {
                handles: handles.as_mut_ptr() as u64,
                points: points.as_mut_ptr() as u64,
                timeout_nsec: deadline,
                count_handles: 1,
                flags: DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT,
                first_signaled: 0,
                pad: 0,
            };
            // SAFETY: `wait` is a correctly sized `drm_syncobj_timeline_wait`
            // whose `handles`/`points` point at one live element each for the
            // duration of the call.
            let status = unsafe {
                libc::ioctl(
                    self.fd.as_raw_fd(),
                    DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT as libc::c_ulong,
                    &raw mut wait,
                )
            };
            if status == 0 {
                return Ok(());
            }
            match errno() {
                libc::ETIME | libc::ETIMEDOUT => Err(SyncError::TimedOut),
                other => Err(SyncError::Ioctl("DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT", other)),
            }
        }

        /// Signals `point` on the syncobj `syncobj_fd` from the CPU.
        ///
        /// # Errors
        ///
        /// [`SyncError`] when the descriptor or the signal is refused.
        pub fn signal_point(&self, syncobj_fd: i32, point: u64) -> Result<(), SyncError> {
            let handle = self.import(syncobj_fd)?;
            let mut handles = [handle.handle];
            let mut points = [point];
            let mut signal = TimelineArray {
                handles: handles.as_mut_ptr() as u64,
                points: points.as_mut_ptr() as u64,
                count_handles: 1,
                flags: 0,
            };
            // SAFETY: as for `wait_point`, with a `drm_syncobj_timeline_array`.
            let status = unsafe {
                libc::ioctl(
                    self.fd.as_raw_fd(),
                    DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL as libc::c_ulong,
                    &raw mut signal,
                )
            };
            if status == 0 {
                Ok(())
            } else {
                Err(SyncError::Ioctl(
                    "DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL",
                    errno(),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_the_kernel_uapi() {
        // Values computed by the C preprocessor from include/uapi/drm/drm.h
        // with the original structure sizes.
        assert_eq!(DRM_IOCTL_SYNCOBJ_DESTROY, 0xC008_64C0);
        assert_eq!(DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, 0xC010_64C2);
        assert_eq!(DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT, 0xC028_64CA);
        assert_eq!(DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, 0xC018_64CD);
    }

    #[test]
    fn sync_timeline_meta_parses_the_spa_layout() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u32.to_ne_bytes());
        bytes.extend_from_slice(&0u32.to_ne_bytes());
        bytes.extend_from_slice(&41u64.to_ne_bytes());
        bytes.extend_from_slice(&42u64.to_ne_bytes());
        assert_eq!(bytes.len(), SPA_META_SYNC_TIMELINE_BYTES);
        assert_eq!(
            SyncTimelineMeta::parse(&bytes),
            Some(SyncTimelineMeta {
                flags: SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE,
                acquire_point: 41,
                release_point: 42,
            })
        );
        assert_eq!(SyncTimelineMeta::parse(&bytes[..23]), None);
    }

    fn fake_meta(flags: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&flags.to_ne_bytes());
        bytes.extend_from_slice(&0u32.to_ne_bytes());
        bytes.extend_from_slice(&7u64.to_ne_bytes());
        bytes.extend_from_slice(&8u64.to_ne_bytes());
        bytes
    }

    #[test]
    fn the_unscheduled_release_flag_is_cleared_only_after_a_signal() {
        let other = 1 << 4;
        let mut meta = fake_meta(SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE | other);
        // A failed signal leaves the producer's flag, and everything else.
        let before = meta.clone();
        assert_eq!(
            hand_back_release(&mut meta, false),
            Some(SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE | other)
        );
        assert_eq!(meta, before);
        // A signalled release clears only that flag; the points are untouched.
        assert_eq!(hand_back_release(&mut meta, true), Some(other));
        let parsed = SyncTimelineMeta::parse(&meta).unwrap();
        assert_eq!(parsed.flags, other);
        assert_eq!((parsed.acquire_point, parsed.release_point), (7, 8));
        // Idempotent, and a truncated meta is refused untouched.
        assert_eq!(hand_back_release(&mut meta, true), Some(other));
        let mut short = vec![1u8, 0, 0];
        assert_eq!(hand_back_release(&mut short, true), None);
        assert_eq!(short, vec![1u8, 0, 0]);
    }

    #[test]
    fn repeated_release_failures_withdraw_dmabuf_once() {
        let mut failures = ReleaseFailures::default();
        assert!(!failures.record(false));
        assert!(!failures.record(false));
        // A success resets the run.
        assert!(!failures.record(true));
        for _ in 1..RELEASE_FAILURES_BEFORE_FALLBACK {
            assert!(!failures.record(false));
        }
        assert!(failures.record(false));
        // Only once: the fallback is already under way.
        assert!(!failures.record(false));
    }

    #[test]
    fn sync_blocks_are_the_last_two() {
        assert_eq!(sync_block_indices(3), Some((1, 2)));
        assert_eq!(sync_block_indices(2), None);
        assert_eq!(sync_block_indices(0), None);
    }
}
