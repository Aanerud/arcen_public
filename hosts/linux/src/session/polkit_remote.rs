//! Arcen's polkit rule on this host and the proof that a desktop is remote
//! to polkit (see [`arcen_session::polkit_remote`]).
//!
//! The rule is installed by the installer and rewritten by the Pier at start
//! for the actions installed then; it names no session, so sign-ins never
//! write it. Before a desktop is launched, polkit's state alone may refuse
//! it ([`refusal_before_launch`]); after its session is active and before it
//! is handed over, polkit is asked (`pkcheck`) about a process in that
//! session for each installed probe action, and the shared [`admission`]
//! decides.

use std::collections::BTreeSet;
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use arcen_session::polkit_remote::{
    admission, console_self_test, console_self_test_candidate, is_arcen_rule,
    later_restrictive_policies, needs_checks, parse_policy_defaults, parse_polkit_version,
    refusal_before_launch, rule_text, supports_js_rules, valid_session_id, ActionDefaults,
    Admission, CatPath, ConsoleSelfTest, DesktopSeat, PolkitCheck, PolkitState, PolkitUnprotected,
    SessionRecord, ACTION_DIRS, LEGACY_RULE_PATHS, PROBE_ACTIONS, RULE_PATH, SESSION_RECORD_DIR,
    SITE_RULE_DIR,
};

use crate::logging::target;

/// How long one polkit tool may take.
const TOOL_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the whole console self-test may take (every `pkcheck` in it).
const SELF_TEST_DEADLINE: Duration = Duration::from_secs(5);

/// Where polkit's daemon lives on the distributions Arcen supports; one of
/// them existing means polkit is installed even when its tools do not run.
const POLKITD_PATHS: &[&str] = &[
    "/usr/lib/polkit-1/polkitd",
    "/usr/libexec/polkitd",
    "/usr/libexec/polkit-1/polkitd",
];

/// Where `pkla-compat` reads `.pkla` files (`<dir>/<NN-name.d>/<file>.pkla`).
const PKLA_DIRS: &[&str] = &[
    "/etc/polkit-1/localauthority",
    "/var/lib/polkit-1/localauthority",
];

/// The polkit tools, by name on `PATH` (a test points them elsewhere).
#[derive(Debug, Clone, Copy)]
struct Tools<'a> {
    pkaction: &'a str,
    pkcheck: &'a str,
}

const TOOLS: Tools<'static> = Tools {
    pkaction: "pkaction",
    pkcheck: "pkcheck",
};

/// polkit and Arcen's rule on this host, now.
#[must_use]
pub fn state() -> PolkitState {
    state_with(TOOLS, Path::new(RULE_PATH))
}

fn state_with(tools: Tools<'_>, rule_path: &Path) -> PolkitState {
    let Some(output) = output_with_timeout(tools.pkaction, &["--version"]) else {
        return if POLKITD_PATHS.iter().any(|path| Path::new(path).exists()) {
            PolkitState::Unknown
        } else {
            PolkitState::Absent
        };
    };
    let Some(version) = parse_polkit_version(&output) else {
        return PolkitState::Unknown;
    };
    if !supports_js_rules(version) {
        return PolkitState::NoRules(version);
    }
    if std::fs::read_to_string(rule_path).is_ok_and(|text| is_arcen_rule(&text)) {
        PolkitState::RuleInstalled
    } else {
        PolkitState::RuleMissing
    }
}

fn cat_path() -> CatPath {
    CatPath::CANDIDATES
        .into_iter()
        .find(|cat| Path::new(cat.path()).is_file())
        .unwrap_or(CatPath::UsrBin)
}

/// The actions installed now, with their defaults, from the `.policy` files
/// (no tool is run: a large `pkaction` listing cannot stall anything).
#[must_use]
pub fn installed_actions() -> Vec<ActionDefaults> {
    let mut actions = Vec::new();
    for dir in ACTION_DIRS {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut paths = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "policy"))
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            if let Ok(xml) = std::fs::read_to_string(&path) {
                actions.extend(parse_policy_defaults(&xml));
            }
        }
    }
    actions
}

