// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The shared X11 and ICE socket directories in `/tmp`: checked before a
//! desktop that runs X11 applications, recreated when missing, and repaired
//! on an operator's request (`arcen-pier repair-x11-unix-dir`).
//!
//! systemd-tmpfiles creates `/tmp/.X11-unix` and `/tmp/.ICE-unix`
//! `root:root 1777` only at boot (`D!` in `x11.conf`), and its daily
//! `--clean` ignores boot-only entries, so it ages them like any other
//! directory in `/tmp`: a host that ran no desktop for ten days loses them,
//! and the next compositor recreates `.X11-unix` as its own user. Arcen
//! installs `/etc/tmpfiles.d/arcen-pier.conf` to stop that; this module
//! covers hosts that already lost them.
//!
//! Every fix goes through the distribution's own entry
//! (`systemd-tmpfiles --boot --create --prefix=<dir>`), which sets owner,
//! mode and SELinux label through a descriptor and never follows a symlink.
//! Automatically only a *missing* directory is recreated; a user-owned or
//! non-sticky one may hold another user's live sockets, so it is reported
//! and repaired only by the operator's command. That command never removes
//! an entry inside.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use arcen_session::wayland_x11::{
    assess_socket_dir, SharedSocketDir, SocketDirFact, SocketDirFault, SocketDirProblem,
};
use rustix::fs::FileType;

use crate::logging::target;

/// The tool that recreates the directories as the distribution declares them.
pub const SYSTEMD_TMPFILES: &str = "/usr/bin/systemd-tmpfiles";
const TMPFILES_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(20);

/// Where the directories live and who must own them.
#[derive(Debug, Clone)]
pub struct SocketDirRoot {
    /// `/tmp` in production.
    pub tmp: PathBuf,
    /// Root in production.
    pub owner: u32,
}

impl SocketDirRoot {
    /// `/tmp`, root-owned directories.
    #[must_use]
    pub fn production() -> Self {
        Self {
            tmp: PathBuf::from("/tmp"),
            owner: 0,
        }
    }

    /// The path of `dir` under this root.
    #[must_use]
    pub fn path(&self, dir: SharedSocketDir) -> PathBuf {
        self.tmp.join(dir.name())
    }
}

/// What `lstat` says about `dir`, never following a symlink; `None` only
/// when nothing is there.
#[must_use]
pub fn fact(root: &SocketDirRoot, dir: SharedSocketDir) -> Option<SocketDirFact> {
    match rustix::fs::lstat(root.path(dir)) {
        Ok(stat) => Some(SocketDirFact {
            directory: FileType::from_raw_mode(stat.st_mode) == FileType::Directory,
            owner: stat.st_uid,
            mode: stat.st_mode & 0o7777,
        }),
        Err(rustix::io::Errno::NOENT) => None,
        // Unreadable is not usable; never "missing", so nothing is created.
        Err(_) => Some(SocketDirFact {
            directory: false,
            owner: u32::MAX,
            mode: 0,
        }),
    }
}

/// [`assess_socket_dir`] on the live directory.
///
/// # Errors
///
/// The [`SocketDirFault`] found.
pub fn assess(root: &SocketDirRoot, dir: SharedSocketDir) -> Result<(), SocketDirFault> {
    assess_socket_dir(dir, fact(root, dir), root.owner)
}

/// Runs `tmpfiles --boot --create --prefix=<dir>`, bounded.
fn run_tmpfiles(tmpfiles: &Path, path: &Path) -> Result<(), String> {
    let mut child = Command::new(tmpfiles)
        .arg("--boot")
        .arg("--create")
        .arg(format!("--prefix={}", path.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("{}: {error}", tmpfiles.display()))?;
    let deadline = Instant::now() + TMPFILES_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("{}: {status}", tmpfiles.display())),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{}: did not finish", tmpfiles.display()));
            }
            Err(error) => return Err(format!("{}: {error}", tmpfiles.display())),
        }
    }
}

