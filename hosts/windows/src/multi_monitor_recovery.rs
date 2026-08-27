//! Pure, versioned full-topology recovery-domain model for Windows physical
//! multi-monitor sessions.
//!
//! This is a deliberately **separate, standalone** versioned model. It is
//! never wired into `recovery.rs`'s live `DisplayRecoveryJournal` (currently
//! `JOURNAL_VERSION = 4`) — this tranche does not mutate the live display or
//! change that journal's on-disk format. `recovery.rs`'s own
//! `StableTopologySnapshot` already captures every active display path's
//! *identity* for restore-verification; this module extends that idea with
//! the full structured state (source binding, mode, position, rotation,
//! primary, opaque custom timing) needed to actually restore a topology, plus
//! its own generation and format version so it can evolve independently of
//! the live journal. A future tranche may fold this into `recovery.rs` via an
//! explicit, tested, backward-compatible version migration; until then it
//! stays a pure, disconnected domain model with no file I/O of its own.

use arcen_media::{Rotation, TopologyGeneration};

use crate::multi_monitor_topology::{WindowsMonitorPlan, WindowsTopologyPlan};
use crate::nvapi::AdapterLuid;

/// Current format version this module writes/expects.
pub const MULTI_MONITOR_RECOVERY_VERSION: u32 = 1;
/// Oldest format version this module still accepts when reading.
const MIN_SUPPORTED_MULTI_MONITOR_RECOVERY_VERSION: u32 = 1;
/// Matches `recovery.rs`'s existing `StableTopologySnapshot` path ceiling —
/// generous headroom above [`arcen_media::MAX_MULTI_MONITOR_COUNT`] because a
/// snapshot represents every active path on the system, not only the ones a
/// multi-monitor session is using.
const MAX_RECOVERY_PATHS: usize = 128;
/// Bounds an opaque custom-timing blob (hex-encoded, so twice this many
/// characters). Generous relative to the NVIDIA `NV_CUSTOM_DISPLAY` raw
/// timing structure this is meant to carry.
const MAX_CUSTOM_TIMING_BYTES: usize = 512;

/// Typed rejection building or validating a [`MultiMonitorRecoverySnapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultiMonitorRecoveryError {
    UnsupportedVersion(u32),
    EmptyTopology,
    TooManyPaths { count: usize },
    DuplicateOutputInTopology,
    ZeroDimensionMode { target_id: u32 },
    ZeroRefreshRate { target_id: u32 },
    InvalidCustomTimingHex { target_id: u32 },
    CustomTimingTooLarge { target_id: u32, bytes: usize },
    MismatchedSourceIdCount { paths: usize, source_ids: usize },
}

impl std::fmt::Display for MultiMonitorRecoveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedVersion(version) => write!(
                formatter,
                "multi-monitor recovery snapshot version {version} is unsupported (supported {MIN_SUPPORTED_MULTI_MONITOR_RECOVERY_VERSION}..={MULTI_MONITOR_RECOVERY_VERSION})"
            ),
            Self::EmptyTopology => {
                formatter.write_str("multi-monitor recovery snapshot has no active paths")
            }
            Self::TooManyPaths { count } => write!(
                formatter,
                "multi-monitor recovery snapshot has {count} paths, exceeding the {MAX_RECOVERY_PATHS} limit"
            ),
            Self::DuplicateOutputInTopology => formatter.write_str(
                "multi-monitor recovery snapshot has the same (adapter, target) output more than once",
            ),
            Self::ZeroDimensionMode { target_id } => write!(
                formatter,
                "multi-monitor recovery snapshot has a zero-width/height mode for target {target_id}"
            ),
            Self::ZeroRefreshRate { target_id } => write!(
                formatter,
                "multi-monitor recovery snapshot has a zero refresh rate for target {target_id}"
            ),
            Self::InvalidCustomTimingHex { target_id } => write!(
                formatter,
                "multi-monitor recovery snapshot has corrupt custom timing hex for target {target_id}"
            ),
            Self::CustomTimingTooLarge { target_id, bytes } => write!(
                formatter,
                "multi-monitor recovery snapshot custom timing for target {target_id} is {bytes} bytes, exceeding the {MAX_CUSTOM_TIMING_BYTES} limit"
            ),
            Self::MismatchedSourceIdCount { paths, source_ids } => write!(
                formatter,
                "multi-monitor recovery conversion got {source_ids} source ids for {paths} plan monitors"
            ),
        }
    }
}

