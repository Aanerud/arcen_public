// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The capenc-side refinement runtime (ADR 0016): the [`RefineProducer`]
//! of one monitor, one compression worker thread, and framed-v2 records.
//! Portable; a capture loop only feeds it frames and calls it around each
//! access unit. Without `refine-zstd-encode` it is a never-constructed
//! stand-in whose `start` refuses. The capture thread never waits on the worker:
//! work goes out and results come back through bounded channels polled with
//! `try_send`/`try_recv`, and records the token bucket refuses wait for a
//! later pass instead of delaying an access unit.

use super::CapencRefineArgs;
use crate::video::{PackedRgb10Layout, Rgb10Signal};
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
    use super::{CapencRefineArgs, PackedRgb10Layout, RefineStream, Rgb10Signal, Write};
    use crate::capenc_stream::{CapencRecordKind, write_record};
    use crate::refine::pier::TileJob;
    use crate::refine::producer::{MAX_OUTSTANDING_TILES, RefineProducer, TileWork};
    use crate::refine::tile::{RefineGrid, TileRect};
    use crate::refine::zstd_encode::TileEncoder;
    use crate::refine::{
        RefineConfig, RefinePlan, RefinePlanInputs, RefineSource, resolve_refine_plan,
    };
    use arcen_protocol::refine::{BarrierReason, LosslessRefineCapability};
    use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
    use std::time::Instant;

    /// The refinement runtime of one monitor.
    #[derive(Debug)]
    pub struct RefineRuntime {
        producer: RefineProducer,
        started: Instant,
        to_worker: SyncSender<TileWork>,
        from_worker: Receiver<(TileJob, Option<Vec<u8>>)>,
        worker_busy_drops: u64,
    }

    impl RefineRuntime {
        /// Starts the runtime and its worker for one monitor.
        ///
        /// # Errors
        ///
        /// When the shared plan refuses the stream or the worker cannot start.
        pub fn start(
            args: &CapencRefineArgs,
            color: RefineStream,
            width: u32,
            height: u32,
            layout: PackedRgb10Layout,
            signal: Rgb10Signal,
        ) -> Result<Self, String> {
            // The Pier vouched for the Deck by passing `refine=on`; the
            // stream contract is still checked here, by the shared rule.
            let deck = LosslessRefineCapability {
                domains: vec!["rgb".into()],
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
                source: RefineSource::CpuPackedRgb10,
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
                    RefineGrid { width, height },
                    layout,
                    signal,
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
            let now = self.started.elapsed();
            self.producer.observe_whole(bytes, stride, now);
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
            self.producer.observe_rect(bytes, stride, rect, now);
        }

        /// A recovery keyframe: a new generation, so nothing refined before
        /// it inherits validity.
        pub fn recovery(&mut self) {
            self.producer.reset(BarrierReason::Recovery);
        }

        /// Writes the video-lane records that precede the next access unit.
        ///
        /// # Errors
        ///
        /// The writer's error.
        pub fn before_access_unit(&mut self, out: &mut impl Write) -> std::io::Result<()> {
            for record in self.producer.before_access_unit() {
                write_record(out, CapencRecordKind::RefineVideo, &record)?;
            }
            Ok(())
        }

        /// Exchanges work with the worker and writes the records the bucket
        /// admits. Never blocks on the worker.
        ///
        /// # Errors
        ///
        /// The writer's error.
        pub fn pump(&mut self, out: &mut impl Write) -> std::io::Result<()> {
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
            for record in self.producer.drain_ready(now) {
                write_record(out, CapencRecordKind::RefineLane, &record)?;
            }
            if let Some(digest) = self.producer.digest(now) {
                write_record(out, CapencRecordKind::RefineLane, &digest)?;
            }
            Ok(())
        }

        /// Per-second log fields.
        #[must_use]
        pub fn log_fields(&self) -> String {
            let scheduler = self.producer.scheduler();
            format!(
                " refine_fence={} refine_gen={} refine_records={} refine_kb={} refine_worker_drops={} refine_motion_cancels={} refine_in_motion={}",
                scheduler.fence(),
                scheduler.scope().generation,
                self.producer.written,
                self.producer.written_bytes / 1024,
                self.worker_busy_drops,
                self.producer.dropped,
                scheduler.in_motion(self.started.elapsed()),
            )
        }
    }
}

#[cfg(feature = "refine-zstd-encode")]
pub use enabled::RefineRuntime;

/// Stand-in when the build has no refinement: never constructed.
#[cfg(not(feature = "refine-zstd-encode"))]
#[derive(Debug)]
pub enum RefineRuntime {}

#[cfg(not(feature = "refine-zstd-encode"))]
#[allow(clippy::unused_self, clippy::needless_pass_by_ref_mut)]
impl RefineRuntime {
    /// Always refuses: this build has no refinement encoder.
    ///
    /// # Errors
    ///
    /// Always.
    pub fn start(
        _args: &CapencRefineArgs,
        _color: RefineStream,
        _width: u32,
        _height: u32,
        _layout: PackedRgb10Layout,
        _signal: Rgb10Signal,
    ) -> Result<Self, String> {
        Err("this build has no refinement encoder (refine-zstd-encode)".to_string())
    }
    /// Never called.
    pub fn observe_whole(&mut self, _bytes: &[u8], _stride: usize) {
        match *self {}
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
        match *self {}
    }
    /// Never called.
    pub fn recovery(&mut self) {
        match *self {}
    }
    /// Never called.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn before_access_unit(&mut self, _out: &mut impl Write) -> std::io::Result<()> {
        match *self {}
    }
    /// Never called.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn pump(&mut self, _out: &mut impl Write) -> std::io::Result<()> {
        match *self {}
    }
    /// Never called.
    #[must_use]
    pub fn log_fields(&self) -> String {
        match *self {}
    }
}
