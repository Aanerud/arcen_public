//! Starting the installed Pier and proving it runs, for the package.
//!
//! The package's `postinstall` puts files in place and then hands the one
//! question that decides success to this module: did the network service
//! come up and listen, and does every signed-in user have a desktop agent?
//! The answer goes through the shared installer transaction, whose `finish`
//! decides the exit status, so Installer reports success only for a host
//! that actually serves.
//!
//! A missing privacy approval is not a failure. Screen Recording and
//! Accessibility are granted by a person at the Mac, after the install; until
//! then a Deck can sign in and is told the picture is unavailable. That is
//! onboarding, and it is reported as such rather than as a failed install.

use std::path::{Path, PathBuf};
use std::time::Duration;

use arcen_session::install_lifecycle::{InstallEvent, InstallTransaction};

/// What activation needs from launchd and the network stack.
pub trait Launchd {
    /// `launchctl enable <target>`; best effort, as launchd itself treats it.
    fn enable(&mut self, target: &str);
    /// `launchctl bootstrap <domain> <plist>`, with launchd's error text.
    ///
    /// # Errors
    ///
    /// launchd's message when it refuses the definition.
    fn bootstrap(&mut self, domain: &str, plist: &Path) -> Result<(), String>;
    /// `launchctl kickstart -k <target>`: restart an already-loaded job.
    fn kickstart(&mut self, target: &str);
    /// Whether launchd reports `<target>` as running.
    fn running(&mut self, target: &str) -> bool;
    /// Whether something holds `port` over UDP.
    fn udp_port_in_use(&mut self, port: u16) -> bool;
    /// Waits between checks.
    fn pause(&mut self, duration: Duration);
}

/// What to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationPlan {
    pub service_label: String,
    pub service_plist: PathBuf,
    pub agent_label: String,
    pub agent_plist: PathBuf,
    pub port: u16,
    /// The signed-in users whose sessions get an agent now. Empty when only
    /// the login window is on screen.
    pub agent_uids: Vec<u32>,
}

impl ActivationPlan {
    /// The installed layout, for the users the package found signed in.
    #[must_use]
    pub fn installed(agent_uids: Vec<u32>, port: u16) -> Self {
        Self {
            service_label: crate::service::DAEMON_LABEL.to_owned(),
            service_plist: PathBuf::from(crate::service::DAEMON_PLIST),
            agent_label: crate::service::AGENT_LABEL.to_owned(),
            agent_plist: PathBuf::from(crate::service::AGENT_PLIST),
            port,
            agent_uids,
        }
    }
}