fn rule_for(actions: &[ActionDefaults]) -> String {
    let pairs = actions
        .iter()
        .map(|action| (action.id.clone(), action.any))
        .collect::<Vec<_>>();
    rule_text(&pairs, cat_path())
}

/// The rule for the actions installed now.
#[must_use]
pub fn current_rule() -> String {
    rule_for(&installed_actions())
}

/// What [`install`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    Written,
    Unchanged,
}

/// Writes the rule for the actions installed now, atomically (a uniquely
/// named temporary file polkit ignores, fsync, rename; mode 0644), unless
/// the installed rule is already exactly that. Removes rules earlier Arcen
/// builds wrote. polkitd reloads its rules when the directory changes.
///
/// # Errors
///
/// When polkit has no JavaScript rules, the rules directory is missing, a
/// file at the rule's path is not Arcen's, or the file cannot be written.
pub fn install() -> Result<InstallOutcome, String> {
    match state() {
        PolkitState::Absent => return Err("polkit is not installed".into()),
        PolkitState::NoRules((major, minor)) => {
            return Err(format!(
                "polkit {major}.{minor} has no JavaScript rules (0.106 or later is needed)"
            ))
        }
        _ => {}
    }
    remove_legacy();
    let path = Path::new(RULE_PATH);
    let text = current_rule();
    match std::fs::read_to_string(path) {
        Ok(existing) if !is_arcen_rule(&existing) => {
            return Err(format!("{RULE_PATH} exists and is not Arcen's"));
        }
        Ok(existing) if existing == text => return Ok(InstallOutcome::Unchanged),
        _ => {}
    }
    let dir = path.parent().ok_or("rule path has no directory")?;
    if !dir.is_dir() {
        return Err(format!(
            "{} does not exist (no polkit rules directory)",
            dir.display()
        ));
    }
    let temporary = dir.join(format!(
        ".{}.{}.tmp",
        arcen_session::polkit_remote::RULE_FILE_NAME,
        std::process::id()
    ));
    let _ = std::fs::remove_file(&temporary);
    let written = (|| {
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("{RULE_PATH}: {error}"));
    }
    Ok(InstallOutcome::Written)
}

/// Removes the rule (and rules earlier Arcen builds wrote) when they are
/// Arcen's; `Ok(false)` when there was none.
///
/// # Errors
///
/// When an Arcen rule is there and cannot be removed.
pub fn remove() -> Result<bool, String> {
    let legacy = remove_legacy();
    match std::fs::read_to_string(RULE_PATH) {
        Ok(text) if is_arcen_rule(&text) => std::fs::remove_file(RULE_PATH)
            .map(|()| true)
            .map_err(|error| format!("{RULE_PATH}: {error}")),
        _ => Ok(legacy),
    }
}

fn remove_legacy() -> bool {
    let mut removed = false;
    for path in LEGACY_RULE_PATHS {
        if std::fs::read_to_string(path).is_ok_and(|text| is_arcen_rule(&text))
            && std::fs::remove_file(path).is_ok()
        {
            removed = true;
        }
    }
    removed
}

/// Site restrictions that Arcen's rule now answers before (see
/// [`later_restrictive_policies`]): site rules numbered after it that return
/// `NO`/`AUTH_ADMIN`, and `.pkla` files restricting active sessions. The
/// rules a distribution ships are not read: Arcen's rule answers before them
/// on purpose, and their files are not the administrator's to renumber.
#[must_use]
pub fn later_site_restrictions() -> Vec<String> {
    let rules = read_files(Path::new(SITE_RULE_DIR), "rules");
    let mut pkla = Vec::new();
    for dir in PKLA_DIRS {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let sub = entry.path();
            if sub.is_dir() {
                let prefix = format!("{dir}/{}", entry.file_name().to_string_lossy());
                pkla.extend(
                    read_files(&sub, "pkla")
                        .into_iter()
                        .map(|(name, text)| (format!("{prefix}/{name}"), text)),
                );
            }
        }
    }
    later_restrictive_policies(&rules, &pkla)
}