impl std::error::Error for MultiMonitorRecoveryError {}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    let decode = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    };
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| Some((decode(pair[0])? << 4) | decode(pair[1])?))
        .collect()
}

/// One active display path's full pre-mutation (or just-applied) state.
///
/// "Path" here matches the Windows CCD sense: one bound source→target
/// connection, not merely a connector's identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredOutputState {
    /// Stable identity: see [`crate::multi_monitor_topology`]'s module
    /// documentation for why LUID + target id, not a global ordinal, is used.
    pub adapter_luid: AdapterLuid,
    pub target_id: u32,
    /// Windows CCD source id this target was bound to.
    pub source_id: u32,
    pub mode_width: u32,
    pub mode_height: u32,
    pub refresh_hz: u32,
    pub position_x: i32,
    pub position_y: i32,
    pub rotation: Rotation,
    pub primary: bool,
    /// Opaque, backend-specific custom display timing (e.g. an NVIDIA
    /// `NV_CUSTOM_DISPLAY` raw timing), preserved byte-for-byte but never
    /// interpreted by this pure module. Hex-encoded for stable, ASCII-safe
    /// (de)serialization, matching `recovery.rs`'s existing convention for
    /// opaque blobs.
    pub custom_timing_hex: Option<String>,
}

impl RecoveredOutputState {
    /// Builds a recovered path record from a raw opaque custom-timing blob
    /// (hex-encodes it internally) instead of an already-encoded string.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero mode dimension, zero refresh rate, or a
    /// custom timing blob over [`MAX_CUSTOM_TIMING_BYTES`].
    #[allow(clippy::too_many_arguments)] // Matches existing precedent in this crate (e.g. `session.rs`, `input.rs`) for state constructors mirroring a wire/native record.
    pub fn new(
        adapter_luid: AdapterLuid,
        target_id: u32,
        source_id: u32,
        mode_width: u32,
        mode_height: u32,
        refresh_hz: u32,
        position_x: i32,
        position_y: i32,
        rotation: Rotation,
        primary: bool,
        custom_timing: Option<&[u8]>,
    ) -> Result<Self, MultiMonitorRecoveryError> {
        if mode_width == 0 || mode_height == 0 {
            return Err(MultiMonitorRecoveryError::ZeroDimensionMode { target_id });
        }
        if refresh_hz == 0 {
            return Err(MultiMonitorRecoveryError::ZeroRefreshRate { target_id });
        }
        let custom_timing_hex = match custom_timing {
            Some(bytes) if bytes.len() > MAX_CUSTOM_TIMING_BYTES => {
                return Err(MultiMonitorRecoveryError::CustomTimingTooLarge {
                    target_id,
                    bytes: bytes.len(),
                });
            }
            Some(bytes) => Some(encode_hex(bytes)),
            None => None,
        };
        Ok(Self {
            adapter_luid,
            target_id,
            source_id,
            mode_width,
            mode_height,
            refresh_hz,
            position_x,
            position_y,
            rotation,
            primary,
            custom_timing_hex,
        })
    }

