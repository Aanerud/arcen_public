// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Whether this host's Mutter applies its tablet pressure curve once: the
//! platform half of `arcen_input::compositor_pressure`. It reads the
//! compositor prefix's [`PressureCurveMarker`] and hashes the library the
//! marker names; the shared marker decides. Anything missing, unreadable or
//! not matching keeps the version's transfer (the inverse on Mutter 46+).

use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use arcen_input::compositor_pressure::{
    CompositorPressure, PressureCurveMarker, PRESSURE_CURVE_MARKER, PRESSURE_CURVE_MARKER_MAX_BYTES,
};
use arcen_media::video::compositor_fidelity::CompositorRelease;
use sha2::{Digest, Sha256};

use crate::logging::target;

/// The largest library hashed (libmutter is about 4 MiB).
const LIBRARY_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// What a desktop's seat pen must invert, decided when its desktop is
/// created ([`SeatPenPressurePlan::resolve`]), so a library swapped or
/// restored while the Pier runs counts from the next desktop on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SeatPenPressurePlan {
    /// The qualified compositor's version; `None` for an X server or a
    /// host without a Wayland record.
    compositor_version: Option<String>,
}

/// What [`SeatPenPressurePlan::resolve`] found in the prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CurveFinding {
    /// The version's transfer is linear; nothing to look for.
    NotNeeded,
    /// The prefix has no marker.
    NoMarker,
    /// A marker that matches the library and the version.
    Proven,
    /// A marker that does not prove the fix, and why.
    Refused(String),
}

impl SeatPenPressurePlan {
    /// No compositor curve (Xorg, no Wayland record).
    #[must_use]
    pub const fn linear() -> Self {
        Self {
            compositor_version: None,
        }
    }

    /// A Wayland desktop on the qualified compositor `compositor_version`.
    #[must_use]
    pub fn compositor(compositor_version: &str) -> Self {
        Self {
            compositor_version: Some(compositor_version.to_owned()),
        }
    }

    /// The transfer the compositor's version alone implies.
    #[must_use]
    pub fn by_version(&self) -> CompositorPressure {
        self.compositor_version
            .as_deref()
            .map_or(CompositorPressure::Linear, |version| {
                CompositorPressure::for_compositor(CompositorRelease::parse_mutter(version))
            })
    }

    /// The transfer the seat pen inverts, after looking for the prefix's
    /// marker. Blocking (reads and hashes a file): call it where the seat
    /// pen is built.
    #[must_use]
    pub fn resolve(&self) -> CompositorPressure {
        let prefix = crate::session::wayland::compositor_prefix();
        let (pressure, finding) = self.resolve_in(Path::new(prefix.as_str()), 0);
        match &finding {
            CurveFinding::NotNeeded => {}
            CurveFinding::Proven => tracing::info!(
                target: target::INPUT,
                prefix = %prefix,
                marker = PRESSURE_CURVE_MARKER,
                "the compositor applies its tablet pressure curve once (patched Mutter, marker matches its library); the seat pen inverts libinput's threshold only"
            ),
            CurveFinding::NoMarker => tracing::info!(
                target: target::INPUT,
                prefix = %prefix,
                "the compositor squares tablet pressure (Mutter 46+, GNOME/mutter#5077); the seat pen writes the inverse"
            ),
            CurveFinding::Refused(reason) => tracing::warn!(
                target: target::INPUT,
                prefix = %prefix,
                reason = %reason,
                "the compositor prefix's pressure-curve marker proves nothing; the seat pen keeps inverting Mutter's squared pressure"
            ),
        }
        pressure
    }

    /// [`Self::resolve`] for the prefix at `prefix`, whose marker and
    /// library must be owned by `owner` (root in the Pier).
    fn resolve_in(&self, prefix: &Path, owner: u32) -> (CompositorPressure, CurveFinding) {
        let by_version = self.by_version();
        let (Some(version), CompositorPressure::MutterSquared) =
            (self.compositor_version.as_deref(), by_version)
        else {
            return (by_version, CurveFinding::NotNeeded);
        };
        let finding = match curve_marker(prefix, version, owner) {
            Ok(false) => CurveFinding::NoMarker,
            Ok(true) => CurveFinding::Proven,
            Err(reason) => CurveFinding::Refused(reason),
        };
        let release = CompositorRelease::parse_mutter(version);
        let pressure =
            CompositorPressure::for_compositor_build(release, finding == CurveFinding::Proven);
        (pressure, finding)
    }
}

/// `Ok(false)` without a marker, `Ok(true)` for a marker that matches.
fn curve_marker(prefix: &Path, version: &str, owner: u32) -> Result<bool, String> {
    let path = prefix.join(PRESSURE_CURVE_MARKER);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    trusted_file(&path, &metadata, owner)?;
    if metadata.len() > PRESSURE_CURVE_MARKER_MAX_BYTES {
        return Err(format!("{} is larger than its limit", path.display()));
    }
    let mut text = String::new();
    std::fs::File::open(&path)
        .and_then(|file| {
            file.take(PRESSURE_CURVE_MARKER_MAX_BYTES)
                .read_to_string(&mut text)
        })
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let marker = PressureCurveMarker::parse(&text).map_err(|error| error.to_string())?;
    let library = library_inside(prefix, marker.library())?;
    let digest = sha256_file(&library, owner)?;
    marker
        .check(version, &digest)
        .map(|()| true)
        .map_err(|error| error.to_string())
}