fn read_files(dir: &Path, extension: &str) -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == extension))
        .filter_map(|entry| {
            let text = std::fs::read_to_string(entry.path()).ok()?;
            Some((entry.file_name().to_string_lossy().into_owned(), text))
        })
        .collect()
}

/// logind's records of the sessions alive now.
fn session_records() -> Vec<(String, SessionRecord)> {
    let Ok(entries) = std::fs::read_dir(SESSION_RECORD_DIR) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let id = entry.file_name().to_string_lossy().into_owned();
            if !valid_session_id(&id) {
                return None;
            }
            let text = std::fs::read_to_string(entry.path()).ok()?;
            Some((id, SessionRecord::parse(&text)))
        })
        .collect()
}

/// The console self-test: asks polkit about a local seated session's leader
/// for each installed probe action its defaults promise it. A `Stripped`
/// answer means the rule fails closed for the console (it could not read the
/// session record) or a site rule denies it. `None` when no local seated
/// session exists. The whole test ends within [`SELF_TEST_DEADLINE`].
#[must_use]
pub fn console_self_test_now(
    actions: &[ActionDefaults],
) -> Option<(String, String, ConsoleSelfTest)> {
    let deadline = Instant::now() + SELF_TEST_DEADLINE;
    let records = session_records();
    let (id, record) = console_self_test_candidate(&records)?;
    let leader = record.leader?;
    let subject = process_subject(leader)?;
    let mut last = None;
    for action in actions
        .iter()
        .filter(|action| PROBE_ACTIONS.contains(&action.id.as_str()))
    {
        if Instant::now() >= deadline {
            break;
        }
        let status = status_until(
            TOOLS.pkcheck,
            &["--process", &subject, "--action-id", &action.id],
            deadline,
        )
        .unwrap_or(-1);
        let verdict = console_self_test(action, record.active, status);
        if verdict != ConsoleSelfTest::Inconclusive {
            return Some((id.clone(), action.id.clone(), verdict));
        }
        last = Some((id.clone(), action.id.clone(), verdict));
    }
    last
}

fn log_self_test(actions: &[ActionDefaults]) {
    match console_self_test_now(actions) {
        None => tracing::info!(
            target: target::SESSION,
            "polkit console self-test skipped: no local seated session (run `arcen-pier polkit-rule status` while someone is signed in at the console)"
        ),
        Some((session_id, action, ConsoleSelfTest::Stripped)) => tracing::warn!(
            target: target::SESSION,
            session_id,
            action,
            "polkit refuses the console a grant its defaults promise: Arcen's rule may be failing closed (polkit cannot read logind's session records with cat) or a site rule denies it; see `journalctl -u polkit` for `arcen:` lines (a polkitd started with `--log-level=err`, as on EL10, logs none)"
        ),
        Some((session_id, action, verdict)) => tracing::info!(
            target: target::SESSION,
            session_id,
            action,
            verdict = ?verdict,
            "polkit console self-test"
        ),
    }
}

fn log_later_restrictions() {
    let later = later_site_restrictions();
    if !later.is_empty() {
        tracing::warn!(
            target: target::SESSION,
            files = ?later,
            "site polkit restrictions sort after Arcen's rule and no longer reach remote seated desktops for the actions it answers; renumber them 00-04"
        );
    }
}

