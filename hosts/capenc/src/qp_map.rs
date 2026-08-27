//! Bridging Keel's damage grid to NVENC's per-block QP delta map.
//!
//! Keel ([`arcen_keel`]) owns *what changed*; `arcen_media`'s
//! [`QpDeltaMapBuilder`] owns *how a codec wants to hear it*. This module is
//! the seam, and the only place that depends on both.
//!
//! It exists as its own module rather than as a few lines inside the capture
//! loops because the invariant it rests on — that Keel's block size and the
//! translator's assumption about Keel's block size are the same number — is
//! not checked by the type system and is worth a test with a name.

use arcen_keel::DamageMap;
use arcen_media::video::{QpBias, QpDeltaMapBuilder, QpMapError};

pub(crate) use arcen_media::video::QpMapPolicy;

/// Fill `builder` from `damage` and return the map NVENC should be handed.
///
/// A keyframe is passed `keyframe = true` and always yields a neutral map: an
/// IDR codes every block intra, so "unchanged since the previous frame"
/// describes nothing the encoder can act on, and a clean-region penalty
/// applied there would be baked into the reference every following frame is
/// predicted from.
///
/// # Errors
///
/// [`QpMapError::GridMismatch`] when the damage map describes a different
/// frame geometry than the builder was constructed for — refused rather than
/// misaligned, because a silently wrong map biases the wrong parts of the
/// screen and would look like an encoder bug, not a wiring bug.
pub(crate) fn fill_qp_delta_map<'a>(
    builder: &'a mut QpDeltaMapBuilder,
    damage: DamageMap<'_>,
    bias: QpBias,
    keyframe: bool,
) -> Result<&'a [i8], QpMapError> {
    if keyframe {
        return Ok(builder.build_neutral());
    }
    let grid = damage.grid();
    let cols = u32::try_from(grid.blocks_wide()).unwrap_or(u32::MAX);
    let rows = u32::try_from(grid.blocks_tall()).unwrap_or(u32::MAX);
    builder.build(cols, rows, bias, |x, y| {
        grid.block_index(x as usize, y as usize)
            .is_some_and(|index| damage.is_dirty(index))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_keel::{BgraFrame, DamageTracker, KernelPreference};
    use arcen_media::VideoCodec;

    /// The one invariant this module rests on and the type system does not
    /// enforce: `arcen_media` mirrors Keel's block size as a constant so it
    /// can stay free of a Keel dependency. If Keel ever changes its grid,
    /// every QP map would silently address the wrong blocks.
    #[test]
    fn keel_block_size_matches_the_translator_assumption() {
        assert_eq!(
            u32::try_from(arcen_keel::BLOCK_SIZE).unwrap(),
            arcen_media::video::KEEL_BLOCK_SIZE,
            "arcen-media mirrors Keel's block size; they have diverged"
        );
    }

    #[test]
    fn policy_tokens_round_trip_and_default_is_off() {
        for policy in QpMapPolicy::ALL.iter().copied() {
            assert_eq!(QpMapPolicy::from_token(policy.token()), Some(policy));
        }
        assert_eq!(QpMapPolicy::from_token("nonsense"), None);
        assert_eq!(QpMapPolicy::default(), QpMapPolicy::Off);
        assert!(!QpMapPolicy::Off.submits_map());
        assert!(QpMapPolicy::On.submits_map());
        assert!(
            QpMapPolicy::Neutral.submits_map(),
            "the control arm must still carry a map, or it measures nothing"
        );
    }

    /// Real Keel damage must reach the right coding blocks. A 64x32 frame is
    /// 4x2 Keel blocks and, for HEVC's 32x32 geometry, 2x1 coding blocks.
    #[test]
    fn real_keel_damage_biases_the_coding_blocks_that_cover_it() {
        const W: u32 = 64;
        const H: u32 = 32;
        let mut tracker =
            DamageTracker::new(W as usize, H as usize, KernelPreference::Xxh3).unwrap();
        let stride = (W * 4) as usize;

        // Baseline frame: everything is "changed" on first observation.
        let base = vec![0u8; stride * H as usize];
        tracker
            .update(BgraFrame::new(&base, W as usize, H as usize, stride).unwrap())
            .unwrap();

        // Change only the top-left 16x16 block, which lives in coding block 0.
        let mut next = base.clone();
        for row in 0..16usize {
            for col in 0..16usize {
                next[row * stride + col * 4] = 0xFF;
            }
        }
        tracker
            .update(BgraFrame::new(&next, W as usize, H as usize, stride).unwrap())
            .unwrap();

        let mut builder = QpDeltaMapBuilder::new(VideoCodec::H265, W, H).unwrap();
        assert_eq!(builder.dimensions(), (2, 1));
        let bias = QpBias {
            dirty: -5,
            clean: 2,
        };
        let map = fill_qp_delta_map(&mut builder, tracker.damage_map(), bias, false).unwrap();

        assert_eq!(map.len(), 2);
        assert_eq!(map[0], -5, "the changed 16x16 block sits in coding block 0");
        assert_eq!(map[1], 2, "nothing changed on the right half");
    }

    #[test]
    fn a_keyframe_always_gets_a_neutral_map_however_dirty_the_frame_is() {
        const W: u32 = 64;
        const H: u32 = 32;
        let mut tracker =
            DamageTracker::new(W as usize, H as usize, KernelPreference::Xxh3).unwrap();
        let stride = (W * 4) as usize;
        let base = vec![0u8; stride * H as usize];
        tracker
            .update(BgraFrame::new(&base, W as usize, H as usize, stride).unwrap())
            .unwrap();
        let noisy = (0..stride * H as usize)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        tracker
            .update(BgraFrame::new(&noisy, W as usize, H as usize, stride).unwrap())
            .unwrap();

        let mut builder = QpDeltaMapBuilder::new(VideoCodec::H265, W, H).unwrap();
        let map =
            fill_qp_delta_map(&mut builder, tracker.damage_map(), QpBias::default(), true).unwrap();
        assert!(
            map.iter().all(|delta| *delta == 0),
            "an IDR codes every block intra; damage describes nothing there"
        );
    }

    /// A damage map for a different resolution must be refused. Truncating or
    /// zero-filling would bias the wrong parts of the screen and present as an
    /// encoder fault rather than a wiring fault.
    #[test]
    fn a_damage_map_for_another_resolution_is_refused() {
        let mut tracker = DamageTracker::new(128, 64, KernelPreference::Xxh3).unwrap();
        let pixels = vec![0u8; 128 * 64 * 4];
        tracker
            .update(BgraFrame::new(&pixels, 128, 64, 128 * 4).unwrap())
            .unwrap();

        let mut builder = QpDeltaMapBuilder::new(VideoCodec::H265, 1920, 1080).unwrap();
        let error = fill_qp_delta_map(&mut builder, tracker.damage_map(), QpBias::default(), false)
            .unwrap_err();
        assert!(matches!(error, QpMapError::GridMismatch { .. }));
    }
}