    /// Builds a recovered path record from one applied multi-monitor plan
    /// entry (see [`WindowsTopologyPlan`]), given the CCD source id it was
    /// (or will be) bound to. `custom_timing` stays `None` here: the pure
    /// topology planner never touches backend custom-timing state, only the
    /// live NVAPI capture layer (a future integration tranche) can supply it.
    ///
    /// # Errors
    ///
    /// Returns an error only if `monitor`'s mode/refresh rate were somehow
    /// zero — never true for a [`WindowsMonitorPlan`] produced by
    /// [`crate::multi_monitor_topology::plan_topology`], which already
    /// rejects zero-dimension/zero-refresh requests.
    pub fn from_monitor_plan(
        monitor: &WindowsMonitorPlan,
        source_id: u32,
    ) -> Result<Self, MultiMonitorRecoveryError> {
        Self::new(
            monitor.adapter_luid,
            monitor.target_id,
            source_id,
            monitor.mode_width,
            monitor.mode_height,
            monitor.refresh_hz,
            monitor.x,
            monitor.y,
            monitor.rotation,
            monitor.primary,
            None,
        )
    }

    /// Decodes [`Self::custom_timing_hex`] back to raw bytes.
    ///
    /// # Errors
    ///
    /// Returns [`MultiMonitorRecoveryError::InvalidCustomTimingHex`] when the
    /// stored hex is corrupt (odd length or a non-hex byte) — this is the
    /// module's corruption-rejection path for this field.
    pub fn decode_custom_timing(&self) -> Result<Option<Vec<u8>>, MultiMonitorRecoveryError> {
        match &self.custom_timing_hex {
            None => Ok(None),
            Some(hex) => {
                decode_hex(hex)
                    .map(Some)
                    .ok_or(MultiMonitorRecoveryError::InvalidCustomTimingHex {
                        target_id: self.target_id,
                    })
            }
        }
    }

    const fn output_key(&self) -> (u32, i32, u32) {
        (
            self.adapter_luid.low_part,
            self.adapter_luid.high_part,
            self.target_id,
        )
    }
}

/// A versioned, validated snapshot of every active display path on the
/// system at one point in time, sufficient to restore the full topology
/// (every path's source/target binding, mode, position, rotation, primary,
/// and opaque custom timing), tagged with the [`TopologyGeneration`] it
/// represents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiMonitorRecoverySnapshot {
    version: u32,
    generation: TopologyGeneration,
    paths: Vec<RecoveredOutputState>,
}

impl MultiMonitorRecoverySnapshot {
    /// Validates and builds a snapshot.
    ///
    /// # Errors
    ///
    /// Rejects an unsupported `version`, an empty or oversized `paths` list,
    /// and duplicate `(adapter_luid, target_id)` entries. Does **not**
    /// require exactly one path to be flagged primary: a real recovered
    /// snapshot may have zero or more than one due to partial capture or
    /// version skew, which is exactly why [`Self::safe_primary_index`] exists
    /// as a total, guaranteed fallback rather than a constructor precondition.
    pub fn new(
        version: u32,
        generation: TopologyGeneration,
        paths: Vec<RecoveredOutputState>,
    ) -> Result<Self, MultiMonitorRecoveryError> {
        if !(MIN_SUPPORTED_MULTI_MONITOR_RECOVERY_VERSION..=MULTI_MONITOR_RECOVERY_VERSION)
            .contains(&version)
        {
            return Err(MultiMonitorRecoveryError::UnsupportedVersion(version));
        }
        if paths.is_empty() {
            return Err(MultiMonitorRecoveryError::EmptyTopology);
        }
        if paths.len() > MAX_RECOVERY_PATHS {
            return Err(MultiMonitorRecoveryError::TooManyPaths { count: paths.len() });
        }
        let mut seen = std::collections::BTreeSet::new();
        for path in &paths {
            if !seen.insert(path.output_key()) {
                return Err(MultiMonitorRecoveryError::DuplicateOutputInTopology);
            }
        }
        Ok(Self {
            version,
            generation,
            paths,
        })
    }