/// At Pier start: the rule is (re)written for the actions installed now, so
/// a package installed since, or an upgrade from a build without the rule,
/// is covered. Failure is logged; seated desktops are then refused before
/// launch. Also warns about site restrictions the rule bypasses and runs the
/// console self-test.
pub fn ensure_at_start() {
    let state = state();
    match state {
        PolkitState::Absent => {
            tracing::info!(target: target::SESSION, "polkit is not installed; no polkit rule needed");
            return;
        }
        PolkitState::NoRules((major, minor)) => {
            tracing::error!(
                target: target::SESSION,
                polkit_version = format!("{major}.{minor}"),
                reason = PolkitUnprotected::NoRules.token(),
                "SECURITY: polkit on this host is older than 0.106 and cannot be given Arcen's rule; an X11 desktop's remote user gets the console's polkit rights (power off, reboot, network, mount). Upgrade polkit, or see the administration guide"
            );
            return;
        }
        _ => {}
    }
    let started = Instant::now();
    match install() {
        Ok(outcome) => {
            tracing::info!(
                target: target::SESSION,
                path = RULE_PATH,
                written = outcome == InstallOutcome::Written,
                elapsed_ms = started.elapsed().as_millis(),
                "polkit remote-session rule is current for the installed actions"
            );
            // Off the start-up path: a hung polkitd must never delay the
            // Pier's start or its signal handling.
            let written = outcome == InstallOutcome::Written;
            let spawned = std::thread::Builder::new()
                .name("arcen-polkit-self-test".into())
                .spawn(move || {
                    if written {
                        // polkitd reloads on the directory change; give it
                        // a moment before the self-test asks it anything.
                        std::thread::sleep(Duration::from_millis(500));
                    }
                    log_later_restrictions();
                    log_self_test(&installed_actions());
                });
            if let Err(error) = spawned {
                tracing::warn!(target: target::SESSION, %error, "polkit self-test not started");
            }
        }
        Err(error) => tracing::error!(
            target: target::SESSION,
            %error,
            path = RULE_PATH,
            "SECURITY: polkit remote-session rule could not be written; X11 desktops will be refused until it is"
        ),
    }
}

/// `arcen-pier polkit-rule install|remove|status|print`.
#[must_use]
pub fn cli_main(args: &[String]) -> std::process::ExitCode {
    use std::process::ExitCode;
    match args.first().map(String::as_str) {
        Some("install") if state() == PolkitState::Absent => {
            println!("polkit: not installed on this host; no rule needed");
            ExitCode::SUCCESS
        }
        Some("install") => match install() {
            Ok(InstallOutcome::Written) => {
                println!("polkit: wrote {RULE_PATH}");
                ExitCode::SUCCESS
            }
            Ok(InstallOutcome::Unchanged) => {
                println!("polkit: {RULE_PATH} is current");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("polkit: rule not installed: {error}");
                ExitCode::FAILURE
            }
        },
        Some("remove") => match remove() {
            Ok(true) => {
                println!("polkit: removed Arcen's rule");
                ExitCode::SUCCESS
            }
            Ok(false) => {
                println!("polkit: no Arcen rule installed");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("polkit: {error}");
                ExitCode::FAILURE
            }
        },
        Some("status") => {
            let state = state();
            println!("polkit: {}", state_words(state));
            if matches!(state, PolkitState::RuleInstalled)
                && std::fs::read_to_string(RULE_PATH).ok().as_deref()
                    != Some(current_rule().as_str())
            {
                println!("polkit: the installed rule predates actions installed since; the Pier rewrites it at start");
            }
            if matches!(state, PolkitState::RuleInstalled | PolkitState::RuleMissing) {
                for file in later_site_restrictions() {
                    println!(
                        "polkit: WARNING: {file} restricts actions after Arcen's rule; for remote seated desktops Arcen's rule answers first, so renumber the restriction 00-04"
                    );
                }
                match console_self_test_now(&installed_actions()) {
                    None => println!("polkit: console self-test skipped (no local seated session)"),
                    Some((session, action, ConsoleSelfTest::Stripped)) => println!(
                        "polkit: WARNING: console session {session} is refused {action}, which its defaults grant: Arcen's rule may be failing closed (see `journalctl -u polkit` for `arcen:` lines, which a polkitd started with `--log-level=err` does not log) or a site rule denies it"
                    ),
                    Some((session, action, verdict)) => {
                        println!("polkit: console self-test on session {session} ({action}): {verdict:?}");
                    }
                }
            }
            ExitCode::SUCCESS
        }
        Some("print") => {
            print!("{}", current_rule());
            ExitCode::SUCCESS
        }
        Some("check") => match args.get(1).and_then(|pid| pid.parse::<u32>().ok()) {
            Some(pid) => check_cli(pid),
            None => {
                eprintln!("usage: arcen-pier polkit-rule check <pid of a process in the desktop's session>");
                ExitCode::from(2)
            }
        },
        _ => {
            eprintln!("usage: arcen-pier polkit-rule install|remove|status|print|check <pid>");
            ExitCode::from(2)
        }
    }
}

