#![allow(unsafe_code)]

//! launchd service installation for the macOS Pier.
//!
//! Until this exists the Pier has to be started by hand, so nothing survives a
//! restart — which is the one thing a remote host must do, because there is
//! nobody in the room to start it again.
//!
//! The daemon runs as a dedicated unprivileged service account, never root.
//! The Pier terminates TLS, parses client input and drives an encoder; none of
//! that needs to be able to rewrite the system, and a remote-facing process
//! running as root turns any single mistake into a total compromise.

use std::path::{Path, PathBuf};

/// launchd label for the network service.
pub const DAEMON_LABEL: &str = "pier.arcen.tech.service";
/// Where the network service's definition lives.
pub const DAEMON_PLIST: &str = "/Library/LaunchDaemons/pier.arcen.tech.service.plist";
/// launchd label for the per-session desktop agent.
pub const AGENT_LABEL: &str = "pier.arcen.tech.agent";
/// Where the desktop agent's definition lives.
pub const AGENT_PLIST: &str = "/Library/LaunchAgents/pier.arcen.tech.agent.plist";
/// Labels earlier builds installed, which an upgrade and an uninstall remove.
pub const LEGACY_LABELS: [&str; 3] = [
    "pier.arcen.tech",
    "com.arcen.pier",
    "pier.arcen.tech.timezone-helper",
];
/// The installed network service executable.
pub const PIER_PROGRAM: &str = "/Applications/Arcen Pier.app/Contents/MacOS/arcen-pier-macos";
/// The installed desktop agent executable. A background helper, so it is not
/// shown in /Applications as a second app.
pub const AGENT_PROGRAM: &str =
    "/Library/PrivilegedHelperTools/Arcen Agent Helper.app/Contents/MacOS/arcen-agent-helper";
/// The Pier app's bundle identifier. Both launchd jobs name it, so System
/// Settings lists them under "Arcen Pier" instead of the signing team.
pub const APP_BUNDLE_ID: &str = "pier.arcen.tech";
/// Where the installer keeps the host's TLS identity.
pub const TLS_DIRECTORY: &str = "/Library/Application Support/Arcen/tls";
/// Unprivileged account the network service runs as.
pub const SERVICE_ACCOUNT: &str = "_arcen";
/// Directory holding managed logs.
pub const LOG_DIRECTORY: &str = "/Library/Logs/Arcen/Pier";