/// How long the service gets to listen, and each agent to start.
const SERVICE_CHECKS: u32 = 15;
const AGENT_CHECKS: u32 = 10;
const CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// Starts the service and agents in `plan`, proves they run, and returns the
/// lines to show the operator.
///
/// # Errors
///
/// A message naming what did not start, after the shared transaction has
/// recorded the failure.
pub fn activate(launchd: &mut impl Launchd, plan: &ActivationPlan) -> Result<Vec<String>, String> {
    fn apply(transaction: &mut InstallTransaction, event: InstallEvent) -> Result<(), String> {
        transaction
            .apply(event)
            .map(|_| ())
            .map_err(|error| format!("installer transaction: {error}"))
    }
    let mut transaction = InstallTransaction::new();
    for plist in [&plan.service_plist, &plan.agent_plist] {
        if !plist.is_file() {
            apply(&mut transaction, InstallEvent::TransactionFailed)?;
            return Err(format!(
                "the package did not install {}; nothing was started",
                plist.display()
            ));
        }
    }
    apply(&mut transaction, InstallEvent::PreflightPassed)?;
    // The payload is in place: the package wrote it before running this.
    apply(&mut transaction, InstallEvent::PayloadStaged)?;

    let service_target = format!("system/{}", plan.service_label);
    launchd.enable(&service_target);
    let agent_targets: Vec<(u32, String)> = plan
        .agent_uids
        .iter()
        .map(|uid| (*uid, format!("gui/{uid}/{}", plan.agent_label)))
        .collect();
    for (_, target) in &agent_targets {
        launchd.enable(target);
    }
    apply(&mut transaction, InstallEvent::ServiceQuiesced)?;

    let mut failures = Vec::new();
    let mut start = |domain: &str, plist: &Path, target: &str, what: &str| {
        match launchd.bootstrap(domain, plist) {
            Ok(()) => None,
            // Loaded by an earlier install: restart it on the new binary.
            Err(message) if message.to_ascii_lowercase().contains("already") => {
                launchd.kickstart(target);
                None
            }
            Err(message) => Some(format!("{what} could not be started: {message}")),
        }
    };
    failures.extend(start(
        "system",
        &plan.service_plist,
        &service_target,
        "the network service",
    ));
    for (uid, target) in &agent_targets {
        failures.extend(start(
            &format!("gui/{uid}"),
            &plan.agent_plist,
            target,
            &format!("the desktop agent for uid {uid}"),
        ));
    }
    if !failures.is_empty() {
        apply(&mut transaction, InstallEvent::TransactionFailed)?;
        return Err(failures.join("; "));
    }
    apply(&mut transaction, InstallEvent::ActivationCommitted)?;

    let mut report = Vec::new();
    let listening = (0..SERVICE_CHECKS).any(|attempt| {
        if attempt > 0 {
            launchd.pause(CHECK_INTERVAL);
        }
        launchd.running(&service_target) && launchd.udp_port_in_use(plan.port)
    });
    if listening {
        report.push(format!(
            "network service running and listening on UDP {}",
            plan.port
        ));
    } else {
        failures.push(format!(
            "the network service is not listening on UDP {}; see {}/service.log",
            plan.port,
            crate::service::LOG_DIRECTORY
        ));
    }
    for (uid, target) in &agent_targets {
        let started = (0..AGENT_CHECKS).any(|attempt| {
            if attempt > 0 {
                launchd.pause(CHECK_INTERVAL);
            }
            launchd.running(target)
        });
        if started {
            report.push(format!("desktop agent running for uid {uid}"));
        } else {
            failures.push(format!(
                "the desktop agent is not running for uid {uid}; see that user's \
                 ~/Library/Logs/Arcen/Pier/agent.log"
            ));
        }
    }
    if failures.is_empty() {
        apply(&mut transaction, InstallEvent::SmokePassed)?;
    } else {
        apply(&mut transaction, InstallEvent::TransactionFailed)?;
    }
    transaction
        .finish()
        .map(|_| report)
        .map_err(|error| format!("{error}: {}", failures.join("; ")))
}

/// The real launchd, by full path: installer scripts run with a minimal
/// `PATH`.
#[derive(Debug, Default)]
pub struct SystemLaunchd;

impl Launchd for SystemLaunchd {
    fn enable(&mut self, target: &str) {
        let _ = std::process::Command::new("/bin/launchctl")
            .args(["enable", target])
            .output();
    }

    fn bootstrap(&mut self, domain: &str, plist: &Path) -> Result<(), String> {
        let output = std::process::Command::new("/bin/launchctl")
            .arg("bootstrap")
            .arg(domain)
            .arg(plist)
            .output()
            .map_err(|error| format!("run launchctl: {error}"))?;
        if output.status.success() {
            return Ok(());
        }
        let mut message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if message.is_empty() {
            message = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        }
        Err(message)
    }

    fn kickstart(&mut self, target: &str) {
        let _ = std::process::Command::new("/bin/launchctl")
            .args(["kickstart", "-k", target])
            .output();
    }