/// `polkit-rule check <pid>`: the admission a desktop whose session holds
/// `pid` would get now, by the same path the launcher takes.
fn check_cli(pid: u32) -> std::process::ExitCode {
    use std::process::ExitCode;
    let Some(session_id) = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .ok()
        .and_then(|cgroup| arcen_session::polkit_remote::session_id_from_cgroup(&cgroup))
    else {
        eprintln!("polkit: process {pid} is in no logind session");
        return ExitCode::from(2);
    };
    let record = std::fs::read_to_string(format!("{SESSION_RECORD_DIR}/{session_id}"))
        .map(|text| SessionRecord::parse(&text))
        .unwrap_or_default();
    let seat = DesktopSeat::from_seat(&record.seat);
    let started = Instant::now();
    let state = state();
    let refusal = refusal_before_launch(state, seat);
    let checks = if needs_checks(state, seat) {
        check_process(pid, &installed_actions())
    } else {
        Vec::new()
    };
    let decision = refusal.map_or_else(|| admission(state, seat, &checks), Admission::Refuse);
    println!(
        "polkit: session {session_id} seat={:?} ({seat:?}) remote_by_rule={} state={state:?}",
        record.seat,
        record.is_remote()
    );
    for check in &checks {
        println!("polkit:   {} -> pkcheck {}", check.action, check.status);
    }
    println!(
        "polkit: admission {decision:?} ({} ms)",
        started.elapsed().as_millis()
    );
    if matches!(decision, Admission::Refuse(_)) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn state_words(state: PolkitState) -> String {
    match state {
        PolkitState::Absent => "not installed (nothing to protect)".into(),
        PolkitState::Unknown => "installed, but pkaction did not answer".into(),
        PolkitState::NoRules((major, minor)) => {
            format!("{major}.{minor}: no JavaScript rules; X11 desktops are NOT protected")
        }
        PolkitState::RuleMissing => format!("rules supported; {RULE_PATH} is NOT installed"),
        PolkitState::RuleInstalled => format!("rules supported; {RULE_PATH} installed"),
    }
}

/// The seat a desktop's logind session sits on: an X11 desktop on the
/// console seat, a native Wayland desktop on its own Arcen seat
/// (`seat-arcen-pen-…`, `wayland.session_mode`), a headless one on none.
#[must_use]
pub const fn desktop_seat(
    provider: arcen_session::desktop_provider::DesktopProvider,
    native_seat: bool,
) -> DesktopSeat {
    match provider {
        arcen_session::desktop_provider::DesktopProvider::Xorg => DesktopSeat::ConsoleSeat,
        arcen_session::desktop_provider::DesktopProvider::Wayland if native_seat => {
            DesktopSeat::ArcenSeat
        }
        arcen_session::desktop_provider::DesktopProvider::Wayland => DesktopSeat::Seatless,
    }
}

/// Before a desktop is launched: the refusal polkit's `state` alone decides
/// for a desktop on `seat` (logged), so a refused remote user never gets a
/// seated session.
#[must_use]
pub fn refusal_logged(state: PolkitState, seat: DesktopSeat) -> Option<PolkitUnprotected> {
    let refusal = refusal_before_launch(state, seat);
    if let Some(reason) = refusal {
        tracing::error!(
            target: target::SESSION,
            seat = ?seat,
            polkit = ?state,
            reason = reason.token(),
            explanation = reason.explanation(),
            "SECURITY: this desktop is refused before launch: polkit would not keep its remote user from the console's rights"
        );
    }
    refusal
}

/// Asks polkit, for the process `pid` in a desktop's session, about each
/// installed [`PROBE_ACTIONS`] action (`pkcheck --process`, no user
/// interaction), in parallel.
#[must_use]
pub fn check_process(pid: u32, installed: &[ActionDefaults]) -> Vec<PolkitCheck> {
    check_process_with(TOOLS, pid, installed)
}

fn check_process_with(
    tools: Tools<'_>,
    pid: u32,
    installed: &[ActionDefaults],
) -> Vec<PolkitCheck> {
    let Some(subject) = process_subject(pid) else {
        return Vec::new();
    };
    let installed = installed
        .iter()
        .map(|action| action.id.as_str())
        .collect::<BTreeSet<_>>();
    std::thread::scope(|scope| {
        let handles = PROBE_ACTIONS
            .iter()
            .filter(|action| installed.contains(**action))
            .map(|action| {
                let subject = &subject;
                scope.spawn(move || PolkitCheck {
                    action: (*action).to_owned(),
                    status: status_with_timeout(
                        tools.pkcheck,
                        &["--process", subject, "--action-id", action],
                    )
                    .unwrap_or(-1),
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| {
                handle.join().unwrap_or_else(|_| PolkitCheck {
                    action: String::new(),
                    status: -1,
                })
            })
            .collect()
    })
}

/// The proof after launch for one desktop, given the state seen before
/// launch: the checks when they are needed, and the shared decision, logged
/// with its cost.
#[must_use]
pub fn prove_desktop(
    pid: u32,
    session_id: &str,
    seat: DesktopSeat,
    state: PolkitState,
) -> Admission {
    prove_desktop_checks(pid, session_id, seat, state).0
}

/// [`prove_desktop`], with the `pkcheck` answers it decided on (for the
/// qualification record). The desktop is judged on the stricter of the
/// launch plan's seat and the seat logind recorded for its session.
#[must_use]
pub fn prove_desktop_checks(
    pid: u32,
    session_id: &str,
    planned_seat: DesktopSeat,
    state: PolkitState,
) -> (Admission, Vec<PolkitCheck>) {
    let started = Instant::now();
    let record = valid_session_id(session_id)
        .then(|| std::fs::read_to_string(format!("{SESSION_RECORD_DIR}/{session_id}")).ok())
        .flatten()
        .map(|text| SessionRecord::parse(&text));
    let seat = record.as_ref().map_or(planned_seat, |record| {
        planned_seat.stricter(DesktopSeat::from_seat(&record.seat))
    });
    let checks = if needs_checks(state, seat) {
        check_process(pid, &installed_actions())
    } else {
        Vec::new()
    };
    let decision = admission(state, seat, &checks);
    let session_remote = record.as_ref().map(SessionRecord::is_remote);
    if seat != planned_seat {
        tracing::warn!(
            target: target::SESSION,
            session_id,
            planned_seat = ?planned_seat,
            seat = ?seat,
            "logind put this desktop's session on a seat its launch plan did not name; judged on logind's seat"
        );
    }
    let checks_text = checks
        .iter()
        .map(|check| format!("{}={}", check.action, check.status))
        .collect::<Vec<_>>();
    let elapsed_ms = started.elapsed().as_millis();
    match decision {
        Admission::Admit => tracing::info!(
            target: target::SESSION,
            session_id,
            seat = ?seat,
            polkit = ?state,
            checks = ?checks_text,
            session_remote,
            elapsed_ms,
            "polkit refuses this desktop's session the console's grants"
        ),
        Admission::AdmitUnverified => tracing::warn!(
            target: target::SESSION,
            session_id,
            seat = ?seat,
            polkit = ?state,
            checks = ?checks_text,
            elapsed_ms,
            "polkit could not be asked about this seatless desktop; admitted (polkit treats a seatless session as remote)"
        ),
        Admission::AdmitUnprotected(reason) => tracing::error!(
            target: target::SESSION,
            session_id,
            seat = ?seat,
            polkit = ?state,
            checks = ?checks_text,
            reason = reason.token(),
            explanation = reason.explanation(),
            elapsed_ms,
            "SECURITY: this desktop is admitted although polkit gives its remote user the console's rights"
        ),
        Admission::Refuse(reason) => tracing::error!(
            target: target::SESSION,
            session_id,
            seat = ?seat,
            polkit = ?state,
            checks = ?checks_text,
            session_remote,
            reason = reason.token(),
            explanation = reason.explanation(),
            elapsed_ms,
            "SECURITY: this desktop is refused: polkit would not keep its remote user from the console's rights; tearing it down"
        ),
    }
    (decision, checks)
}

/// `pid,start-time,uid` for `pkcheck --process` (the start time keeps a
/// reused pid from being asked about).
fn process_subject(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    // Fields after `comm`: state is field 3, start time field 22.
    let start = after_comm.split_whitespace().nth(19)?;
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let uid = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?;
    Some(format!("{pid},{start},{uid}"))
}

/// Runs `program`, reading its standard output while it runs (a full pipe
/// never stalls it), and gives up after [`TOOL_TIMEOUT`]. `None` when it did
/// not start, failed, or timed out.
fn output_with_timeout(program: &str, args: &[&str]) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut text = Vec::new();
        let _ = stdout.read_to_end(&mut text);
        text
    });
    let status = wait_until(&mut child, Instant::now() + TOOL_TIMEOUT);
    // The pipe closes when the child exits or is killed, so this returns.
    let text = reader.join().unwrap_or_default();
    status
        .filter(std::process::ExitStatus::success)
        .map(|_| String::from_utf8_lossy(&text).into_owned())
}

fn status_with_timeout(program: &str, args: &[&str]) -> Option<i32> {
    status_until(program, args, Instant::now() + TOOL_TIMEOUT)
}

fn status_until(program: &str, args: &[&str], deadline: Instant) -> Option<i32> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    wait_until(&mut child, deadline).and_then(|status| status.code())
}

