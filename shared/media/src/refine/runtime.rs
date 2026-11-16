// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The capenc-side refinement runtime (ADR 0016): the
//! [`super::producer::RefineProducer`] of one monitor over a
//! [`RefineSource`], one compression worker thread, and framed-v2 records.
//! Portable; a capture loop only feeds it frames and calls it around each
//! access unit. Without `refine-zstd-encode` it is a never-constructed
//! stand-in whose `start` refuses. The capture thread never waits on the worker:
//! work goes out and results come back through bounded channels polled with
//! `try_send`/`try_recv`, and records the token bucket refuses wait for a
//! later pass instead of delaying an access unit.

use super::CapencRefineArgs;
use super::source::RefineSource;
use crate::{BitDepth, ChromaSubsampling, ColorPrimaries, TransferCharacteristics};
use std::io::Write;

/// The stream contract a runtime refines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefineStream {
    /// Depth.
    pub depth: BitDepth,
    /// Chroma.
    pub chroma: ChromaSubsampling,
    /// Transfer.
    pub transfer: TransferCharacteristics,
    /// Primaries.
    pub primaries: ColorPrimaries,
}

#[cfg(feature = "refine-zstd-encode")]
mod enabled {
    use super::{CapencRefineArgs, RefineSource, RefineStream, Write};
    use crate::capenc_stream::{CapencRecordKind, write_record};
    use crate::refine::pier::TileJob;
    use crate::refine::producer::{MAX_OUTSTANDING_TILES, RefineProducer, TileWork};
    use crate::refine::tile::TileRect;
    use crate::refine::zstd_encode::TileEncoder;
    use crate::refine::{RefineConfig, RefinePlan, RefinePlanInputs, resolve_refine_plan};
    use arcen_protocol::refine::{BarrierReason, LosslessRefineCapability};
    use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
    use std::time::Instant;

    /// The refinement runtime of one monitor.
    #[derive(Debug)]
    pub struct RefineRuntime<S: RefineSource> {
        producer: RefineProducer<S>,
        started: Instant,
        to_worker: SyncSender<TileWork>,
        from_worker: Receiver<(TileJob, Option<Vec<u8>>)>,
        worker_busy_drops: u64,
    }

    impl<S: RefineSource> RefineRuntime<S> {
        /// Starts the runtime and its worker for one monitor over `source`.
        ///
        /// # Errors
        ///
        /// When the shared plan refuses the stream or the worker cannot start.
        pub fn start(
            args: &CapencRefineArgs,
            color: RefineStream,
            source: S,
        ) -> Result<Self, String> {
            // The Pier vouched for the Deck by passing `refine=on`; the
            // stream contract is still checked here, by the shared rule.
            let domain = source.domain();
            let deck = LosslessRefineCapability {
                domains: vec![domain.token().into()],
                codecs: vec!["delta-zstd1".into()],
            };
            let plan = resolve_refine_plan(&RefinePlanInputs {
                compiled: true,
                config: RefineConfig {
                    enabled: true,
                    still_ms: args.still_ms,
                    max_mbps: args.max_mbps,
                },
                depth: color.depth,
                chroma: color.chroma,
                transfer: color.transfer,
                primaries: color.primaries,
                source: source.kind(),
                deck: Some(&deck),
                deck_ten_bit: true,
            });
            let RefinePlan::On {
                contract,
                still_ms,
                max_mbps,
            } = plan
            else {
                let RefinePlan::Off(reason) = plan else {
                    unreachable!()
                };
                return Err(format!("refinement refused: {}", reason.token()));
            };
            if source.depth() != contract.bits() {
                return Err(format!(
                    "refinement refused: the source has {}-bit codes, the stream {}",
                    source.depth(),
                    contract.bits()
                ));
            }
            let (to_worker, work_rx) = sync_channel::<TileWork>(MAX_OUTSTANDING_TILES);
            let (result_tx, from_worker) =
                sync_channel::<(TileJob, Option<Vec<u8>>)>(MAX_OUTSTANDING_TILES);
            let mut encoder = TileEncoder::new().map_err(|error| format!("{error:?}"))?;
            let depth = contract.bits();
            std::thread::Builder::new()
                .name("capenc-refine".into())
                .spawn(move || {
                    while let Ok(work) = work_rx.recv() {
                        let payload = encoder.encode(&work.codes, depth).ok();
                        if result_tx.send((work.job, payload)).is_err() {
                            break;
                        }
                    }
                })
                .map_err(|error| format!("refine worker: {error}"))?;
            let started = Instant::now();
            Ok(Self {
                producer: RefineProducer::new(
                    args.scope,
                    contract,
                    source,
                    still_ms,
                    u64::from(max_mbps) * 1_000_000,
                    started.elapsed(),
                ),
                started,
                to_worker,
                from_worker,
                worker_busy_drops: 0,
            })
        }

