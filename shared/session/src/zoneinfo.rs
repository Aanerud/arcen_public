//! Zoneinfo-backed validation for IANA time-zone redirection.

use std::path::Path;

use crate::restore_lease::IanaTimeZone;

/// Zoneinfo validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneinfoValidationError {
    /// The supplied identifier failed the shared path-safe IANA syntax check.
    InvalidIdentifier,
    /// The alternate `posix` and `right` trees are intentionally unsupported.
    AlternateTree,
    /// The configured zoneinfo root is absent or not a directory.
    RootUnavailable,
    /// The requested zoneinfo entry does not exist.
    EntryUnavailable,
    /// The requested entry resolves outside the trusted zoneinfo root.
    EscapesRoot,
    /// The requested entry is not a regular file.
    NotRegularFile,
}

impl std::fmt::Display for ZoneinfoValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidIdentifier => "invalid IANA time-zone identifier",
            Self::AlternateTree => "alternate posix/right zoneinfo trees are not accepted",
            Self::RootUnavailable => "zoneinfo root is unavailable",
            Self::EntryUnavailable => "time-zone entry is unavailable",
            Self::EscapesRoot => "time-zone entry escapes the zoneinfo root",
            Self::NotRegularFile => "time-zone entry is not a regular file",
        })
    }
}

impl std::error::Error for ZoneinfoValidationError {}

/// Validates syntax and resolves an identifier to a regular file contained by
/// the canonical zoneinfo root. File contents are never read.
///
/// # Errors
///
/// Returns an error when the identifier is malformed, refers to an alternate
/// tree, is missing, is not a file, or resolves outside `zoneinfo_root`.
pub fn validate_zoneinfo_timezone(
    zoneinfo_root: &Path,
    requested: &str,
) -> Result<IanaTimeZone, ZoneinfoValidationError> {
    let timezone =
        IanaTimeZone::parse(requested).map_err(|_| ZoneinfoValidationError::InvalidIdentifier)?;
    if timezone
        .as_str()
        .split('/')
        .any(|segment| matches!(segment, "posix" | "right"))
    {
        return Err(ZoneinfoValidationError::AlternateTree);
    }

    let canonical_root = std::fs::canonicalize(zoneinfo_root)
        .map_err(|_| ZoneinfoValidationError::RootUnavailable)?;
    if !canonical_root.is_dir() {
        return Err(ZoneinfoValidationError::RootUnavailable);
    }
    let canonical_target = std::fs::canonicalize(canonical_root.join(timezone.as_str()))
        .map_err(|_| ZoneinfoValidationError::EntryUnavailable)?;
    if !canonical_target.starts_with(&canonical_root) {
        return Err(ZoneinfoValidationError::EscapesRoot);
    }
    if !canonical_target.is_file() {
        return Err(ZoneinfoValidationError::NotRegularFile);
    }
    Ok(timezone)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let id = FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
            let base = std::env::var_os("CARGO_TARGET_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("target"));
            let root = base
                .join("arcen-test-fixtures")
                .join(format!("zoneinfo-{}-{id}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("Europe")).unwrap();
            std::fs::create_dir_all(root.join("posix/Europe")).unwrap();
            std::fs::create_dir_all(root.join("right/Europe")).unwrap();
            std::fs::write(root.join("Europe/Oslo"), b"fixture").unwrap();
            std::fs::write(root.join("posix/Europe/Oslo"), b"fixture").unwrap();
            std::fs::write(root.join("right/Europe/Oslo"), b"fixture").unwrap();
            // Symlink targets in the tests are absolute only when the root is.
            let root = std::fs::canonicalize(&root).unwrap();
            Self { root }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn accepts_regular_zoneinfo_file() {
        let fixture = Fixture::new();
        let timezone = validate_zoneinfo_timezone(&fixture.root, "Europe/Oslo").unwrap();
        assert_eq!(timezone.as_str(), "Europe/Oslo");
    }

    #[test]
    fn rejects_missing_directory_and_alternate_trees() {
        let fixture = Fixture::new();
        let root_file = fixture.root.join("not-a-root");
        std::fs::write(&root_file, b"fixture").unwrap();
        assert!(matches!(
            validate_zoneinfo_timezone(&root_file, "Europe/Oslo"),
            Err(ZoneinfoValidationError::RootUnavailable)
        ));
        assert!(matches!(
            validate_zoneinfo_timezone(&fixture.root, "Europe/Missing"),
            Err(ZoneinfoValidationError::EntryUnavailable)
        ));
        assert!(matches!(
            validate_zoneinfo_timezone(&fixture.root, "Europe"),
            Err(ZoneinfoValidationError::NotRegularFile)
        ));
        assert!(matches!(
            validate_zoneinfo_timezone(&fixture.root, "posix/Europe/Oslo"),
            Err(ZoneinfoValidationError::AlternateTree)
        ));
        assert!(matches!(
            validate_zoneinfo_timezone(&fixture.root, "right/Europe/Oslo"),
            Err(ZoneinfoValidationError::AlternateTree)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let outside = fixture
            .root
            .parent()
            .unwrap()
            .join(format!("zoneinfo-outside-{}", std::process::id()));
        std::fs::write(&outside, b"outside").unwrap();
        symlink(&outside, fixture.root.join("Europe/Escape")).unwrap();
        assert!(matches!(
            validate_zoneinfo_timezone(&fixture.root, "Europe/Escape"),
            Err(ZoneinfoValidationError::EscapesRoot)
        ));
        let _ = std::fs::remove_file(outside);
    }
}