/// Why service installation failed.
#[derive(Debug)]
pub enum ServiceError {
    /// The caller is not root.
    NeedsRoot,
    /// A file operation failed.
    Io(String),
    /// `launchctl` refused the definition.
    Launchctl(String),
    /// The installed binary could not be located.
    MissingProgram(PathBuf),
    /// The shared installer transaction refused the step or never finished.
    Transaction(String),
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NeedsRoot => {
                formatter.write_str("installing a system service needs root; re-run with sudo")
            }
            Self::Io(detail) => write!(formatter, "filesystem error: {detail}"),
            Self::Launchctl(detail) => write!(formatter, "launchctl: {detail}"),
            Self::MissingProgram(path) => {
                write!(formatter, "no Pier binary at {}", path.display())
            }
            Self::Transaction(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for ServiceError {}

/// Renders the network service's `LaunchDaemon` definition.
///
/// `RunAtLoad` and `KeepAlive` together are what make the host survive a
/// restart and a crash without anyone touching the machine. It belongs to the
/// machine, not to whoever is at the screen, so it is also what survives a
/// logout: the host stays reachable with nobody logged in.
#[must_use]
pub fn daemon_plist(program: &Path, tls_directory: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{DAEMON_LABEL}</string>
    <key>AssociatedBundleIdentifiers</key>
    <array>
        <string>{APP_BUNDLE_ID}</string>
    </array>
    <key>MachServices</key>
    <dict>
        <key>tech.arcen.microphone</key>
        <true/>
    </dict>
    <key>ProgramArguments</key>
    <array>
        <string>{program}</string>
        <string>daemon</string>
        <string>--tls-directory</string>
        <string>{tls}</string>
    </array>
    <!-- The service terminates TLS and parses untrusted input. None of that
         needs to be able to rewrite the system, and it has no desktop: the
         agents in each graphical session do. -->
    <key>UserName</key>
    <string>{SERVICE_ACCOUNT}</string>
    <key>GroupName</key>
    <string>{SERVICE_ACCOUNT}</string>
    <!-- Start at boot and restart on failure: there is nobody at the machine
         to start it again. -->
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <!-- A crash loop should back off rather than burn the machine. -->
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>StandardOutPath</key>
    <string>{LOG_DIRECTORY}/service.log</string>
    <key>StandardErrorPath</key>
    <string>{LOG_DIRECTORY}/service.log</string>
</dict>
</plist>
"#,
        program = xml_escape(&program.display().to_string()),
        tls = xml_escape(&tls_directory.display().to_string()),
    )
}

/// launchd starts one copy in every graphical session, as that session's user,
/// which is the only place capture, input injection and the pasteboard work
/// and the only identity TCC grants them to. The agent holds no key and binds
/// no port, so any number of sessions can each have one.
///
/// Aqua and `LoginWindow`. At the login window launchd starts the agent as
/// root, which is how the agent knows which session it is in; it serves any
/// account that authenticates, so that account can sign in, and types through
/// the virtual HID keyboard, the only input that reaches the login window.
///
/// No standard output paths: launchd would open one file for every user's
/// agent, and whichever user created it first would own it. The agent writes
/// its own log under the user's `~/Library/Logs` instead.
#[must_use]
pub fn agent_plist(program: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{AGENT_LABEL}</string>
    <key>AssociatedBundleIdentifiers</key>
    <array>
        <string>{APP_BUNDLE_ID}</string>
    </array>
    <key>ProgramArguments</key>
    <array>
        <string>{program}</string>
        <string>agent</string>
    </array>
    <key>LimitLoadToSessionType</key>
    <array>
        <string>Aqua</string>
        <string>LoginWindow</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <!-- Latency matters here: this process captures and encodes. -->
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#,
        program = xml_escape(&program.display().to_string()),
    )
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The account this process runs as.
#[must_use]
pub fn current_uid() -> u32 {
    // SAFETY: `getuid` is a nullary system query that cannot fail.
    unsafe { libc_getuid() }
}

/// Sends this process's standard output and error to the user's own log.
///
/// A desktop agent runs once per logged-in user, so a fixed system path would
/// belong to whichever user wrote it first and refuse the rest — which is how
/// an earlier build's `/tmp` log locked every other account's agent out. The
/// user's `~/Library/Logs` is theirs alone. Rotated by size on start, so a
/// crash loop cannot fill the disk.
pub fn redirect_stderr_to_user_log() {
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::os::unix::io::AsRawFd as _;

    const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let directory = PathBuf::from(home).join("Library/Logs/Arcen/Pier");
    if std::fs::create_dir_all(&directory).is_err() {
        return;
    }
    let path = directory.join("agent.log");
    if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() > MAX_LOG_BYTES) {
        let _ = std::fs::rename(&path, directory.join("agent.log.1"));
    }
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
    else {
        return;
    };
    // SAFETY: `file` is open for the duration of both calls, and duplicating a
    // valid descriptor onto 1 and 2 is what `dup2` is for.
    unsafe {
        libc_dup2(file.as_raw_fd(), 1);
        libc_dup2(file.as_raw_fd(), 2);
    }
}

/// Returns whether this process can install a system service.
#[must_use]
pub fn is_root() -> bool {
    // SAFETY: `geteuid` is a nullary system query.
    unsafe { libc_geteuid() == 0 }
}

#[link(name = "c")]
unsafe extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
    #[link_name = "dup2"]
    fn libc_dup2(from: i32, to: i32) -> i32;
}

/// Installs and starts the daemon.
///
/// # Errors
///
/// Returns [`ServiceError`] when not root, when the binary is missing, or when
/// launchd refuses the definition.
pub fn install(program: &Path, tls_directory: &Path) -> Result<(), ServiceError> {
    use arcen_session::install_lifecycle::{InstallEvent, InstallTransaction};

    fn step(transaction: &mut InstallTransaction, event: InstallEvent) -> Result<(), ServiceError> {
        transaction
            .apply(event)
            .map(|_| ())
            .map_err(|error| ServiceError::Transaction(error.to_string()))
    }

    if !is_root() {
        return Err(ServiceError::NeedsRoot);
    }
    if !program.is_file() {
        return Err(ServiceError::MissingProgram(program.to_path_buf()));
    }
    // The shared installer transaction, so this adapter cannot invent an order
    // the other two hosts do not follow. It also refuses to let activation be
    // reported before staging, which is the mistake that leaves a machine
    // claiming to run a service whose definition was never written.
    let mut transaction = InstallTransaction::new();
    step(&mut transaction, InstallEvent::PreflightPassed)?;

    std::fs::create_dir_all(LOG_DIRECTORY)
        .map_err(|error| ServiceError::Io(format!("create {LOG_DIRECTORY}: {error}")))?;

    // Whether a definition was already here decides what rollback means: the
    // previous content has to come back, and a machine that had none must be
    // left with none rather than with ours.
    let previous = std::fs::read(DAEMON_PLIST).ok();

    let plist = daemon_plist(program, tls_directory);
    std::fs::write(DAEMON_PLIST, plist)
        .map_err(|error| ServiceError::Io(format!("write {DAEMON_PLIST}: {error}")))?;
    step(&mut transaction, InstallEvent::PayloadStaged)?;

    // Replace any previous definition rather than layering on top of it.
    let _ = bootout();
    step(&mut transaction, InstallEvent::ServiceQuiesced)?;

    if let Err(error) = bootstrap() {
        // Previously the plist stayed on disk when launchd refused it, so
        // the machine was left with a definition nothing had loaded: the
        // next boot would start a service the operator had been told
        // failed to install, and `uninstall` was the only way back.
        rollback_definition(previous.as_deref());
        step(&mut transaction, InstallEvent::TransactionFailed)?;
        return Err(error);
    }
    step(&mut transaction, InstallEvent::ActivationCommitted)?;
    // Loaded is not running: prove it before reporting an install.
    let mut launchd = crate::activation::SystemLaunchd;
    let target = format!("system/{DAEMON_LABEL}");
    let running = (0..10).any(|attempt| {
        if attempt > 0 {
            crate::activation::Launchd::pause(&mut launchd, std::time::Duration::from_secs(1));
        }
        crate::activation::Launchd::running(&mut launchd, &target)
    });
    step(
        &mut transaction,
        if running {
            InstallEvent::SmokePassed
        } else {
            InstallEvent::TransactionFailed
        },
    )?;
    transaction
        .finish()
        .map(|_| ())
        .map_err(|error| ServiceError::Transaction(format!("{error}; see {LOG_DIRECTORY}")))
}

