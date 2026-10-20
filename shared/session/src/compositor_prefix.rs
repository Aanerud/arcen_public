//! Where the headless Wayland compositor is installed.
//!
//! A Linux Pier runs its headless GNOME session from the distribution's
//! `/usr` by default. An operator may install a newer GNOME Shell and Mutter
//! side by side in their own prefix (for example `/opt/arcen-gnome/51` on an
//! enterprise distribution that ships an older GNOME) and name it in
//! `capture.wayland_compositor_prefix`. Only Arcen's sessions use it; the
//! system desktop, the display manager and every other program keep `/usr`.
//!
//! This module is the pure policy: which strings name a prefix, where the
//! compositor binary and data directory are inside it, what must hold of
//! every file in it before a root service starts programs from it
//! ([`verify_prefix`]), and how the session's `XDG_DATA_DIRS` puts the
//! prefix first. The platform adapter only answers `lstat`, `readdir` and
//! `realpath` ([`PrefixInspector`]).

use std::collections::BTreeSet;

use std::fmt::{Display, Formatter};

use crate::wayland_session::HeadlessCompositor;

/// The distribution's own prefix, the default.
pub const SYSTEM_PREFIX: &str = "/usr";

/// The longest prefix accepted, in bytes.
pub const MAX_PREFIX_LEN: usize = 256;

/// `XDG_DATA_DIRS` when the environment names none (the XDG base directory
/// specification's default).
pub const DEFAULT_XDG_DATA_DIRS: &str = "/usr/local/share:/usr/share";

/// A validated compositor installation prefix: absolute, normalised, no
/// `.` or `..` components, no trailing slash, no whitespace or control
/// characters, and not `/` itself.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CompositorPrefix(String);

/// Why a string does not name a compositor prefix, or why a prefix may not
/// be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompositorPrefixError {
    /// Empty.
    Empty,
    /// Longer than [`MAX_PREFIX_LEN`].
    TooLong,
    /// Not an absolute path.
    NotAbsolute,
    /// The filesystem root.
    Root,
    /// A `.`, `..` or empty component, or a trailing slash.
    NotNormalised,
    /// Whitespace, a control character or a `:` (which would split
    /// `XDG_DATA_DIRS`).
    BadCharacter,
    /// A path the Pier would execute from is not owned by root.
    NotRootOwned {
        /// The path.
        path: String,
        /// Its owner.
        uid: u32,
    },
    /// A path the Pier would execute from is writable by its group or by
    /// everyone.
    Writable {
        /// The path.
        path: String,
        /// Its permission bits.
        mode: u32,
    },
    /// A path the Pier would execute from is missing or unreadable.
    Missing {
        /// The path.
        path: String,
    },
    /// The prefix holds neither `bin/gnome-shell` nor `bin/mutter` as an
    /// executable file.
    NoCompositor,
    /// The prefix, with what its links lead to, has more than
    /// [`MAX_PREFIX_ENTRIES`] entries.
    TooLarge,
}

impl Display for CompositorPrefixError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => formatter.write_str("the compositor prefix is empty"),
            Self::TooLong => write!(
                formatter,
                "the compositor prefix is longer than {MAX_PREFIX_LEN} bytes"
            ),
            Self::NotAbsolute => {
                formatter.write_str("the compositor prefix is not an absolute path")
            }
            Self::Root => formatter.write_str("the compositor prefix may not be `/`"),
            Self::NotNormalised => formatter.write_str(
                "the compositor prefix has a `.`, `..` or empty component or a trailing slash",
            ),
            Self::BadCharacter => formatter
                .write_str("the compositor prefix has whitespace, a control character or `:`"),
            Self::NotRootOwned { path, uid } => {
                write!(formatter, "{path} is owned by uid {uid}, not root")
            }
            Self::Writable { path, mode } => write!(
                formatter,
                "{path} is writable by its group or everyone (mode {mode:04o})"
            ),
            Self::Missing { path } => write!(formatter, "{path} is missing or unreadable"),
            Self::NoCompositor => formatter
                .write_str("the compositor prefix has no executable bin/gnome-shell or bin/mutter"),
            Self::TooLarge => write!(
                formatter,
                "the compositor prefix has more than {MAX_PREFIX_ENTRIES} entries"
            ),
        }
    }
}