/// The real path of `relative` in `prefix`, refused when a link leads
/// outside the prefix.
fn library_inside(prefix: &Path, relative: &str) -> Result<PathBuf, String> {
    let root =
        std::fs::canonicalize(prefix).map_err(|error| format!("{}: {error}", prefix.display()))?;
    let library = std::fs::canonicalize(prefix.join(relative))
        .map_err(|error| format!("{relative}: {error}"))?;
    if library.starts_with(&root) {
        Ok(library)
    } else {
        Err(format!("{relative} leads outside the prefix"))
    }
}

/// A regular file owned by `owner` and writable by no one else.
fn trusted_file(path: &Path, metadata: &std::fs::Metadata, owner: u32) -> Result<(), String> {
    if !metadata.file_type().is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if metadata.uid() != owner || metadata.mode() & 0o022 != 0 {
        return Err(format!(
            "{} is not owned by uid {owner} or is writable by others",
            path.display()
        ));
    }
    Ok(())
}

fn sha256_file(path: &Path, owner: u32) -> Result<String, String> {
    let metadata =
        std::fs::metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    trusted_file(path, &metadata, owner)?;
    if metadata.len() > LIBRARY_MAX_BYTES {
        return Err(format!("{} is larger than its limit", path.display()));
    }
    let mut file =
        std::fs::File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Prefix(PathBuf);

    impl Prefix {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "arcen-pressure-curve-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("lib64")).unwrap();
            std::fs::create_dir_all(root.join("share/arcen")).unwrap();
            std::fs::write(root.join("lib64/libmutter-51.so.0.0.0"), b"patched mutter").unwrap();
            std::os::unix::fs::symlink(
                "libmutter-51.so.0.0.0",
                root.join("lib64/libmutter-51.so.0"),
            )
            .unwrap();
            Self(root)
        }

        fn marker(&self, version: &str, library: &str) {
            let digest = format!("{:x}", Sha256::digest(b"patched mutter"));
            std::fs::write(
                self.0.join(PRESSURE_CURVE_MARKER),
                format!(
                    "arcen-mutter-pressure-curve=1\nmutter-version={version}\nlibrary={library}\nlibrary-sha256={digest}\n"
                ),
            )
            .unwrap();
        }

        fn resolve(&self, plan: &SeatPenPressurePlan) -> (CompositorPressure, CurveFinding) {
            plan.resolve_in(&self.0, nix::unistd::geteuid().as_raw())
        }
    }

    impl Drop for Prefix {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_matching_marker_turns_the_inverse_off() {
        let prefix = Prefix::new("match");
        prefix.marker("51.1", "lib64/libmutter-51.so.0");
        assert_eq!(
            prefix.resolve(&SeatPenPressurePlan::compositor("51.1")),
            (CompositorPressure::Linear, CurveFinding::Proven)
        );
    }

    #[test]
    fn without_a_marker_or_with_a_stale_one_the_inverse_stays() {
        let plan = SeatPenPressurePlan::compositor("51.1");
        let prefix = Prefix::new("stale");
        assert_eq!(
            prefix.resolve(&plan),
            (CompositorPressure::MutterSquared, CurveFinding::NoMarker)
        );
        prefix.marker("51.1", "lib64/libmutter-51.so.0");
        // The unpatched library restored over the patched one.
        std::fs::write(
            prefix.0.join("lib64/libmutter-51.so.0.0.0"),
            b"stock mutter",
        )
        .unwrap();
        let (pressure, finding) = prefix.resolve(&plan);
        assert_eq!(pressure, CompositorPressure::MutterSquared);
        assert!(
            matches!(&finding, CurveFinding::Refused(reason) if reason.contains("changed")),
            "{finding:?}"
        );
        // Another Mutter in the prefix.
        let (pressure, _) = prefix.resolve(&SeatPenPressurePlan::compositor("52.0"));
        assert_eq!(pressure, CompositorPressure::MutterSquared);
    }

    #[test]
    fn a_marker_cannot_point_outside_the_prefix_or_be_writable() {
        let plan = SeatPenPressurePlan::compositor("51.1");
        let prefix = Prefix::new("outside");
        let outside = Prefix::new("outside-target");
        std::os::unix::fs::symlink(
            outside.0.join("lib64/libmutter-51.so.0.0.0"),
            prefix.0.join("lib64/escape.so"),
        )
        .unwrap();
        prefix.marker("51.1", "lib64/escape.so");
        let (pressure, finding) = prefix.resolve(&plan);
        assert_eq!(pressure, CompositorPressure::MutterSquared);
        assert!(matches!(finding, CurveFinding::Refused(_)), "{finding:?}");

        prefix.marker("51.1", "lib64/libmutter-51.so.0");
        let marker = prefix.0.join(PRESSURE_CURVE_MARKER);
        std::fs::set_permissions(&marker, std::os::unix::fs::PermissionsExt::from_mode(0o666))
            .unwrap();
        let (pressure, finding) = prefix.resolve(&plan);
        assert_eq!(pressure, CompositorPressure::MutterSquared);
        assert!(matches!(finding, CurveFinding::Refused(_)), "{finding:?}");
    }

    #[test]
    fn x11_and_older_mutter_never_look_for_a_marker() {
        let prefix = Prefix::new("linear");
        prefix.marker("45.0", "lib64/libmutter-51.so.0");
        for plan in [
            SeatPenPressurePlan::linear(),
            SeatPenPressurePlan::compositor("45.0"),
        ] {
            assert_eq!(
                prefix.resolve(&plan),
                (CompositorPressure::Linear, CurveFinding::NotNeeded)
            );
        }
    }
}