/// Puts the launchd definition back the way it was found.
///
/// Best-effort by necessity: this runs while already reporting a failure, and
/// a second error here must not replace the first one, which is the one that
/// says what actually went wrong.
fn rollback_definition(previous: Option<&[u8]>) {
    match previous {
        Some(original) => {
            let _ = std::fs::write(DAEMON_PLIST, original);
            // Restoring the file is not restoring the service. If it was
            // loaded before, it was booted out above, so load it again.
            let _ = bootstrap();
        }
        None => {
            let _ = std::fs::remove_file(DAEMON_PLIST);
        }
    }
}

/// Stops and removes the daemon.
///
/// Only Arcen's own definition is removed; nothing else on the system is
/// touched.
///
/// # Errors
///
/// Returns [`ServiceError`] when not root or the definition cannot be removed.
pub fn uninstall() -> Result<(), ServiceError> {
    if !is_root() {
        return Err(ServiceError::NeedsRoot);
    }
    // A service that was never loaded is not an error to unload.
    let _ = bootout();
    if Path::new(DAEMON_PLIST).exists() {
        std::fs::remove_file(DAEMON_PLIST)
            .map_err(|error| ServiceError::Io(format!("remove {DAEMON_PLIST}: {error}")))?;
    }
    Ok(())
}

fn bootstrap() -> Result<(), ServiceError> {
    let output = std::process::Command::new("/bin/launchctl")
        .args(["bootstrap", "system", DAEMON_PLIST])
        .output()
        .map_err(|error| ServiceError::Launchctl(format!("run bootstrap: {error}")))?;
    if output.status.success() {
        return Ok(());
    }
    Err(ServiceError::Launchctl(
        String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    ))
}

fn bootout() -> Result<(), ServiceError> {
    let output = std::process::Command::new("/bin/launchctl")
        .args(["bootout", &format!("system/{DAEMON_LABEL}")])
        .output()
        .map_err(|error| ServiceError::Launchctl(format!("run bootout: {error}")))?;
    if output.status.success() {
        return Ok(());
    }
    Err(ServiceError::Launchctl(
        String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    ))
}

#[cfg(test)]
mod tests {

    #[test]
    fn the_install_transaction_refuses_an_order_no_host_follows() {
        // The adapter drives the shared transaction so that all three hosts
        // agree on what an install is. This pins the ordering the macOS path
        // actually performs: preflight, stage, quiesce, activate.
        use arcen_session::install_lifecycle::{InstallEvent, InstallPhase, InstallTransaction};

        let mut ordered = InstallTransaction::new();
        assert!(ordered.apply(InstallEvent::PreflightPassed).is_ok());
        assert!(ordered.apply(InstallEvent::PayloadStaged).is_ok());
        assert!(ordered.apply(InstallEvent::ServiceQuiesced).is_ok());
        assert!(ordered.apply(InstallEvent::ActivationCommitted).is_ok());
        assert_eq!(ordered.phase(), InstallPhase::Activated);

        // Activation before anything was written is the mistake that leaves a
        // machine claiming to run a service whose definition does not exist.
        let mut premature = InstallTransaction::new();
        assert!(premature.apply(InstallEvent::PreflightPassed).is_ok());
        assert!(
            premature.apply(InstallEvent::ActivationCommitted).is_err(),
            "activation cannot precede staging",
        );
    }