    fn running(&mut self, target: &str) -> bool {
        std::process::Command::new("/bin/launchctl")
            .args(["print", target])
            .output()
            .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains("state = running"))
    }

    fn udp_port_in_use(&mut self, port: u16) -> bool {
        std::process::Command::new("/usr/sbin/lsof")
            .arg("-nP")
            .arg(format!("-iUDP:{port}"))
            .output()
            .is_ok_and(|output| output.status.success())
    }

    fn pause(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    #[derive(Default)]
    struct FakeLaunchd {
        refuse: HashMap<String, String>,
        running: HashSet<String>,
        listening: bool,
        kicked: Vec<String>,
    }

    impl Launchd for FakeLaunchd {
        fn enable(&mut self, _target: &str) {}
        fn bootstrap(&mut self, domain: &str, _plist: &Path) -> Result<(), String> {
            self.refuse
                .get(domain)
                .map_or(Ok(()), |error| Err(error.clone()))
        }
        fn kickstart(&mut self, target: &str) {
            self.kicked.push(target.to_owned());
        }
        fn running(&mut self, target: &str) -> bool {
            self.running.contains(target)
        }
        fn udp_port_in_use(&mut self, _port: u16) -> bool {
            self.listening
        }
        fn pause(&mut self, _duration: Duration) {}
    }

    fn plan(directory: &Path, agent_uids: Vec<u32>) -> ActivationPlan {
        let service_plist = directory.join("service.plist");
        let agent_plist = directory.join("agent.plist");
        std::fs::write(&service_plist, "<plist/>").unwrap();
        std::fs::write(&agent_plist, "<plist/>").unwrap();
        ActivationPlan {
            service_label: "pier.test.service".into(),
            service_plist,
            agent_label: "pier.test.agent".into(),
            agent_plist,
            port: 18444,
            agent_uids,
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("arcen-activation-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn healthy() -> FakeLaunchd {
        FakeLaunchd {
            running: ["system/pier.test.service", "gui/501/pier.test.agent"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            listening: true,
            ..FakeLaunchd::default()
        }
    }

    #[test]
    fn a_listening_service_and_running_agents_succeed() {
        let report = activate(&mut healthy(), &plan(&scratch("ok"), vec![501])).expect("ok");
        assert!(report[0].contains("listening on UDP 18444"), "{report:?}");
        assert!(report[1].contains("uid 501"), "{report:?}");
    }

    #[test]
    fn nobody_signed_in_is_not_a_failure() {
        let mut launchd = healthy();
        launchd.running.remove("gui/501/pier.test.agent");
        activate(&mut launchd, &plan(&scratch("login-window"), Vec::new())).expect("ok");
    }

    #[test]
    fn a_service_that_never_listens_fails_the_install() {
        let mut launchd = healthy();
        launchd.listening = false;
        let error = activate(&mut launchd, &plan(&scratch("deaf"), vec![501])).unwrap_err();
        assert!(error.contains("not listening on UDP 18444"), "{error}");
        assert!(error.contains("did not complete"), "{error}");
    }

    #[test]
    fn a_missing_agent_fails_the_install() {
        let mut launchd = healthy();
        launchd.running.remove("gui/501/pier.test.agent");
        let error = activate(&mut launchd, &plan(&scratch("agentless"), vec![501])).unwrap_err();
        assert!(
            error.contains("desktop agent is not running for uid 501"),
            "{error}"
        );
    }

    #[test]
    fn an_already_loaded_job_is_restarted_but_a_refusal_fails() {
        let mut launchd = healthy();
        launchd
            .refuse
            .insert("system".into(), "service already loaded".into());
        activate(&mut launchd, &plan(&scratch("upgrade"), vec![501])).expect("upgrade");
        assert_eq!(launchd.kicked, vec!["system/pier.test.service".to_owned()]);

        let mut launchd = healthy();
        launchd.refuse.insert(
            "gui/501".into(),
            "Bootstrap failed: 5: Input/output error".into(),
        );
        let error = activate(&mut launchd, &plan(&scratch("refused"), vec![501])).unwrap_err();
        assert!(error.contains("uid 501 could not be started"), "{error}");
    }

    #[test]
    fn a_package_without_its_definitions_starts_nothing() {
        let directory = scratch("empty");
        let mut plan = plan(&directory, vec![501]);
        plan.agent_plist = directory.join("missing.plist");
        let error = activate(&mut healthy(), &plan).unwrap_err();
        assert!(error.contains("nothing was started"), "{error}");
    }
}