    /// Converts an applied multi-monitor topology plan (item 1's planner
    /// output) into a recovery snapshot representing exactly the plan's
    /// monitors — a convenience for the common single-session case. A full
    /// system snapshot that also covers untouched sibling outputs outside
    /// the multi-monitor session must instead assemble its `paths` directly
    /// via [`Self::new`] (a future live-capture integration's job, out of
    /// scope this tranche).
    ///
    /// `source_ids` must have exactly one entry per `plan.monitors`, in the
    /// same order.
    ///
    /// # Errors
    ///
    /// Returns [`MultiMonitorRecoveryError::MismatchedSourceIdCount`] when
    /// `source_ids.len() != plan.monitors.len()`, or any error
    /// [`Self::new`]/[`RecoveredOutputState::from_monitor_plan`] can return.
    pub fn from_topology_plan(
        plan: &WindowsTopologyPlan,
        source_ids: &[u32],
    ) -> Result<Self, MultiMonitorRecoveryError> {
        if source_ids.len() != plan.monitors.len() {
            return Err(MultiMonitorRecoveryError::MismatchedSourceIdCount {
                paths: plan.monitors.len(),
                source_ids: source_ids.len(),
            });
        }
        let paths = plan
            .monitors
            .iter()
            .zip(source_ids.iter().copied())
            .map(|(monitor, source_id)| RecoveredOutputState::from_monitor_plan(monitor, source_id))
            .collect::<Result<Vec<_>, _>>()?;
        Self::new(MULTI_MONITOR_RECOVERY_VERSION, plan.generation, paths)
    }

    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    #[must_use]
    pub const fn generation(&self) -> TopologyGeneration {
        self.generation
    }

    #[must_use]
    pub fn paths(&self) -> &[RecoveredOutputState] {
        &self.paths
    }

    /// Guaranteed-safe primary selection: total and panic-free for any
    /// validated snapshot (which is always non-empty by construction).
    ///
    /// Returns the index of the single path flagged primary when there is
    /// exactly one. Otherwise — zero paths flagged primary (all cleared, or
    /// an older/partial capture never recorded one) or more than one flagged
    /// (corrupt/ambiguous) — deterministically falls back to path index `0`,
    /// the first active path in the snapshot, so restore always has exactly
    /// one primary to apply and never fails or picks inconsistently between
    /// runs.
    #[must_use]
    pub fn safe_primary_index(&self) -> usize {
        let mut primary_indices = self
            .paths
            .iter()
            .enumerate()
            .filter(|(_, path)| path.primary)
            .map(|(index, _)| index);
        match (primary_indices.next(), primary_indices.next()) {
            (Some(only), None) => only,
            _ => 0,
        }
    }