impl std::error::Error for CompositorPrefixError {}

impl CompositorPrefix {
    /// The distribution's `/usr`.
    #[must_use]
    pub fn system() -> Self {
        Self(SYSTEM_PREFIX.to_owned())
    }

    /// Parses an operator-supplied prefix.
    ///
    /// # Errors
    ///
    /// See [`CompositorPrefixError`]; ownership is checked separately by
    /// [`check_prefix_ownership`].
    pub fn parse(value: &str) -> Result<Self, CompositorPrefixError> {
        if value.is_empty() {
            return Err(CompositorPrefixError::Empty);
        }
        if value.len() > MAX_PREFIX_LEN {
            return Err(CompositorPrefixError::TooLong);
        }
        if value.chars().any(|character| {
            character.is_whitespace() || character.is_control() || character == ':'
        }) {
            return Err(CompositorPrefixError::BadCharacter);
        }
        let Some(rest) = value.strip_prefix('/') else {
            return Err(CompositorPrefixError::NotAbsolute);
        };
        if rest.is_empty() {
            return Err(CompositorPrefixError::Root);
        }
        if rest
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
        {
            return Err(CompositorPrefixError::NotNormalised);
        }
        Ok(Self(value.to_owned()))
    }

    /// The prefix, as configured.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is the distribution's `/usr`.
    #[must_use]
    pub fn is_system(&self) -> bool {
        self.0 == SYSTEM_PREFIX
    }

    /// The prefix for a qualification record: `None` for `/usr`, so records
    /// proven on the system compositor keep their identity.
    #[must_use]
    pub fn recorded(&self) -> Option<String> {
        (!self.is_system()).then(|| self.0.clone())
    }

    /// The executable for `compositor` in this prefix.
    #[must_use]
    pub fn binary(&self, compositor: HeadlessCompositor) -> String {
        format!("{}/bin/{}", self.0, compositor.executable_name())
    }

    /// The prefix's data directory (`share`).
    #[must_use]
    pub fn data_dir(&self) -> String {
        format!("{}/share", self.0)
    }

    /// The session's `XDG_DATA_DIRS` given the inherited value: the prefix's
    /// data directory first, then the inherited list (or the specification's
    /// default), without duplicates. `None` for `/usr`: the session keeps
    /// whatever it inherited.
    #[must_use]
    pub fn xdg_data_dirs(&self, inherited: Option<&str>) -> Option<String> {
        if self.is_system() {
            return None;
        }
        let own = self.data_dir();
        let inherited = inherited
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_XDG_DATA_DIRS);
        let mut dirs = vec![own.as_str()];
        for dir in inherited.split(':') {
            if !dir.is_empty() && !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
        Some(dirs.join(":"))
    }
}

impl Default for CompositorPrefix {
    fn default() -> Self {
        Self::system()
    }
}

impl Display for CompositorPrefix {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// What a path is, as `lstat` sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A directory.
    Directory,
    /// A regular file.
    File,
    /// A symbolic link (judged by its own owner, then by its target).
    Symlink,
    /// Anything else (device, socket, FIFO).
    Other,
}

/// What the adapter read about one path with `lstat` (never following a
/// link).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathOwnership {
    /// The path.
    pub path: String,
    /// Owner, or `None` when the path is missing or unreadable.
    pub uid: Option<u32>,
    /// Permission bits (`st_mode & 0o7777`).
    pub mode: u32,
    /// What it is.
    pub kind: EntryKind,
}

/// Whether a root service may execute from these paths: every one exists,
/// is owned by root and, unless it is a symbolic link (whose own bits mean
/// nothing), is not writable by its group or by everyone.
///
/// # Errors
///
/// The first path that fails.
pub fn check_prefix_ownership(entries: &[PathOwnership]) -> Result<(), CompositorPrefixError> {
    entries.iter().try_for_each(check_entry)
}