/// Checks `dir` before a desktop; a missing one is recreated with
/// `tmpfiles` when given (as root). Anything else is only reported.
///
/// # Errors
///
/// The [`SocketDirFault`] that remains.
pub fn prepare(
    root: &SocketDirRoot,
    dir: SharedSocketDir,
    tmpfiles: Option<&Path>,
) -> Result<(), SocketDirFault> {
    let fault = match assess(root, dir) {
        Ok(()) => return Ok(()),
        Err(fault) => fault,
    };
    let Some(tmpfiles) = tmpfiles.filter(|_| fault.problem == SocketDirProblem::Missing) else {
        return Err(fault);
    };
    let ran = run_tmpfiles(tmpfiles, &root.path(dir));
    let after = assess(root, dir);
    match ran {
        Ok(()) => tracing::info!(
            target: target::SESSION,
            path = %root.path(dir).display(),
            recreated = after.is_ok(),
            "shared socket directory was missing; recreated it from the system's tmpfiles entry"
        ),
        Err(error) => tracing::warn!(
            target: target::SESSION,
            %error,
            path = %root.path(dir).display(),
            "shared socket directory is missing and systemd-tmpfiles failed"
        ),
    }
    after
}

/// Repairs `dir` on the operator's request: anything but a non-directory at
/// the path is set to the distribution's owner, mode and label by
/// `tmpfiles`. Entries inside are never touched.
///
/// # Errors
///
/// The [`SocketDirFault`] that remains, and why `tmpfiles` failed.
pub fn repair(
    root: &SocketDirRoot,
    dir: SharedSocketDir,
    tmpfiles: &Path,
) -> Result<bool, (SocketDirFault, Option<String>)> {
    match assess(root, dir) {
        Ok(()) => Ok(false),
        Err(fault) if fault.problem == SocketDirProblem::NotADirectory => Err((fault, None)),
        Err(_) => {
            let ran = run_tmpfiles(tmpfiles, &root.path(dir)).err();
            assess(root, dir)
                .map(|()| true)
                .map_err(|fault| (fault, ran))
        }
    }
}

/// [`prepare`] for both production directories; recreates a missing one
/// only when running as root.
pub fn prepare_production() -> [(SharedSocketDir, Result<(), SocketDirFault>); 2] {
    let root = SocketDirRoot::production();
    let tmpfiles = nix::unistd::Uid::effective()
        .is_root()
        .then_some(Path::new(SYSTEMD_TMPFILES));
    SharedSocketDir::ALL.map(|dir| (dir, prepare(&root, dir, tmpfiles)))
}

/// The `/tmp/.X11-unix` result of [`prepare_production`].
///
/// # Errors
///
/// Its [`SocketDirFault`].
pub fn x11_result(
    results: &[(SharedSocketDir, Result<(), SocketDirFault>); 2],
) -> Result<(), SocketDirFault> {
    results
        .iter()
        .find(|(dir, _)| *dir == SharedSocketDir::X11Unix)
        .map_or(Ok(()), |(_, result)| *result)
}

/// Entries of `dir` not owned by the directory's required owner, as
/// `name (uid n)`: a former owner's leftovers, reported, never removed.
fn foreign_entries(root: &SocketDirRoot, dir: SharedSocketDir) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root.path(dir)) else {
        return Vec::new();
    };
    let mut found = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let stat = rustix::fs::lstat(entry.path()).ok()?;
            (stat.st_uid != root.owner).then(|| {
                format!(
                    "{} (uid {})",
                    entry.file_name().to_string_lossy(),
                    stat.st_uid
                )
            })
        })
        .collect::<Vec<_>>();
    found.sort();
    found
}

/// Usage of `arcen-pier repair-x11-unix-dir`.
pub const USAGE: &str =
    "usage: arcen-pier repair-x11-unix-dir [--check | [--requalify] [--record PATH]]";

/// What `arcen-pier repair-x11-unix-dir` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairOptions {
    /// `--check`: report only.
    pub check: bool,
    /// `--requalify`: after a repair that clears a recorded X11 host
    /// precondition, restart the production service now so it re-proves
    /// X11 applications before it serves (ends running desktops).
    pub requalify: bool,
    /// `--record PATH`: the qualification record whose automatic X11
    /// re-proof a repair re-arms (default: production).
    pub record: Option<String>,
}