        /// A whole captured frame.
        pub fn observe_whole(&mut self, bytes: &[u8], stride: usize) {
            let _ = self.observe_frame(bytes, stride);
        }

        /// A whole captured frame; returns the blocks it changed.
        pub fn observe_frame(&mut self, bytes: &[u8], stride: usize) -> Vec<usize> {
            let now = self.started.elapsed();
            self.producer.observe_whole(bytes, stride, now)
        }

        /// Damage known without the pixels (dirty regions of a frame whose
        /// pixels reach the source later).
        pub fn observe_blocks(&mut self, blocks: Vec<usize>) {
            let now = self.started.elapsed();
            self.producer.observe_blocks(blocks, now);
        }

        /// The source.
        pub fn source_mut(&mut self) -> &mut S {
            self.producer.source_mut()
        }

        /// A captured rectangle (`bytes[0]` is its top-left pixel).
        pub fn observe_rect(
            &mut self,
            bytes: &[u8],
            stride: usize,
            x: usize,
            y: usize,
            width: usize,
            height: usize,
        ) {
            let narrow = |value: usize| u32::try_from(value).unwrap_or(u32::MAX);
            let rect = TileRect {
                x: narrow(x),
                y: narrow(y),
                width: narrow(width),
                height: narrow(height),
            };
            let now = self.started.elapsed();
            let _ = self.producer.observe_rect(bytes, stride, rect, now);
        }

        /// A recovery keyframe: a new generation, so nothing refined before
        /// it inherits validity.
        pub fn recovery(&mut self) {
            self.producer.reset(BarrierReason::Recovery);
        }

        /// The Deck asked to start over (`refine_reset`).
        pub fn reset_request(&mut self) {
            self.producer.reset(BarrierReason::ResetRequest);
        }

        /// The Deck turned refinement for this monitor off or on again
        /// (`refine_control`).
        pub fn set_enabled(&mut self, enabled: bool) {
            self.producer.set_enabled(enabled);
        }

        /// The session's video bitrate target changed: the refinement
        /// floor until the Pier's link budget sets the rate.
        pub fn cap_rate(&mut self, bps: u64) {
            let now = self.started.elapsed();
            self.producer.cap_rate(bps, now);
        }

        /// The Pier's link budget (`REFINE-RATE`): the rate from the link's
        /// residual, or a hold.
        pub fn set_link_rate(&mut self, bps: u64, hold: bool) {
            let now = self.started.elapsed();
            self.producer.set_link_rate(bps, hold, now);
        }

        /// Writes the video-lane records that precede the next access unit.
        ///
        /// # Errors
        ///
        /// The writer's error.
        pub fn before_access_unit(&mut self, out: &mut impl Write) -> std::io::Result<()> {
            for record in self.take_video_records() {
                write_record(out, CapencRecordKind::RefineVideo, &record)?;
            }
            Ok(())
        }

        /// The video-lane records (barrier, fence) that must precede the
        /// next access unit, as wire records, for a Pier that writes its
        /// own lanes instead of a capenc pipe.
        pub fn take_video_records(&mut self) -> Vec<Vec<u8>> {
            self.producer.before_access_unit()
        }

        /// Exchanges work with the worker and writes the records the bucket
        /// admits. Never blocks on the worker.
        ///
        /// # Errors
        ///
        /// The writer's error.
        pub fn pump(&mut self, out: &mut impl Write) -> std::io::Result<()> {
            for record in self.pump_records() {
                write_record(out, CapencRecordKind::RefineLane, &record)?;
            }
            Ok(())
        }