    /// The path [`Self::safe_primary_index`] selects.
    #[must_use]
    pub fn safe_primary(&self) -> &RecoveredOutputState {
        &self.paths[self.safe_primary_index()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luid(low_part: u32) -> AdapterLuid {
        AdapterLuid {
            low_part,
            high_part: 0,
        }
    }

    fn generation(value: u64) -> TopologyGeneration {
        TopologyGeneration::new(value).expect("nonzero generation")
    }

    fn path(target_id: u32, primary: bool) -> RecoveredOutputState {
        RecoveredOutputState::new(
            luid(1),
            target_id,
            target_id,
            1_920,
            1_080,
            60,
            0,
            0,
            Rotation::Degrees0,
            primary,
            None,
        )
        .expect("valid path")
    }

    #[test]
    fn new_rejects_an_unsupported_version() {
        let error = MultiMonitorRecoverySnapshot::new(0, generation(1), vec![path(0, true)])
            .expect_err("rejected");
        assert_eq!(error, MultiMonitorRecoveryError::UnsupportedVersion(0));

        let error = MultiMonitorRecoverySnapshot::new(
            MULTI_MONITOR_RECOVERY_VERSION + 1,
            generation(1),
            vec![path(0, true)],
        )
        .expect_err("rejected");
        assert_eq!(
            error,
            MultiMonitorRecoveryError::UnsupportedVersion(MULTI_MONITOR_RECOVERY_VERSION + 1)
        );
    }

    #[test]
    fn new_rejects_an_empty_topology() {
        let error =
            MultiMonitorRecoverySnapshot::new(1, generation(1), Vec::new()).expect_err("rejected");
        assert_eq!(error, MultiMonitorRecoveryError::EmptyTopology);
    }

    #[test]
    fn new_rejects_more_than_the_path_ceiling() {
        let paths = (0..=MAX_RECOVERY_PATHS as u32)
            .map(|target_id| path(target_id, target_id == 0))
            .collect::<Vec<_>>();
        let count = paths.len();
        let error =
            MultiMonitorRecoverySnapshot::new(1, generation(1), paths).expect_err("rejected");
        assert_eq!(error, MultiMonitorRecoveryError::TooManyPaths { count });
    }

    #[test]
    fn new_rejects_a_duplicate_output_binding() {
        let error = MultiMonitorRecoverySnapshot::new(
            1,
            generation(1),
            vec![path(0, true), path(0, false)],
        )
        .expect_err("rejected");
        assert_eq!(error, MultiMonitorRecoveryError::DuplicateOutputInTopology);
    }

    #[test]
    fn recovered_output_state_rejects_zero_mode_dimensions_and_refresh() {
        let error = RecoveredOutputState::new(
            luid(1),
            0,
            0,
            0,
            1_080,
            60,
            0,
            0,
            Rotation::Degrees0,
            true,
            None,
        )
        .expect_err("rejected");
        assert_eq!(
            error,
            MultiMonitorRecoveryError::ZeroDimensionMode { target_id: 0 }
        );

        let error = RecoveredOutputState::new(
            luid(1),
            0,
            0,
            1_920,
            1_080,
            0,
            0,
            0,
            Rotation::Degrees0,
            true,
            None,
        )
        .expect_err("rejected");
        assert_eq!(
            error,
            MultiMonitorRecoveryError::ZeroRefreshRate { target_id: 0 }
        );
    }

    #[test]
    fn custom_timing_round_trips_through_hex_encoding() {
        let raw = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let recovered = RecoveredOutputState::new(
            luid(1),
            0,
            0,
            1_920,
            1_080,
            60,
            0,
            0,
            Rotation::Degrees0,
            true,
            Some(&raw),
        )
        .expect("valid path");
        assert_eq!(recovered.custom_timing_hex.as_deref(), Some("deadbeef"));
        assert_eq!(
            recovered.decode_custom_timing().expect("decodes"),
            Some(raw)
        );
    }

    #[test]
    fn custom_timing_rejects_an_oversized_blob() {
        let raw = vec![0u8; MAX_CUSTOM_TIMING_BYTES + 1];
        let error = RecoveredOutputState::new(
            luid(1),
            0,
            0,
            1_920,
            1_080,
            60,
            0,
            0,
            Rotation::Degrees0,
            true,
            Some(&raw),
        )
        .expect_err("rejected");
        assert_eq!(
            error,
            MultiMonitorRecoveryError::CustomTimingTooLarge {
                target_id: 0,
                bytes: MAX_CUSTOM_TIMING_BYTES + 1
            }
        );
    }

    #[test]
    fn decode_custom_timing_rejects_corrupt_hex() {
        let mut recovered = path(0, true);
        recovered.custom_timing_hex = Some("not-hex!".to_owned());
        let error = recovered.decode_custom_timing().expect_err("rejected");
        assert_eq!(
            error,
            MultiMonitorRecoveryError::InvalidCustomTimingHex { target_id: 0 }
        );

        let mut odd_length = path(0, true);
        odd_length.custom_timing_hex = Some("abc".to_owned());
        let error = odd_length.decode_custom_timing().expect_err("rejected");
        assert_eq!(
            error,
            MultiMonitorRecoveryError::InvalidCustomTimingHex { target_id: 0 }
        );
    }

    #[test]
    fn safe_primary_index_picks_the_single_flagged_primary() {
        let snapshot = MultiMonitorRecoverySnapshot::new(
            1,
            generation(1),
            vec![path(0, false), path(1, true), path(2, false)],
        )
        .expect("valid snapshot");
        assert_eq!(snapshot.safe_primary_index(), 1);
        assert_eq!(snapshot.safe_primary().target_id, 1);
    }

    #[test]
    fn safe_primary_index_falls_back_to_the_first_path_when_none_are_flagged() {
        let snapshot = MultiMonitorRecoverySnapshot::new(
            1,
            generation(1),
            vec![path(0, false), path(1, false)],
        )
        .expect("valid snapshot");
        assert_eq!(snapshot.safe_primary_index(), 0);
    }

    #[test]
    fn safe_primary_index_falls_back_to_the_first_path_when_more_than_one_is_flagged() {
        let snapshot =
            MultiMonitorRecoverySnapshot::new(1, generation(1), vec![path(0, true), path(1, true)])
                .expect("valid snapshot");
        // Corrupt/ambiguous input (two primaries flagged) still yields a
        // deterministic, in-bounds, "safe" choice rather than panicking or
        // picking inconsistently.
        assert_eq!(snapshot.safe_primary_index(), 0);
    }

    fn sample_monitor_plan(target_id: u32, primary: bool) -> WindowsMonitorPlan {
        WindowsMonitorPlan {
            session_monitor_id: arcen_media::SessionMonitorId::new(
                u16::try_from(target_id + 1).expect("fits u16"),
            )
            .expect("nonzero"),
            client_display_id: format!("display-{target_id}"),
            adapter_luid: luid(1),
            target_id,
            adapter_output_index: target_id,
            adapter_name: "Test Adapter".to_owned(),
            global_index: target_id,
            device_name: format!(r"\\.\DISPLAY{}", target_id + 1),
            x: 0,
            y: 0,
            width: 1_920,
            height: 1_080,
            mode_width: 1_920,
            mode_height: 1_080,
            logical_rect: arcen_media::LogicalRect::new(
                arcen_media::LogicalPoint::new(0, 0),
                arcen_media::LogicalSize::from_pixels(1_920, 1_080).expect("logical size"),
            )
            .expect("logical rect"),
            scale: arcen_media::Scale120::new(120).expect("scale"),
            refresh_hz: 60,
            rotation: Rotation::Degrees0,
            primary,
        }
    }

    #[test]
    fn from_topology_plan_converts_every_monitor_and_preserves_its_fields() {
        let plan = WindowsTopologyPlan {
            generation: generation(7),
            desktop_x: 0,
            desktop_y: 0,
            desktop_width: 3_840,
            desktop_height: 1_080,
            monitors: vec![sample_monitor_plan(0, true), sample_monitor_plan(1, false)],
            requires_custom_timing: false,
        };
        let snapshot =
            MultiMonitorRecoverySnapshot::from_topology_plan(&plan, &[10, 11]).expect("converts");
        assert_eq!(snapshot.version(), MULTI_MONITOR_RECOVERY_VERSION);
        assert_eq!(snapshot.generation(), generation(7));
        assert_eq!(snapshot.paths().len(), 2);
        assert_eq!(snapshot.paths()[0].source_id, 10);
        assert_eq!(snapshot.paths()[1].source_id, 11);
        assert_eq!(snapshot.paths()[0].target_id, 0);
        assert!(snapshot.paths()[0].primary);
        assert!(!snapshot.paths()[1].primary);
        assert_eq!(snapshot.safe_primary_index(), 0);
    }

    #[test]
    fn from_topology_plan_rejects_a_mismatched_source_id_count() {
        let plan = WindowsTopologyPlan {
            generation: generation(1),
            desktop_x: 0,
            desktop_y: 0,
            desktop_width: 1_920,
            desktop_height: 1_080,
            monitors: vec![sample_monitor_plan(0, true)],
            requires_custom_timing: false,
        };
        let error =
            MultiMonitorRecoverySnapshot::from_topology_plan(&plan, &[]).expect_err("rejected");
        assert_eq!(
            error,
            MultiMonitorRecoveryError::MismatchedSourceIdCount {
                paths: 1,
                source_ids: 0
            }
        );
    }
}