fn check_entry(entry: &PathOwnership) -> Result<(), CompositorPrefixError> {
    let Some(uid) = entry.uid else {
        return Err(CompositorPrefixError::Missing {
            path: entry.path.clone(),
        });
    };
    if uid != 0 {
        return Err(CompositorPrefixError::NotRootOwned {
            path: entry.path.clone(),
            uid,
        });
    }
    if entry.kind != EntryKind::Symlink && entry.mode & 0o022 != 0 {
        return Err(CompositorPrefixError::Writable {
            path: entry.path.clone(),
            mode: entry.mode,
        });
    }
    Ok(())
}

/// The most entries [`verify_prefix`] walks (a full GNOME Shell prefix has
/// a few thousand).
pub const MAX_PREFIX_ENTRIES: usize = 200_000;

/// The filesystem questions [`verify_prefix`] asks; the platform adapter
/// answers them.
pub trait PrefixInspector {
    /// `lstat` of `path`: never follows a link.
    fn inspect(&self, path: &str) -> PathOwnership;
    /// The names in directory `path`, or `None` when it cannot be read.
    fn children(&self, path: &str) -> Option<Vec<String>>;
    /// `realpath` of `path`: absolute, every link resolved, or `None`.
    fn resolve(&self, path: &str) -> Option<String>;
}

/// Every directory from `/` down to `path`, then `path`.
#[must_use]
pub fn ancestors(path: &str) -> Vec<String> {
    let mut paths = vec!["/".to_owned()];
    let mut current = String::new();
    for component in path.split('/').filter(|component| !component.is_empty()) {
        current.push('/');
        current.push_str(component);
        paths.push(current.clone());
    }
    paths
}

