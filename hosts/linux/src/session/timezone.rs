//! Linux zoneinfo-backed validation for process-scoped time-zone redirection.

use std::path::Path;

use arcen_session::restore_lease::IanaTimeZone;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TimezoneValidationError {
    #[error("invalid IANA time-zone identifier")]
    InvalidIdentifier,
    #[error("alternate posix/right zoneinfo trees are not accepted")]
    AlternateTree,
    #[error("zoneinfo root is unavailable")]
    RootUnavailable,
    #[error("time-zone entry is unavailable")]
    EntryUnavailable,
    #[error("time-zone entry escapes the zoneinfo root")]
    EscapesRoot,
    #[error("time-zone entry is not a regular file")]
    NotRegularFile,
}

/// Validates syntax and resolves an identifier to a regular file contained by
/// the canonical zoneinfo root. File contents are never read.
pub fn validate_zoneinfo_timezone(
    zoneinfo_root: &Path,
    requested: &str,
) -> Result<IanaTimeZone, TimezoneValidationError> {
    let timezone =
        IanaTimeZone::parse(requested).map_err(|_| TimezoneValidationError::InvalidIdentifier)?;
    if timezone
        .as_str()
        .split('/')
        .any(|segment| matches!(segment, "posix" | "right"))
    {
        return Err(TimezoneValidationError::AlternateTree);
    }

    let canonical_root = std::fs::canonicalize(zoneinfo_root)
        .map_err(|_| TimezoneValidationError::RootUnavailable)?;
    if !canonical_root.is_dir() {
        return Err(TimezoneValidationError::RootUnavailable);
    }
    let canonical_target = std::fs::canonicalize(canonical_root.join(timezone.as_str()))
        .map_err(|_| TimezoneValidationError::EntryUnavailable)?;
    if !canonical_target.starts_with(&canonical_root) {
        return Err(TimezoneValidationError::EscapesRoot);
    }
    if !canonical_target.is_file() {
        return Err(TimezoneValidationError::NotRegularFile);
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
            let root = std::env::temp_dir()
                .join(format!("arcen-linux-zoneinfo-{}-{id}", std::process::id()));
            std::fs::create_dir_all(root.join("Europe")).unwrap();
            std::fs::create_dir_all(root.join("posix/Europe")).unwrap();
            std::fs::create_dir_all(root.join("right/Europe")).unwrap();
            std::fs::write(root.join("Europe/Oslo"), b"fixture").unwrap();
            std::fs::write(root.join("posix/Europe/Oslo"), b"fixture").unwrap();
            std::fs::write(root.join("right/Europe/Oslo"), b"fixture").unwrap();
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
            Err(TimezoneValidationError::RootUnavailable)
        ));
        assert!(matches!(
            validate_zoneinfo_timezone(&fixture.root, "Europe/Missing"),
            Err(TimezoneValidationError::EntryUnavailable)
        ));
        assert!(matches!(
            validate_zoneinfo_timezone(&fixture.root, "Europe"),
            Err(TimezoneValidationError::NotRegularFile)
        ));
        assert!(matches!(
            validate_zoneinfo_timezone(&fixture.root, "posix/Europe/Oslo"),
            Err(TimezoneValidationError::AlternateTree)
        ));
        assert!(matches!(
            validate_zoneinfo_timezone(&fixture.root, "right/Europe/Oslo"),
            Err(TimezoneValidationError::AlternateTree)
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
            .join(format!("arcen-zoneinfo-outside-{}", std::process::id()));
        std::fs::write(&outside, b"outside").unwrap();
        symlink(&outside, fixture.root.join("Europe/Escape")).unwrap();
        assert!(matches!(
            validate_zoneinfo_timezone(&fixture.root, "Europe/Escape"),
            Err(TimezoneValidationError::EscapesRoot)
        ));
        let _ = std::fs::remove_file(outside);
    }
}