        /// [`Self::pump`] for a Pier that writes its own lanes: the
        /// refine-lane records (tiles, then a digest) the bucket admits now,
        /// in order. Never blocks on the worker.
        pub fn pump_records(&mut self) -> Vec<Vec<u8>> {
            let now = self.started.elapsed();
            while let Ok((job, payload)) = self.from_worker.try_recv() {
                self.producer.finish(&job, payload.as_deref());
            }
            for work in self.producer.take_work(now) {
                match self.to_worker.try_send(work) {
                    Ok(()) => {}
                    Err(TrySendError::Full(work) | TrySendError::Disconnected(work)) => {
                        self.worker_busy_drops += 1;
                        self.producer.finish(&work.job, None);
                    }
                }
            }
            let mut records = self.producer.drain_ready(now);
            records.extend(self.producer.digest(now));
            records
        }

        /// Per-second log fields.
        #[must_use]
        pub fn log_fields(&self) -> String {
            let scheduler = self.producer.scheduler();
            format!(
                " refine_fence={} refine_gen={} refine_records={} refine_kb={} refine_worker_drops={} refine_motion_cancels={} refine_in_motion={} refine_rate_kbps={} refine_rate_src={} refine_holds={} refine_paused={}",
                scheduler.fence(),
                scheduler.scope().generation,
                self.producer.written,
                self.producer.written_bytes / 1024,
                self.worker_busy_drops,
                self.producer.dropped,
                scheduler.in_motion(self.started.elapsed()),
                self.producer.rate_bps() / 1000,
                if self.producer.rate_from_link() {
                    "link"
                } else {
                    "floor"
                },
                self.producer.holds,
                self.producer.is_paused(),
            )
        }
    }
}

#[cfg(feature = "refine-zstd-encode")]
pub use enabled::RefineRuntime;

/// Stand-in when the build has no refinement: never constructed.
#[cfg(not(feature = "refine-zstd-encode"))]
#[derive(Debug)]
pub struct RefineRuntime<S: RefineSource>(std::convert::Infallible, std::marker::PhantomData<S>);

#[cfg(not(feature = "refine-zstd-encode"))]
#[allow(clippy::unused_self, clippy::needless_pass_by_ref_mut)]
impl<S: RefineSource> RefineRuntime<S> {
    /// Always refuses: this build has no refinement encoder.
    ///
    /// # Errors
    ///
    /// Always.
    pub fn start(
        _args: &CapencRefineArgs,
        _color: RefineStream,
        _source: S,
    ) -> Result<Self, String> {
        Err("this build has no refinement encoder (refine-zstd-encode)".to_string())
    }
    /// Never called.
    pub fn observe_whole(&mut self, _bytes: &[u8], _stride: usize) {
        match self.0 {}
    }
    /// Never called.
    pub fn observe_frame(&mut self, _bytes: &[u8], _stride: usize) -> Vec<usize> {
        match self.0 {}
    }
    /// Never called.
    pub fn observe_blocks(&mut self, _blocks: Vec<usize>) {
        match self.0 {}
    }
    /// Never called.
    pub fn source_mut(&mut self) -> &mut S {
        match self.0 {}
    }
    /// Never called.
    pub fn observe_rect(
        &mut self,
        _bytes: &[u8],
        _stride: usize,
        _x: usize,
        _y: usize,
        _width: usize,
        _height: usize,
    ) {
        match self.0 {}
    }
    /// Never called.
    pub fn recovery(&mut self) {
        match self.0 {}
    }
    /// Never called.
    pub fn reset_request(&mut self) {
        match self.0 {}
    }
    /// Never called.
    pub fn set_enabled(&mut self, _enabled: bool) {
        match self.0 {}
    }
    /// Never called.
    pub fn cap_rate(&mut self, _bps: u64) {
        match self.0 {}
    }
    /// Never called.
    pub fn set_link_rate(&mut self, _bps: u64, _hold: bool) {
        match self.0 {}
    }
    /// Never called.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn before_access_unit(&mut self, _out: &mut impl Write) -> std::io::Result<()> {
        match self.0 {}
    }
    /// Never called.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn pump(&mut self, _out: &mut impl Write) -> std::io::Result<()> {
        match self.0 {}
    }
    /// Never called.
    pub fn take_video_records(&mut self) -> Vec<Vec<u8>> {
        match self.0 {}
    }
    /// Never called.
    pub fn pump_records(&mut self) -> Vec<Vec<u8>> {
        match self.0 {}
    }
    /// Never called.
    #[must_use]
    pub fn log_fields(&self) -> String {
        match self.0 {}
    }
}
