//! Keel-gated software encoding of delivered pictures.
//!
//! A capture source that delivers a picture only when something changed
//! (a Wayland compositor's `PipeWire` stream) needs exactly this between it
//! and [`SoftwareH264Session`]: each picture is hashed once by Keel, only its
//! changed 16x16 block rows are converted into the retained planes, an
//! unchanged picture costs no encode, and Keel's idle cadence encodes on
//! change, on a requested keyframe, and otherwise only at the keepalive,
//! re-encoding the retained picture. The Xorg loop polls its capture and
//! drives the per-region activity scheduler instead; both share the
//! session.

use std::time::{Duration, Instant};

use arcen_keel::{BgraFrame, DamageTracker, EmitMode, IdleCadence, KernelPreference};

use super::{EncodedAccessUnit, SoftwareH264Session, SoftwareSessionError, StageDamage};

/// What staging one delivered picture did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StagedPicture {
    /// The picture differs from the retained one.
    pub changed: bool,
    /// 16x16 blocks that changed (all of them for a full rebuild).
    pub dirty_blocks: usize,
    /// The whole picture was converted.
    pub full: bool,
}

/// Per-interval counters for the stats line.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LoopStats {
    /// Pictures staged.
    pub staged: u64,
    /// Pictures that changed nothing.
    pub clean: u64,
    /// Changed 16x16 blocks.
    pub dirty_blocks: u64,
    /// Time converting, milliseconds.
    pub convert_ms_sum: f64,
    /// Encodes submitted.
    pub submitted: u64,
    /// Time encoding, milliseconds.
    pub encode_ms_sum: f64,
    /// Slowest encode, milliseconds.
    pub encode_ms_max: f64,
    /// Submissions by mode: first, IDR, activity, keepalive.
    pub by_mode: [u64; 4],
}

impl LoopStats {
    const fn mode_index(mode: EmitMode) -> usize {
        match mode {
            EmitMode::FirstFrame => 0,
            EmitMode::Idr => 1,
            EmitMode::Activity => 2,
            EmitMode::Keepalive => 3,
        }
    }
}

/// The encode half of a software epoch.
#[derive(Debug)]
pub struct SoftwareEncodeLoop {
    session: SoftwareH264Session,
    tracker: Option<DamageTracker>,
    cadence: IdleCadence,
    staged_any: bool,
    stats: LoopStats,
}

impl SoftwareEncodeLoop {
    /// Wraps `session`; `keepalive` is the idle re-encode interval.
    #[must_use]
    pub fn new(session: SoftwareH264Session, keepalive: Duration) -> Self {
        // Without a damage tracker every picture is converted in full,
        // which is what a degraded tracker would report anyway.
        let tracker = DamageTracker::new(
            session.width() as usize,
            session.height() as usize,
            KernelPreference::Auto,
        )
        .ok();
        Self {
            session,
            tracker,
            cadence: IdleCadence::new(keepalive),
            staged_any: false,
            stats: LoopStats::default(),
        }
    }

    /// The shared session (live rate changes, bitrate bounds).
    pub fn session_mut(&mut self) -> &mut SoftwareH264Session {
        &mut self.session
    }

    /// Whether Keel hashes pictures (selective conversion).
    #[must_use]
    pub const fn damage_tracked(&self) -> bool {
        self.tracker.is_some()
    }

    /// Takes and resets the interval counters.
    pub fn take_stats(&mut self) -> LoopStats {
        std::mem::take(&mut self.stats)
    }

    /// Hashes one delivered picture and converts what changed into the
    /// retained planes. An unchanged picture marks nothing to encode.
    ///
    /// # Errors
    ///
    /// [`SoftwareSessionError`] from conversion; the retained planes
    /// must then be rebuilt in full (the tracker is reset).
    pub fn stage(&mut self, frame: BgraFrame<'_>) -> Result<StagedPicture, SoftwareSessionError> {
        let started = Instant::now();
        let staged = match self.tracker.as_mut().map(|tracker| tracker.update(frame)) {
            Some(Ok(summary)) if !self.staged_any || summary.is_full_damage() => StagedPicture {
                changed: true,
                dirty_blocks: summary.dirty_blocks,
                full: true,
            },
            Some(Ok(summary)) if summary.is_clean() => StagedPicture {
                changed: false,
                dirty_blocks: 0,
                full: false,
            },
            Some(Ok(summary)) => StagedPicture {
                changed: true,
                dirty_blocks: summary.dirty_blocks,
                full: false,
            },
            // No tracker, or a geometry it refused: convert everything.
            Some(Err(_)) | None => StagedPicture {
                changed: true,
                dirty_blocks: 0,
                full: true,
            },
        };
        let damage = match (staged.changed, staged.full, self.tracker.as_ref()) {
            (false, _, _) => StageDamage::Clean,
            (true, false, Some(tracker)) => StageDamage::Rows(tracker.damage_map()),
            (true, _, _) => StageDamage::Full,
        };
        if let Err(error) = self.session.stage(frame, damage) {
            if let Some(tracker) = self.tracker.as_mut() {
                tracker.reset();
            }
            self.staged_any = false;
            return Err(error);
        }
        self.staged_any = true;
        self.stats.staged += 1;
        self.stats.convert_ms_sum += started.elapsed().as_secs_f64() * 1000.0;
        if staged.changed {
            self.stats.dirty_blocks += staged.dirty_blocks as u64;
            self.cadence.note_frame();
        } else {
            self.stats.clean += 1;
        }
        Ok(staged)
    }