fn wait_until(
    child: &mut std::process::Child,
    deadline: Instant,
) -> Option<std::process::ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// Stand-in `pkaction`/`pkcheck` in a fresh directory: `pkaction
    /// --version` prints `version`, `pkaction` lists `installed`, and
    /// `pkcheck` exits `granted_status` for power-off and 2 otherwise.
    fn fake_tools(name: &str, version: &str, installed: &[&str], granted_status: i32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("arcen-polkit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("fake tool dir");
        let pkaction = dir.join("pkaction");
        std::fs::write(
            &pkaction,
            format!(
                "#!/bin/sh\n[ \"$1\" = --version ] && {{ echo 'pkaction version {version}'; exit 0; }}\nprintf '%s\\n' {}\n",
                installed.join(" ")
            ),
        )
        .expect("fake pkaction");
        let pkcheck = dir.join("pkcheck");
        std::fs::write(
            &pkcheck,
            format!(
                "#!/bin/sh\n[ \"$4\" = org.freedesktop.login1.power-off ] && exit {granted_status}\nexit 2\n"
            ),
        )
        .expect("fake pkcheck");
        for tool in [&pkaction, &pkcheck] {
            std::fs::set_permissions(tool, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        dir
    }

    fn tool_paths(dir: &Path) -> (String, String) {
        (
            dir.join("pkaction").display().to_string(),
            dir.join("pkcheck").display().to_string(),
        )
    }

    #[test]
    fn the_launch_check_asks_about_each_installed_probe_action_and_decides() {
        let installed = [
            "org.freedesktop.login1.power-off",
            "org.freedesktop.login1.reboot",
            "org.example.unrelated",
        ];
        let installed_defaults = installed
            .iter()
            .map(|id| ActionDefaults {
                id: (*id).to_owned(),
                any: arcen_session::polkit_remote::ImplicitAuth::AuthAdminKeep,
                inactive: arcen_session::polkit_remote::ImplicitAuth::AuthAdminKeep,
                active: arcen_session::polkit_remote::ImplicitAuth::Yes,
            })
            .collect::<Vec<_>>();
        let dir = fake_tools("checks", "0.117", &installed, 2);
        let (pkaction, pkcheck) = tool_paths(&dir);
        let tools = Tools {
            pkaction: &pkaction,
            pkcheck: &pkcheck,
        };
        let checks = check_process_with(tools, std::process::id(), &installed_defaults);
        assert_eq!(
            checks
                .iter()
                .map(|check| (check.action.as_str(), check.status))
                .collect::<Vec<_>>(),
            [
                ("org.freedesktop.login1.power-off", 2),
                ("org.freedesktop.login1.reboot", 2)
            ],
            "only installed probe actions are asked about"
        );
        assert_eq!(
            admission(
                PolkitState::RuleInstalled,
                DesktopSeat::ConsoleSeat,
                &checks
            ),
            Admission::Admit
        );
        let granted = fake_tools("granted", "0.117", &installed, 0);
        let (pkaction, pkcheck) = tool_paths(&granted);
        let checks = check_process_with(
            Tools {
                pkaction: &pkaction,
                pkcheck: &pkcheck,
            },
            std::process::id(),
            &installed_defaults,
        );
        assert_eq!(
            admission(
                PolkitState::RuleInstalled,
                DesktopSeat::ConsoleSeat,
                &checks
            ),
            Admission::Refuse(arcen_session::polkit_remote::PolkitUnprotected::Granted)
        );
        assert!(
            check_process_with(tools, u32::MAX, &installed_defaults).is_empty(),
            "a process that is gone is not asked about"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&granted);
    }

    #[test]
    fn the_state_follows_the_polkit_version_and_the_installed_rule() {
        let modern = fake_tools("state-modern", "0.117", &[], 2);
        let old = fake_tools("state-old", "0.105", &[], 2);
        let rule = modern.join(arcen_session::polkit_remote::RULE_FILE_NAME);
        let (pkaction, pkcheck) = tool_paths(&modern);
        let modern_tools = Tools {
            pkaction: &pkaction,
            pkcheck: &pkcheck,
        };
        assert_eq!(state_with(modern_tools, &rule), PolkitState::RuleMissing);
        std::fs::write(&rule, "polkit.addRule(function () {});\n").expect("foreign");
        assert_eq!(
            state_with(modern_tools, &rule),
            PolkitState::RuleMissing,
            "someone else's file is not Arcen's rule"
        );
        std::fs::write(&rule, current_rule()).expect("rule");
        assert_eq!(state_with(modern_tools, &rule), PolkitState::RuleInstalled);
        let (pkaction, pkcheck) = tool_paths(&old);
        assert_eq!(
            state_with(
                Tools {
                    pkaction: &pkaction,
                    pkcheck: &pkcheck
                },
                &rule
            ),
            PolkitState::NoRules((0, 105))
        );
        let _ = std::fs::remove_dir_all(&modern);
        let _ = std::fs::remove_dir_all(&old);
    }

    #[test]
    fn a_process_subject_carries_pid_start_time_and_uid() {
        let pid = std::process::id();
        let subject = process_subject(pid).expect("own process");
        let parts = subject.split(',').collect::<Vec<_>>();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], pid.to_string());
        assert!(parts[1].parse::<u64>().is_ok());
        assert!(parts[2].parse::<u32>().is_ok());
        assert!(process_subject(u32::MAX).is_none());
    }

    #[test]
    fn a_tool_with_more_output_than_a_pipe_holds_is_read_while_it_runs() {
        let started = Instant::now();
        let output =
            output_with_timeout("/bin/sh", &["-c", "head -c 300000 /dev/zero | tr '\\0' a"])
                .expect("output");
        assert_eq!(output.len(), 300_000);
        assert!(started.elapsed() < TOOL_TIMEOUT, "no stall on a full pipe");
    }

    #[test]
    fn a_tool_past_its_deadline_is_killed() {
        let started = Instant::now();
        assert_eq!(
            status_until(
                "/bin/sh",
                &["-c", "sleep 5"],
                Instant::now() + Duration::from_millis(200)
            ),
            None
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn tools_that_do_not_exist_give_no_answer() {
        assert_eq!(
            output_with_timeout("/nonexistent/arcen-pkaction", &["--version"]),
            None
        );
        assert_eq!(status_with_timeout("/nonexistent/arcen-pkcheck", &[]), None);
        assert_eq!(status_with_timeout("/bin/sh", &["-c", "exit 2"]), Some(2));
    }

    #[test]
    fn the_rule_reads_records_with_a_cat_that_exists() {
        assert!(Path::new(cat_path().path()).is_file());
        assert!(current_rule().contains(cat_path().path()));
    }
}