impl RepairOptions {
    /// Parses the command's arguments.
    ///
    /// # Errors
    ///
    /// [`USAGE`] for anything else.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Self {
            check: false,
            requalify: false,
            record: None,
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--check" if !options.check => options.check = true,
                "--requalify" if !options.requalify => options.requalify = true,
                "--record" if options.record.is_none() => {
                    options.record = Some(args.next().ok_or(USAGE)?.clone());
                }
                _ => return Err(USAGE.to_owned()),
            }
        }
        if options.check && (options.requalify || options.record.is_some()) {
            return Err(USAGE.to_owned());
        }
        Ok(options)
    }
}

/// `arcen-pier repair-x11-unix-dir [--check | [--requalify] [--record PATH]]`:
/// reports, and as root repairs, `/tmp/.X11-unix` and `/tmp/.ICE-unix`.
/// After a repair, a record whose X11 proof was skipped for the directory
/// gets its automatic re-proof re-armed (the Pier's next start runs it);
/// `--requalify` restarts the production service to run it now. Exit
/// status 0 when both are sound afterwards.
#[must_use]
pub fn cli_main(args: &[String]) -> ExitCode {
    let options = match RepairOptions::parse(args) {
        Ok(options) => options,
        Err(usage) => {
            eprintln!("{usage}");
            return ExitCode::from(2);
        }
    };
    let check = options.check;
    let target = match arcen_session::wayland_qualification::QualificationTarget::from_config(
        options.record.as_deref(),
    ) {
        Ok(target) => target,
        Err(error) => {
            eprintln!("repair-x11-unix-dir: {error}");
            return ExitCode::from(2);
        }
    };
    if !check && !nix::unistd::Uid::effective().is_root() {
        eprintln!("repair-x11-unix-dir: run as root (or with --check to only report)");
        return ExitCode::from(2);
    }
    let root = SocketDirRoot::production();
    let mut sound = true;
    for dir in SharedSocketDir::ALL {
        let path = dir.path();
        let outcome = if check {
            assess(&root, dir)
                .map(|()| false)
                .map_err(|fault| (fault, None))
        } else {
            repair(&root, dir, Path::new(SYSTEMD_TMPFILES))
        };
        match outcome {
            Ok(false) => println!("ok   {path}: root:root, world-writable, sticky"),
            Ok(true) => println!(
                "ok   {path}: repaired with `{SYSTEMD_TMPFILES} --boot --create --prefix={path}`"
            ),
            Err((fault, tmpfiles_error)) => {
                sound = false;
                println!("FAIL {fault}");
                if let Some(error) = tmpfiles_error {
                    println!("     systemd-tmpfiles: {error}");
                }
            }
        }
        let foreign = foreign_entries(&root, dir);
        if !foreign.is_empty() {
            println!(
                "     entries owned by other users (left in place; remove them once none of \
                 their X servers runs): {}",
                foreign.join(", ")
            );
        }
    }
    if sound && !check {
        after_repair(&target, options.requalify);
    }
    if sound {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// After a repair: re-arms the automatic X11 re-proof of a record that
/// skipped X11 applications for a host precondition, and with `restart`
/// restarts the production service so it runs now.
#[cfg(feature = "wayland-provider")]
fn after_repair(target: &arcen_session::wayland_qualification::QualificationTarget, restart: bool) {
    let record = target.record_path();
    let precondition = super::wayland::qualification::load_record(Path::new(record))
        .ok()
        .and_then(|record| record.facts.x11_apps)
        .and_then(|proof| proof.host_precondition);
    let Some(precondition) = precondition else {
        println!(
            "The Wayland qualification record {record} did not skip X11 applications for a host \
             precondition: nothing to re-prove."
        );
        return;
    };
    // The operator's repair is a new state: the once-per-boot hold-off of
    // the automatic re-proof no longer applies.
    let marker = target.x11_requalify_marker_path();
    match std::fs::remove_file(&marker) {
        Ok(()) => println!("Re-armed the automatic X11 re-proof (removed {marker})."),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => println!("Could not remove {marker}: {error}"),
    }
    println!(
        "The record skipped X11 applications for {precondition}; the Pier re-proves them \
         automatically at its next start, before it serves (a short run; \
         capture.wayland_auto_requalify, logged as x11_requalify_outcome)."
    );
    if !restart {
        println!(
            "Restart it when no desktop is in use (`systemctl restart arcen-pier` ends running \
             desktops), or run this command with --requalify."
        );
        return;
    }
    if !target.is_production() {
        println!(
            "--requalify restarts only the production service; restart the Pier that uses \
             {record} yourself."
        );
        return;
    }
    let systemctl = |args: &[&str]| {
        Command::new("/usr/bin/systemctl")
            .args(args)
            .stdin(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    if !systemctl(&["is-active", "--quiet", "arcen-pier.service"]) {
        println!("arcen-pier is not running; it re-proves X11 applications when it starts.");
        return;
    }
    if systemctl(&["--no-block", "restart", "arcen-pier.service"]) {
        println!(
            "Restarting arcen-pier (running desktops end); follow the re-proof with \
             `journalctl -u arcen-pier -f`."
        );
    } else {
        println!("systemctl restart arcen-pier failed; restart it yourself.");
    }
}

/// Without the Wayland provider there is no qualification record.
#[cfg(not(feature = "wayland-provider"))]
fn after_repair(
    _target: &arcen_session::wayland_qualification::QualificationTarget,
    _restart: bool,
) {
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn own_uid() -> u32 {
        nix::unistd::Uid::effective().as_raw()
    }

    /// A private `/tmp` with both directories `1777`, owned by the test's
    /// user, and a stand-in for systemd-tmpfiles that (re)creates the
    /// `--prefix` directory `1777` and leaves a marker.
    fn fixture(name: &str) -> (SocketDirRoot, PathBuf, PathBuf) {
        let tmp = std::env::temp_dir().join(format!("socket-dirs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("fixture");
        let root = SocketDirRoot {
            tmp: tmp.clone(),
            owner: own_uid(),
        };
        for dir in SharedSocketDir::ALL {
            std::fs::create_dir(root.path(dir)).expect("dir");
            std::fs::set_permissions(root.path(dir), std::fs::Permissions::from_mode(0o1777))
                .expect("1777");
        }
        let marker = tmp.join("tmpfiles-ran");
        let script = tmp.join("tmpfiles.sh");
        let body = format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\nfor a; do case \"$a\" in --prefix=*) d=\"${{a#--prefix=}}\"; mkdir -p \"$d\" && chmod 1777 \"$d\";; esac; done\n",
            marker.display()
        );
        // A child shell writes the stand-in, so this process never holds a
        // writable descriptor to it that a test forking concurrently could
        // inherit; executing it would then fail with ETXTBSY.
        let written = Command::new("/bin/sh")
            .arg("-c")
            .arg("printf '%s' \"$2\" > \"$1\" && chmod 755 \"$1\"")
            .arg("sh")
            .arg(&script)
            .arg(body)
            .status()
            .expect("script");
        assert!(written.success(), "script");
        (root, script, marker)
    }

    #[test]
    fn the_repair_command_takes_requalify_and_a_record_but_not_with_check() {
        let parse = |args: &[&str]| {
            RepairOptions::parse(&args.iter().map(|&arg| arg.to_owned()).collect::<Vec<_>>())
        };
        assert_eq!(
            parse(&[]),
            Ok(RepairOptions {
                check: false,
                requalify: false,
                record: None
            })
        );
        assert_eq!(
            parse(&["--requalify", "--record", "/var/lib/arcen-dev/q.json"]),
            Ok(RepairOptions {
                check: false,
                requalify: true,
                record: Some("/var/lib/arcen-dev/q.json".into())
            })
        );
        assert!(parse(&["--check"]).is_ok_and(|options| options.check));
        for bad in [
            &["--check", "--requalify"][..],
            &["--check", "--record", "/x.json"],
            &["--record"],
            &["--requalify", "--requalify"],
            &["--force"],
        ] {
            assert_eq!(parse(bad), Err(USAGE.to_owned()), "{bad:?}");
        }
    }

    #[test]
    fn only_a_missing_directory_is_recreated_before_a_desktop() {
        use SharedSocketDir::{IceUnix, X11Unix};
        let (root, tmpfiles, marker) = fixture("prepare");
        for dir in SharedSocketDir::ALL {
            assert_eq!(prepare(&root, dir, Some(&tmpfiles)), Ok(()));
        }
        assert!(!marker.exists(), "sound directories need nothing");

        std::fs::remove_dir(root.path(IceUnix)).expect("remove");
        let missing = prepare(&root, IceUnix, None);
        assert_eq!(
            missing.map_err(|fault| fault.token()),
            Err("ice_unix_dir_missing")
        );
        assert_eq!(prepare(&root, IceUnix, Some(&tmpfiles)), Ok(()));
        let ran = std::fs::read_to_string(&marker).expect("ran");
        assert!(ran.contains("--boot --create --prefix=") && ran.contains(".ICE-unix"));
        std::fs::remove_file(&marker).expect("marker");

        // Mutter's own mkdir under a 022 umask (the 0.18.0 qualification
        // host's state) is reported, never repaired automatically.
        std::fs::set_permissions(root.path(X11Unix), std::fs::Permissions::from_mode(0o1755))
            .expect("1755");
        assert_eq!(
            prepare(&root, X11Unix, Some(&tmpfiles)).map_err(|fault| fault.problem),
            Err(SocketDirProblem::NotShared { mode: 0o1755 })
        );
        let foreign = SocketDirRoot {
            owner: own_uid().wrapping_add(1),
            ..root.clone()
        };
        assert_eq!(
            prepare(&foreign, IceUnix, Some(&tmpfiles)).map_err(|fault| fault.problem),
            Err(SocketDirProblem::ForeignOwner { owner: own_uid() })
        );
        assert!(!marker.exists(), "only a missing directory is recreated");
        let _ = std::fs::remove_dir_all(&root.tmp);
    }

    #[test]
    fn the_repair_runs_tmpfiles_for_a_wrong_directory_but_never_for_a_symlink() {
        use SharedSocketDir::{IceUnix, X11Unix};
        let (root, tmpfiles, marker) = fixture("repair");
        assert_eq!(repair(&root, X11Unix, &tmpfiles), Ok(false));
        assert!(!marker.exists());

        std::fs::set_permissions(root.path(X11Unix), std::fs::Permissions::from_mode(0o1755))
            .expect("1755");
        std::fs::write(root.path(X11Unix).join("X0"), b"").expect("leftover");
        assert_eq!(repair(&root, X11Unix, &tmpfiles), Ok(true));
        assert!(std::fs::read_to_string(&marker)
            .expect("ran")
            .contains(".X11-unix"));
        assert!(
            root.path(X11Unix).join("X0").exists(),
            "entries inside are never removed"
        );
        std::fs::remove_file(&marker).expect("marker");

        std::fs::remove_dir(root.path(IceUnix)).expect("remove");
        let elsewhere = root.tmp.join("elsewhere");
        std::fs::create_dir(&elsewhere).expect("target");
        std::os::unix::fs::symlink(&elsewhere, root.path(IceUnix)).expect("symlink");
        let refused = repair(&root, IceUnix, &tmpfiles).map_err(|(fault, _)| fault.token());
        assert_eq!(refused, Err("ice_unix_dir_not_a_directory"));
        assert!(!marker.exists(), "a symlink is never handed to tmpfiles");

        let foreign = SocketDirRoot {
            owner: own_uid().wrapping_add(1),
            ..root.clone()
        };
        assert_eq!(
            foreign_entries(&foreign, X11Unix),
            vec![format!("X0 (uid {})", own_uid())]
        );
        let _ = std::fs::remove_dir_all(&root.tmp);
    }
}