fn inside(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Proves a root service may execute anything from `prefix`, and returns
/// the compositors it holds (GNOME Shell first).
///
/// Every path the service could load from must be owned by root and
/// writable by no one else: each component of the configured prefix and of
/// its real path (so no user can swap a parent), and every file, directory
/// and link inside it, recursively. A link must resolve; when its target
/// lies outside the prefix, the target and its ancestors pass the same rule
/// and a directory target is walked as well. `/usr` is the distribution's
/// and is not checked.
///
/// # Errors
///
/// The first path that fails, [`CompositorPrefixError::NoCompositor`] or
/// [`CompositorPrefixError::TooLarge`].
pub fn verify_prefix(
    prefix: &CompositorPrefix,
    inspector: &impl PrefixInspector,
) -> Result<Vec<HeadlessCompositor>, CompositorPrefixError> {
    let compositors = [HeadlessCompositor::GnomeShell, HeadlessCompositor::Mutter];
    if prefix.is_system() {
        return Ok(compositors.to_vec());
    }
    let mut checked = BTreeSet::new();
    let mut check = |path: &str| -> Result<PathOwnership, CompositorPrefixError> {
        let entry = inspector.inspect(path);
        if checked.insert(path.to_owned()) {
            check_entry(&entry)?;
        }
        Ok(entry)
    };
    for path in ancestors(prefix.as_str()) {
        check(&path)?;
    }
    let root =
        inspector
            .resolve(prefix.as_str())
            .ok_or_else(|| CompositorPrefixError::Missing {
                path: prefix.as_str().to_owned(),
            })?;
    for path in ancestors(&root) {
        check(&path)?;
    }
    let mut walked = BTreeSet::from([root.clone()]);
    let mut pending = vec![root.clone()];
    let mut entries = 0_usize;
    while let Some(directory) = pending.pop() {
        let names =
            inspector
                .children(&directory)
                .ok_or_else(|| CompositorPrefixError::Missing {
                    path: directory.clone(),
                })?;
        for name in names {
            entries += 1;
            if entries > MAX_PREFIX_ENTRIES {
                return Err(CompositorPrefixError::TooLarge);
            }
            let path = if directory == "/" {
                format!("/{name}")
            } else {
                format!("{directory}/{name}")
            };
            let entry = check(&path)?;
            let target = match entry.kind {
                EntryKind::Directory => path,
                EntryKind::Symlink => {
                    let target = inspector
                        .resolve(&path)
                        .ok_or(CompositorPrefixError::Missing { path })?;
                    if inside(&target, &root) {
                        continue;
                    }
                    let mut kind = EntryKind::Other;
                    for ancestor in ancestors(&target) {
                        kind = check(&ancestor)?.kind;
                    }
                    if kind != EntryKind::Directory {
                        continue;
                    }
                    target
                }
                EntryKind::File | EntryKind::Other => continue,
            };
            if walked.insert(target.clone()) {
                pending.push(target);
            }
        }
    }
    let present: Vec<HeadlessCompositor> = compositors
        .into_iter()
        .filter(|compositor| {
            inspector
                .resolve(&prefix.binary(*compositor))
                .map(|binary| inspector.inspect(&binary))
                .is_some_and(|binary| binary.kind == EntryKind::File && binary.mode & 0o111 != 0)
        })
        .collect();
    if present.is_empty() {
        return Err(CompositorPrefixError::NoCompositor);
    }
    Ok(present)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// An in-memory filesystem: path -> (uid, mode, kind, link target).
    #[derive(Default, Clone)]
    struct FakeFs(BTreeMap<String, (u32, u32, EntryKind, Option<String>)>);

    impl FakeFs {
        fn dir(mut self, path: &str, uid: u32, mode: u32) -> Self {
            self.0
                .insert(path.to_owned(), (uid, mode, EntryKind::Directory, None));
            self
        }
        fn file(mut self, path: &str, uid: u32, mode: u32) -> Self {
            self.0
                .insert(path.to_owned(), (uid, mode, EntryKind::File, None));
            self
        }
        fn link(mut self, path: &str, uid: u32, target: &str) -> Self {
            self.0.insert(
                path.to_owned(),
                (uid, 0o777, EntryKind::Symlink, Some(target.to_owned())),
            );
            self
        }
        /// A good side-by-side prefix under a root-owned /opt.
        fn prefix() -> Self {
            Self::default()
                .dir("/", 0, 0o555)
                .dir("/opt", 0, 0o755)
                .dir("/opt/g", 0, 0o755)
                .dir("/opt/g/bin", 0, 0o755)
                .file("/opt/g/bin/gnome-shell", 0, 0o755)
                .file("/opt/g/bin/mutter", 0, 0o755)
                .dir("/opt/g/lib64", 0, 0o755)
                .file("/opt/g/lib64/libmutter-18.so.0.0.0", 0, 0o755)
                .link(
                    "/opt/g/lib64/libmutter-18.so.0",
                    0,
                    "/opt/g/lib64/libmutter-18.so.0.0.0",
                )
                .dir("/opt/g/lib64/gio", 0, 0o755)
                .link(
                    "/opt/g/lib64/gio/libdconf.so",
                    0,
                    "/usr/lib64/gio/libdconf.so",
                )
                .dir("/opt/g/share", 0, 0o755)
                .dir("/opt/g/share/gnome-shell", 0, 0o755)
                .file("/opt/g/share/gnome-shell/shell.js", 0, 0o644)
                .dir("/usr", 0, 0o755)
                .dir("/usr/lib64", 0, 0o755)
                .dir("/usr/lib64/gio", 0, 0o755)
                .file("/usr/lib64/gio/libdconf.so", 0, 0o755)
        }
    }

    impl PrefixInspector for FakeFs {
        fn inspect(&self, path: &str) -> PathOwnership {
            let found = self.0.get(path);
            PathOwnership {
                path: path.to_owned(),
                uid: found.map(|entry| entry.0),
                mode: found.map_or(0, |entry| entry.1),
                kind: found.map_or(EntryKind::Other, |entry| entry.2),
            }
        }
        fn children(&self, path: &str) -> Option<Vec<String>> {
            let base = if path == "/" {
                String::new()
            } else {
                path.to_owned()
            };
            Some(
                self.0
                    .keys()
                    .filter_map(|key| key.strip_prefix(&format!("{base}/")))
                    .filter(|rest| !rest.is_empty() && !rest.contains('/'))
                    .map(str::to_owned)
                    .collect(),
            )
        }
        fn resolve(&self, path: &str) -> Option<String> {
            // Resolve every component, following links (bounded).
            let mut resolved = String::new();
            let mut hops = 0;
            let mut components: Vec<String> = path
                .split('/')
                .filter(|component| !component.is_empty())
                .map(str::to_owned)
                .rev()
                .collect();
            while let Some(component) = components.pop() {
                let candidate = format!("{resolved}/{component}");
                let (_, _, kind, target) = self.0.get(&candidate)?;
                if *kind == EntryKind::Symlink {
                    hops += 1;
                    if hops > 16 {
                        return None;
                    }
                    let target = target.clone()?;
                    resolved.clear();
                    components.extend(
                        target
                            .split('/')
                            .filter(|component| !component.is_empty())
                            .map(str::to_owned)
                            .rev(),
                    );
                } else {
                    resolved = candidate;
                }
            }
            Some(if resolved.is_empty() {
                "/".to_owned()
            } else {
                resolved
            })
        }
    }

    fn verify(fs: &FakeFs) -> Result<Vec<HeadlessCompositor>, CompositorPrefixError> {
        verify_prefix(&CompositorPrefix::parse("/opt/g").unwrap(), fs)
    }

    #[test]
    fn a_root_only_prefix_passes_and_names_every_compositor_it_holds() {
        assert_eq!(
            verify(&FakeFs::prefix()),
            Ok(vec![
                HeadlessCompositor::GnomeShell,
                HeadlessCompositor::Mutter
            ])
        );
        let mut mutter_only = FakeFs::prefix();
        mutter_only.0.remove("/opt/g/bin/gnome-shell");
        assert_eq!(verify(&mutter_only), Ok(vec![HeadlessCompositor::Mutter]));
        let mut none = mutter_only;
        none.0.remove("/opt/g/bin/mutter");
        assert_eq!(verify(&none), Err(CompositorPrefixError::NoCompositor));
        let not_executable = FakeFs::prefix()
            .file("/opt/g/bin/gnome-shell", 0, 0o644)
            .file("/opt/g/bin/mutter", 0, 0o644);
        assert_eq!(
            verify(&not_executable),
            Err(CompositorPrefixError::NoCompositor)
        );
    }

    #[test]
    fn every_compositor_binary_present_is_checked_not_only_the_one_that_passes() {
        // A user-writable gnome-shell next to a good mutter refuses the
        // prefix: the Pier would otherwise choose and run gnome-shell.
        let fs = FakeFs::prefix().file("/opt/g/bin/gnome-shell", 1000, 0o755);
        assert_eq!(
            verify(&fs),
            Err(CompositorPrefixError::NotRootOwned {
                path: "/opt/g/bin/gnome-shell".to_owned(),
                uid: 1000
            })
        );
        let fs = FakeFs::prefix().file("/opt/g/bin/gnome-shell", 0, 0o757);
        assert!(matches!(
            verify(&fs),
            Err(CompositorPrefixError::Writable { .. })
        ));
    }

    #[test]
    fn everything_the_compositor_loads_is_checked_recursively() {
        for (path, uid, mode) in [
            ("/opt/g/lib64/libmutter-18.so.0.0.0", 1000, 0o755),
            ("/opt/g/lib64/libmutter-18.so.0.0.0", 0, 0o775),
            ("/opt/g/share/gnome-shell/shell.js", 0, 0o666),
            ("/opt/g/share/gnome-shell/shell.js", 1000, 0o644),
        ] {
            let fs = FakeFs::prefix().file(path, uid, mode);
            assert!(verify(&fs).is_err(), "{path} {uid} {mode:o}");
        }
        let fs = FakeFs::prefix().dir("/opt/g/share/gnome-shell", 0, 0o777);
        assert!(verify(&fs).is_err());
        let fs = FakeFs::prefix().file("/opt/g/libexec-helper", 1000, 0o755);
        assert!(verify(&fs).is_err());
    }

    #[test]
    fn links_are_judged_by_their_owner_and_by_what_they_lead_to() {
        // A user's link inside the prefix.
        let fs = FakeFs::prefix().link("/opt/g/lib64/libx.so", 1000, "/usr/lib64/gio/libdconf.so");
        assert!(matches!(
            verify(&fs),
            Err(CompositorPrefixError::NotRootOwned { .. })
        ));
        // A root link to a user-writable file outside.
        let fs = FakeFs::prefix().file("/usr/lib64/gio/libdconf.so", 1000, 0o755);
        assert!(matches!(
            verify(&fs),
            Err(CompositorPrefixError::NotRootOwned { .. })
        ));
        // A root link into a user-owned directory outside.
        let fs = FakeFs::prefix()
            .dir("/home", 0, 0o755)
            .dir("/home/u", 1000, 0o700)
            .file("/home/u/evil.so", 1000, 0o755)
            .link("/opt/g/lib64/evil.so", 0, "/home/u/evil.so");
        assert!(matches!(
            verify(&fs),
            Err(CompositorPrefixError::NotRootOwned { .. })
        ));
        // A root link to a directory outside whose contents are not root's.
        let fs = FakeFs::prefix()
            .dir("/srv", 0, 0o755)
            .dir("/srv/js", 0, 0o755)
            .file("/srv/js/ui.js", 1000, 0o644)
            .link("/opt/g/share/js", 0, "/srv/js");
        assert!(matches!(
            verify(&fs),
            Err(CompositorPrefixError::NotRootOwned { .. })
        ));
        // A dangling link.
        let fs = FakeFs::prefix().link("/opt/g/lib64/gone.so", 0, "/nowhere/gone.so");
        assert!(matches!(
            verify(&fs),
            Err(CompositorPrefixError::Missing { .. })
        ));
        // A loop is walked once.
        let fs = FakeFs::prefix().link("/opt/g/share/self", 0, "/opt/g/share");
        assert!(verify(&fs).is_ok());
    }

    #[test]
    fn the_real_path_and_the_configured_path_must_both_be_root_only() {
        // /opt/g is a root link into a directory a user can swap.
        let mut fs = FakeFs::prefix();
        let moved: Vec<_> =
            fs.0.iter()
                .filter(|(path, _)| path.starts_with("/opt/g"))
                .map(|(path, entry)| (path.replacen("/opt/g", "/var/u/g", 1), entry.clone()))
                .collect();
        fs.0.retain(|path, _| !path.starts_with("/opt/g"));
        for (path, mut entry) in moved {
            if let Some(target) = entry.3.as_mut() {
                *target = target.replacen("/opt/g", "/var/u/g", 1);
            }
            fs.0.insert(path, entry);
        }
        let fs = fs
            .dir("/var", 0, 0o755)
            .dir("/var/u", 1000, 0o755)
            .link("/opt/g", 0, "/var/u/g");
        assert_eq!(
            verify(&fs),
            Err(CompositorPrefixError::NotRootOwned {
                path: "/var/u".to_owned(),
                uid: 1000
            })
        );
        // The same through a root-only directory passes.
        let fs = fs.dir("/var/u", 0, 0o755);
        assert!(verify(&fs).is_ok());
        // A user-writable parent of the configured path fails.
        let fs = FakeFs::prefix().dir("/opt", 0, 0o777);
        assert!(matches!(
            verify(&fs),
            Err(CompositorPrefixError::Writable { .. })
        ));
        assert_eq!(
            verify(&FakeFs::default()),
            Err(CompositorPrefixError::Missing {
                path: "/".to_owned()
            })
        );
    }

    #[test]
    fn ancestors_run_from_the_root_down() {
        assert_eq!(
            ancestors("/opt/arcen-gnome/51"),
            ["/", "/opt", "/opt/arcen-gnome", "/opt/arcen-gnome/51"]
        );
        assert_eq!(ancestors("/"), ["/"]);
    }

    #[test]
    fn the_default_is_the_systems_usr_and_needs_no_checks() {
        let prefix = CompositorPrefix::default();
        assert!(prefix.is_system());
        assert_eq!(prefix.recorded(), None);
        assert_eq!(
            prefix.binary(HeadlessCompositor::GnomeShell),
            HeadlessCompositor::GnomeShell.binary()
        );
        assert_eq!(
            prefix.binary(HeadlessCompositor::Mutter),
            HeadlessCompositor::Mutter.binary()
        );
        assert_eq!(prefix.xdg_data_dirs(Some("/x/share")), None);
        assert_eq!(
            verify_prefix(&prefix, &FakeFs::default()).map(|found| found.len()),
            Ok(2)
        );
        assert_eq!(
            CompositorPrefix::parse("/usr"),
            Ok(CompositorPrefix::system())
        );
    }

    #[test]
    fn a_side_by_side_prefix_names_its_binary_data_and_record_identity() {
        let prefix = CompositorPrefix::parse("/opt/arcen-gnome/51").unwrap();
        assert!(!prefix.is_system());
        assert_eq!(prefix.recorded().as_deref(), Some("/opt/arcen-gnome/51"));
        assert_eq!(
            prefix.binary(HeadlessCompositor::GnomeShell),
            "/opt/arcen-gnome/51/bin/gnome-shell"
        );
        assert_eq!(
            prefix.binary(HeadlessCompositor::Mutter),
            "/opt/arcen-gnome/51/bin/mutter"
        );
        assert_eq!(prefix.data_dir(), "/opt/arcen-gnome/51/share");
        assert_eq!(prefix.to_string(), "/opt/arcen-gnome/51");
    }

    #[test]
    fn malformed_prefixes_are_refused() {
        for (value, error) in [
            ("", CompositorPrefixError::Empty),
            ("opt/gnome", CompositorPrefixError::NotAbsolute),
            ("/", CompositorPrefixError::Root),
            ("/opt/", CompositorPrefixError::NotNormalised),
            ("/opt//gnome", CompositorPrefixError::NotNormalised),
            ("/opt/./gnome", CompositorPrefixError::NotNormalised),
            ("/opt/../usr", CompositorPrefixError::NotNormalised),
            ("/opt/my gnome", CompositorPrefixError::BadCharacter),
            ("/opt/a:b", CompositorPrefixError::BadCharacter),
            ("/opt/a\nb", CompositorPrefixError::BadCharacter),
        ] {
            assert_eq!(CompositorPrefix::parse(value), Err(error), "{value:?}");
        }
        let long = format!("/{}", "a".repeat(MAX_PREFIX_LEN));
        assert_eq!(
            CompositorPrefix::parse(&long),
            Err(CompositorPrefixError::TooLong)
        );
    }

    #[test]
    fn the_prefix_goes_first_in_xdg_data_dirs_without_duplicates() {
        let prefix = CompositorPrefix::parse("/opt/arcen-gnome/51").unwrap();
        assert_eq!(
            prefix.xdg_data_dirs(None).as_deref(),
            Some("/opt/arcen-gnome/51/share:/usr/local/share:/usr/share")
        );
        assert_eq!(
            prefix.xdg_data_dirs(Some("")).as_deref(),
            Some("/opt/arcen-gnome/51/share:/usr/local/share:/usr/share")
        );
        assert_eq!(
            prefix
                .xdg_data_dirs(Some(
                    "/usr/share:/opt/arcen-gnome/51/share::/var/lib/flatpak/exports/share"
                ))
                .as_deref(),
            Some("/opt/arcen-gnome/51/share:/usr/share:/var/lib/flatpak/exports/share")
        );
    }

    #[test]
    fn only_root_owned_paths_writable_by_no_one_else_pass() {
        let entry = |path: &str, uid: Option<u32>, mode: u32| PathOwnership {
            path: path.to_owned(),
            uid,
            mode,
            kind: EntryKind::Directory,
        };
        assert_eq!(
            check_prefix_ownership(&[
                entry("/opt", Some(0), 0o755),
                entry("/opt/g", Some(0), 0o555)
            ]),
            Ok(())
        );
        assert_eq!(
            check_prefix_ownership(&[
                entry("/opt", Some(0), 0o755),
                entry("/opt/g", Some(1000), 0o755)
            ]),
            Err(CompositorPrefixError::NotRootOwned {
                path: "/opt/g".to_owned(),
                uid: 1000
            })
        );
        assert_eq!(
            check_prefix_ownership(&[entry("/opt/g", Some(0), 0o775)]),
            Err(CompositorPrefixError::Writable {
                path: "/opt/g".to_owned(),
                mode: 0o775
            })
        );
        assert_eq!(
            check_prefix_ownership(&[entry("/tmp", Some(0), 0o1777)]),
            Err(CompositorPrefixError::Writable {
                path: "/tmp".to_owned(),
                mode: 0o1777
            })
        );
        assert_eq!(
            check_prefix_ownership(&[entry("/opt/g/bin/gnome-shell", None, 0)]),
            Err(CompositorPrefixError::Missing {
                path: "/opt/g/bin/gnome-shell".to_owned()
            })
        );
    }
}