    /// What Keel's idle cadence asks for now: nothing before the first
    /// picture; then a change, a requested keyframe, or the keepalive.
    #[must_use]
    pub fn decision(&self, idr_pending: bool, since_emit: Duration) -> Option<EmitMode> {
        if !self.staged_any {
            return None;
        }
        self.cadence.decision(idr_pending, since_emit)
    }

    /// Encodes the retained picture for `mode` (a keyframe for the first
    /// picture and for a requested IDR). `Ok(None)` is `OpenH264`'s skip:
    /// the cadence stays due and the next tick encodes again.
    ///
    /// # Errors
    ///
    /// [`SoftwareSessionError`] from `OpenH264`.
    pub fn encode(
        &mut self,
        mode: EmitMode,
    ) -> Result<Option<EncodedAccessUnit<'_>>, SoftwareSessionError> {
        if matches!(mode, EmitMode::FirstFrame | EmitMode::Idr) {
            self.session.force_idr();
        }
        let started = Instant::now();
        let output = self.session.encode()?;
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        self.stats.submitted += 1;
        self.stats.encode_ms_sum += ms;
        self.stats.encode_ms_max = self.stats.encode_ms_max.max(ms);
        self.stats.by_mode[LoopStats::mode_index(mode)] += 1;
        if output.is_some() {
            self.cadence.on_submitted();
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::SoftwareSessionConfig;
    use crate::{
        BitDepth, ChromaSubsampling, ColorMatrix, ColorPrimaries, ColorRange,
        TransferCharacteristics, VideoCodec, VideoConfiguration,
    };

    const VIDEO: VideoConfiguration = VideoConfiguration {
        codec: VideoCodec::H264,
        chroma: ChromaSubsampling::Yuv420,
        bit_depth: BitDepth::Eight,
        range: ColorRange::Limited,
        matrix: ColorMatrix::Bt709,
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristics::Bt709,
    };

    const KEEPALIVE: Duration = Duration::from_secs(1);
    const SIZE: usize = 64;

    fn encode_loop() -> SoftwareEncodeLoop {
        let session = SoftwareH264Session::new(SoftwareSessionConfig {
            threads: 1,
            ..SoftwareSessionConfig::for_stream(SIZE as u32, SIZE as u32, 30, VIDEO)
        })
        .expect("session");
        SoftwareEncodeLoop::new(session, KEEPALIVE)
    }

    fn picture(value: u8, band: Option<std::ops::Range<usize>>) -> Vec<u8> {
        let mut pixels = vec![0x30u8; SIZE * SIZE * 4];
        if let Some(rows) = band {
            for row in rows {
                pixels[row * SIZE * 4..(row + 1) * SIZE * 4].fill(value);
            }
        }
        pixels
    }

    fn frame(pixels: &[u8]) -> BgraFrame<'_> {
        BgraFrame::new(pixels, SIZE, SIZE, SIZE * 4).expect("frame")
    }

    #[test]
    fn nothing_is_encoded_before_the_first_picture() {
        let encoder = encode_loop();
        assert_eq!(encoder.decision(true, KEEPALIVE * 5), None);
    }

    #[test]
    fn change_keyframe_and_keepalive_follow_keels_cadence() {
        let mut encoder = encode_loop();
        assert!(encoder.damage_tracked());
        let first = picture(0, None);
        let staged = encoder.stage(frame(&first)).expect("stage");
        assert!(staged.changed && staged.full);
        let mode = encoder.decision(false, Duration::ZERO).expect("first");
        assert_eq!(mode, EmitMode::FirstFrame);
        let access_unit = encoder.encode(mode).expect("encode").expect("access unit");
        assert!(access_unit.is_keyframe);

        // The same picture again changes nothing and costs no encode.
        let staged = encoder.stage(frame(&first)).expect("restage");
        assert!(!staged.changed);
        assert_eq!(encoder.decision(false, Duration::from_millis(10)), None);

        // One changed band is converted alone and encoded as activity.
        let moved = picture(0xC0, Some(16..32));
        let staged = encoder.stage(frame(&moved)).expect("band");
        assert!(staged.changed && !staged.full);
        assert!(staged.dirty_blocks > 0 && staged.dirty_blocks < (SIZE / 16).pow(2));
        let mode = encoder.decision(false, Duration::ZERO).expect("activity");
        assert_eq!(mode, EmitMode::Activity);
        let _ = encoder.encode(mode).expect("encode");

        // Idle: a keyframe request re-encodes the retained picture...
        assert_eq!(encoder.decision(true, Duration::ZERO), Some(EmitMode::Idr));
        let idr = encoder
            .encode(EmitMode::Idr)
            .expect("idr")
            .expect("access unit");
        assert!(idr.is_keyframe);
        // ...and otherwise only the keepalive does.
        assert_eq!(encoder.decision(false, KEEPALIVE / 2), None);
        assert_eq!(
            encoder.decision(false, KEEPALIVE),
            Some(EmitMode::Keepalive)
        );
        let stats = encoder.take_stats();
        assert_eq!(stats.staged, 3);
        assert_eq!(stats.clean, 1);
        assert_eq!(stats.by_mode, [1, 1, 1, 0]);
    }
}
