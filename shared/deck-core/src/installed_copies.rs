// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Other copies of this Deck on the machine.
//!
//! An old copy left beside the installed one (a download folder, a mounted
//! disk image) shares the application's identity, so the operating system
//! may open it from a link, a file or a search instead of the installed one,
//! and the user runs a version that no longer matches what they installed.
//! The platform lists the copies it knows (macOS: Launch Services and the
//! running applications of the bundle identifier; Windows: the running
//! `arcen-deck.exe` processes and the installed package's copy); this
//! decides which of them to warn about and how.

/// One copy of the Deck the platform knows of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledCopy {
    /// Where it is (the application bundle or executable).
    pub path: String,
    /// Its version, when it can be read.
    pub version: Option<String>,
    /// Its build number, when it can be read.
    pub build: Option<String>,
    /// Whether it is running now.
    pub running: bool,
}

/// The running Deck itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThisCopy<'a> {
    pub path: &'a str,
    pub version: &'a str,
    pub build: &'a str,
}

/// A copy worth telling the user about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyFinding {
    /// Another copy is running now, from another place.
    RunningElsewhere {
        path: String,
        version: Option<String>,
    },
    /// Another copy of a different (or unreadable) version is installed;
    /// the system may open it instead of this one.
    OtherVersion {
        path: String,
        version: Option<String>,
    },
}

impl CopyFinding {
    /// The copy's path.
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Self::RunningElsewhere { path, .. } | Self::OtherVersion { path, .. } => path,
        }
    }
}

/// Whether two paths name the same place: separators of either kind, a
/// trailing separator and letter case aside (the default macOS and Windows
/// file systems ignore case).
fn same_place(left: &str, right: &str) -> bool {
    let canonical = |path: &str| {
        path.trim_end_matches(['/', '\\'])
            .chars()
            .map(|character| match character {
                '\\' => '/',
                other => other.to_ascii_lowercase(),
            })
            .collect::<String>()
    };
    canonical(left) == canonical(right)
}

/// A copy in the Trash or the Recycle Bin is not opened by the system.
fn in_trash(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path.contains("/.trash/") || path.contains("/.trashes/") || path.contains("\\$recycle.bin\\")
}

/// The copies to warn about, running ones first: every copy elsewhere that is running, and every
/// copy elsewhere whose version or build differs from this one's. A second
/// copy of exactly this build is harmless and not reported; this copy and
/// the Trash's are never reported.
#[must_use]
pub fn stale_copies(this: &ThisCopy<'_>, copies: &[InstalledCopy]) -> Vec<CopyFinding> {
    let mut findings: Vec<CopyFinding> = Vec::new();
    // Running copies first: a path both listed and running is reported as
    // running.
    let running = |copy: &&InstalledCopy| copy.running;
    for copy in copies
        .iter()
        .filter(running)
        .chain(copies.iter().filter(|copy| !running(copy)))
    {
        if same_place(&copy.path, this.path) || in_trash(&copy.path) {
            continue;
        }
        if findings
            .iter()
            .any(|finding| same_place(finding.path(), &copy.path))
        {
            continue;
        }
        if copy.running {
            findings.push(CopyFinding::RunningElsewhere {
                path: copy.path.clone(),
                version: copy.version.clone(),
            });
        } else if copy.version.as_deref() != Some(this.version)
            || copy
                .build
                .as_deref()
                .is_some_and(|build| build != this.build)
        {
            findings.push(CopyFinding::OtherVersion {
                path: copy.path.clone(),
                version: copy.version.clone(),
            });
        }
    }
    findings
}