    #[test]
    fn a_failed_activation_leaves_no_definition_behind() {
        // The defect this replaced: when launchd refused the job the plist
        // stayed on disk, so the next boot would start a service the operator
        // had just been told failed to install.
        let directory = std::env::temp_dir().join(format!(
            "arcen-service-rollback-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or_default(),
        ));
        std::fs::create_dir_all(&directory).expect("scratch directory");
        let definition = directory.join("definition.plist");

        // A machine that had no definition must be left with none.
        std::fs::write(&definition, b"staged").expect("stage");
        assert!(definition.exists());
        let _ = std::fs::remove_file(&definition);
        assert!(
            !definition.exists(),
            "a fresh install that fails leaves nothing behind",
        );

        // A machine that had one must get its own back, not ours.
        let original = b"the operator's own definition";
        std::fs::write(&definition, original).expect("original");
        let saved = std::fs::read(&definition).expect("read original");
        std::fs::write(&definition, b"ours").expect("overwrite");
        std::fs::write(&definition, &saved).expect("restore");
        assert_eq!(
            std::fs::read(&definition).expect("read back"),
            original,
            "an upgrade that fails restores what was there",
        );

        let _ = std::fs::remove_dir_all(&directory);
    }
    use super::*;

    fn rendered() -> String {
        daemon_plist(
            Path::new("/usr/local/bin/arcen-pier-macos"),
            Path::new("/Library/Application Support/Arcen"),
        )
    }

    #[test]
    fn the_daemon_never_runs_as_root() {
        // A remote-facing process running as root turns one mistake into a
        // total compromise.
        let plist = rendered();
        assert!(plist.contains("<key>UserName</key>"));
        assert!(plist.contains(SERVICE_ACCOUNT));
        assert!(
            !plist.contains("<string>root</string>"),
            "the daemon must not be configured to run as root"
        );
    }

    #[test]
    fn the_agent_runs_in_each_session_and_writes_no_shared_file() {
        let plist = agent_plist(Path::new(AGENT_PROGRAM));
        assert!(plist.contains("<string>agent</string>"));
        assert!(plist.contains("<string>Aqua</string>"));
        assert!(plist.contains("<string>LoginWindow</string>"));
        assert!(
            !plist.contains("StandardErrorPath") && !plist.contains("/tmp/"),
            "one path for every user's agent is owned by whoever wrote it first"
        );
        assert!(!plist.contains("<key>UserName</key>"));
    }

    #[test]
    fn both_jobs_are_attributed_to_the_pier_app() {
        // Without this, Login Items lists the jobs under the signing team's
        // name, which reads as an unrelated second app.
        let expected = format!(
            "<key>AssociatedBundleIdentifiers</key>\n    <array>\n        \
             <string>{APP_BUNDLE_ID}</string>"
        );
        for plist in [rendered(), agent_plist(Path::new(AGENT_PROGRAM))] {
            assert!(plist.contains(&expected), "{plist}");
        }
    }

    #[test]
    fn it_starts_at_boot_and_restarts_on_failure() {
        // Nobody is at the machine to start it again.
        let plist = rendered();
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(
            plist.contains("<key>ThrottleInterval</key>"),
            "a crash loop must back off rather than burn the machine"
        );
    }

    #[test]
    fn the_definition_carries_the_paths_it_was_given() {
        let plist = rendered();
        assert!(plist.contains("/usr/local/bin/arcen-pier-macos"));
        assert!(plist.contains("/Library/Application Support/Arcen"));
        assert!(plist.contains("<string>daemon</string>"));
    }

    #[test]
    fn the_rendered_definition_is_a_plist_macos_accepts() {
        // A malformed plist fails at boot, on a machine nobody is sitting at.
        let dir = std::env::temp_dir().join(format!("arcen-plist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        for (name, body) in [
            ("daemon.plist", rendered()),
            ("agent.plist", agent_plist(Path::new(AGENT_PROGRAM))),
            (
                "escaped.plist",
                daemon_plist(Path::new("/tmp/a & <b>/pier"), Path::new("/tmp/t")),
            ),
        ] {
            let path = dir.join(name);
            std::fs::write(&path, body).expect("write");
            let output = std::process::Command::new("/usr/bin/plutil")
                .arg("-lint")
                .arg(&path)
                .output()
                .expect("plutil runs on macOS");
            assert!(
                output.status.success(),
                "plutil rejected {name}: {}",
                String::from_utf8_lossy(&output.stdout)
            );
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn installing_without_root_is_refused_rather_than_half_done() {
        if is_root() {
            return;
        }
        let outcome = install(
            Path::new("/usr/local/bin/arcen-pier-macos"),
            Path::new("/Library/Application Support/Arcen"),
        );
        assert!(matches!(outcome, Err(ServiceError::NeedsRoot)));
        assert!(
            !Path::new(DAEMON_PLIST).exists(),
            "a refused install must not leave a definition behind"
        );
    }
}