/// One line for the user, naming the most important copy; `None` when
/// there is nothing to say.
#[must_use]
pub fn startup_notice(this_version: &str, findings: &[CopyFinding]) -> Option<String> {
    let first = findings.first()?;
    let more = match findings.len() - 1 {
        0 => String::new(),
        1 => " (and 1 more copy)".to_owned(),
        others => format!(" (and {others} more copies)"),
    };
    let version = |version: &Option<String>| {
        version.as_deref().map_or_else(
            || "unknown version".to_owned(),
            |version| format!("version {version}"),
        )
    };
    Some(match first {
        CopyFinding::RunningElsewhere {
            path, version: v, ..
        } => format!(
            "Another Arcen Deck ({}) is running from {path}{more}. This is version {this_version}; quit the other copy.",
            version(v)
        ),
        CopyFinding::OtherVersion { path, version: v } => format!(
            "Another Arcen Deck ({}) is at {path}{more} and may open instead of this one (version {this_version}). Remove it to keep running this version.",
            version(v)
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const THIS: ThisCopy<'static> = ThisCopy {
        path: "/Applications/Arcen Deck.app",
        version: "0.20.2",
        build: "20261117",
    };

    fn copy(path: &str, version: &str, build: &str, running: bool) -> InstalledCopy {
        InstalledCopy {
            path: path.to_owned(),
            version: Some(version.to_owned()),
            build: Some(build.to_owned()),
            running,
        }
    }

    #[test]
    fn this_copy_the_trash_and_an_identical_build_are_not_reported() {
        let copies = [
            copy("/Applications/Arcen Deck.app/", "0.20.2", "20261117", true),
            copy("/Users/u/.Trash/Arcen Deck.app", "0.18.0", "1", false),
            copy(
                "/Users/u/Desktop/Arcen Deck.app",
                "0.20.2",
                "20261117",
                false,
            ),
        ];
        assert!(stale_copies(&THIS, &copies).is_empty());
        assert_eq!(startup_notice("0.20.2", &[]), None);
    }

    #[test]
    fn an_older_copy_elsewhere_and_a_running_copy_are_reported_running_first() {
        let copies = [
            copy(
                "/Users/u/Downloads/Arcen Deck.app",
                "0.19.1",
                "20261020",
                false,
            ),
            copy(
                "/Volumes/Arcen Deck/Arcen Deck.app",
                "0.20.2",
                "20261117",
                true,
            ),
            InstalledCopy {
                path: "/Users/u/Apps/Arcen Deck.app".to_owned(),
                version: None,
                build: None,
                running: false,
            },
            copy(
                "/Users/u/Downloads/Arcen Deck.app",
                "0.19.1",
                "20261020",
                true,
            ),
        ];
        let findings = stale_copies(&THIS, &copies);
        assert_eq!(
            findings,
            [
                CopyFinding::RunningElsewhere {
                    path: "/Volumes/Arcen Deck/Arcen Deck.app".to_owned(),
                    version: Some("0.20.2".to_owned()),
                },
                CopyFinding::RunningElsewhere {
                    path: "/Users/u/Downloads/Arcen Deck.app".to_owned(),
                    version: Some("0.19.1".to_owned()),
                },
                CopyFinding::OtherVersion {
                    path: "/Users/u/Apps/Arcen Deck.app".to_owned(),
                    version: None,
                },
            ]
        );
        let notice = startup_notice("0.20.2", &findings).unwrap();
        assert!(
            notice.contains("running from /Volumes/Arcen Deck/Arcen Deck.app"),
            "{notice}"
        );
        assert!(notice.contains("(and 2 more copies)"), "{notice}");
        let unknown = startup_notice("0.20.2", &findings[2..]).unwrap();
        assert!(
            unknown.contains("(unknown version) is at /Users/u/Apps"),
            "{unknown}"
        );
        assert!(unknown.contains("may open instead"), "{unknown}");
        let older = [copy(
            "/Users/u/Downloads/Arcen Deck.app",
            "0.19.1",
            "1",
            false,
        )];
        let older = startup_notice("0.20.2", &stale_copies(&THIS, &older)).unwrap();
        assert!(older.contains("(version 0.19.1) is at"), "{older}");
    }

    #[test]
    fn windows_paths_compare_without_case_and_the_recycle_bin_is_skipped() {
        let this = ThisCopy {
            path: r"C:\Program Files\Arcen\Deck\arcen-deck.exe",
            version: "0.20.2",
            build: "20261117",
        };
        let copies = [
            copy(
                r"c:\program files\arcen\deck\ARCEN-DECK.EXE",
                "0.20.2",
                "20261117",
                true,
            ),
            copy(
                r"C:\$Recycle.Bin\S-1-5-21\arcen-deck.exe",
                "0.19.1",
                "1",
                true,
            ),
            copy(r"~\Downloads\arcen-deck.exe", "0.19.1", "1", true),
        ];
        assert_eq!(
            stale_copies(&this, &copies),
            [CopyFinding::RunningElsewhere {
                path: r"~\Downloads\arcen-deck.exe".to_owned(),
                version: Some("0.19.1".to_owned()),
            }]
        );
    }

    #[test]
    fn a_same_version_copy_of_another_build_is_reported() {
        let copies = [copy(
            "/Users/u/Desktop/Arcen Deck.app",
            "0.20.2",
            "20261116",
            false,
        )];
        assert_eq!(stale_copies(&THIS, &copies).len(), 1);
    }
}
