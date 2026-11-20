// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Keyboard, pointers and pen of a joined Xorg console (ADR 0025), kept to
//! that desktop.
//!
//! The session's uinput devices are offered to every reader on `seat0`, and
//! a joined console's X server is not one Arcen configures: a login screen,
//! another account's desktop or another product's X server on the same seat
//! would add them too. So on a joined console every device is admitted only
//! while the shared proof (`arcen_input::joined_input`) holds over the facts
//! `input::joined_devices` reads, and the shared `JoinedInputGuard` decides
//! what happens when it lapses:
//!
//! - at the start an essential device (keyboard, pointers) that is not
//!   proved refuses the attachment; an unproved pen is released, so the
//!   hello claims none;
//! - while a Deck is attached the proof is repeated every
//!   `JOINED_INPUT_RECHECK` (off the dispatcher) and whether the session is
//!   still in front every [`IN_FRONT_POLL`]; a lapse shuts the devices' gate
//!   at once (nothing is injected, not even releases), held input is
//!   released when the proof holds again within `JOINED_INPUT_GRACE`, and
//!   otherwise the attachment ends with a reason the Deck shows. A withdrawn
//!   pen stays withdrawn and the Deck is told.
//!
//! Glue only: the decisions are shared.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use arcen_input::joined_input::{
    deck_reason, JoinedDeviceGap, JoinedInputGuard, JoinedInputStep, JOINED_INPUT_RECHECK,
};
use arcen_input::virtual_device::VirtualInputRole;
use tokio::sync::mpsc;

use crate::input::joined_devices::{self, DeviceNode, JoinedDesktop, JoinedInputGate};
use crate::input::PierInput;
use crate::logging::target::INPUT;

/// How often the session's place in front of its seat is read between full
/// proofs (one small read of logind's seat record).
pub(crate) const IN_FRONT_POLL: Duration = Duration::from_millis(250);
/// How long the start waits for the joined X server to add the devices.
const SETTLE: Duration = Duration::from_secs(3);

/// A joined console's admitted input, ready to be watched.
#[derive(Debug)]
pub(crate) struct JoinedInputPlan {
    guard: JoinedInputGuard,
    gate: Arc<JoinedInputGate>,
    devices: Vec<(VirtualInputRole, Vec<DeviceNode>)>,
    joined: JoinedDesktop,
    /// Allowed readers already reported this attachment.
    reported: BTreeSet<String>,
}

impl JoinedInputPlan {
    /// Logs the allowed readers (system services, the joined session's own
    /// processes) not yet reported this attachment.
    fn report(&mut self, readers: BTreeSet<String>) {
        let new: Vec<_> = readers
            .into_iter()
            .filter(|reader| self.reported.insert(reader.clone()))
            .collect();
        if !new.is_empty() {
            tracing::info!(
                target: INPUT,
                display = self.joined.display,
                readers = %new.join(", "),
                "joined console input is also read by allowed readers (not another session or display server)"
            );
        }
    }

    /// The gate every injection passes, for the joined console's Native
    /// tablet.
    pub(crate) fn gate(&self) -> Arc<JoinedInputGate> {
        Arc::clone(&self.gate)
    }
}

/// What the attachment must do for its joined console's input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JoinedInputEvent {
    /// The pen was withdrawn; the Deck is told (Tablet support stops).
    PenWithdrawn(JoinedDeviceGap),
    /// Keyboard and pointer input could not be kept to the desktop: end the
    /// attachment with this reason.
    End(String),
}

/// Proves and admits a joined console's uinput devices before anything is
/// injected (or claimed in the hello).
///
/// # Errors
///
/// The reason the Deck is shown when an essential device is not proved to
/// reach the joined desktop alone: the attachment is refused.
#[allow(
    clippy::disallowed_methods,
    reason = "plain /proc, sysfs and logind file reads that finish (bounded by SETTLE); awaited to completion"
)]
pub(crate) async fn admit(
    input: &mut PierInput,
    metadata: &crate::session::lifecycle::SessionMetadata,
) -> Result<JoinedInputPlan, String> {
    let Some(joined) = super::joined_pen::joined_desktop(metadata) else {
        tracing::warn!(
            target: INPUT,
            display = %metadata.display,
            "joined console input refused: its display cannot be identified"
        );
        return Err(
            "session input refused: Arcen's input devices could not be kept to this desktop"
                .to_owned(),
        );
    };
    let mut devices = Vec::new();
    for role in VirtualInputRole::ALL {
        match input.device_nodes(role) {
            Some(Ok(nodes)) => devices.push((role, nodes)),
            Some(Err(error)) => {
                tracing::warn!(target: INPUT, ?role, %error, "a typed device's nodes could not be read");
                devices.push((role, Vec::new()));
            }
            None => {}
        }
    }
    let (proving, at) = (devices.clone(), joined.clone());
    let proved =
        tokio::task::spawn_blocking(move || joined_devices::prove_devices(&proving, &at, SETTLE))
            .await
            .unwrap_or_default();
    let mut reported = BTreeSet::new();
    for proved in &proved {
        tracing::info!(
            target: INPUT,
            role = ?proved.key,
            display = joined.display,
            proof = ?proved.proof,
            readers = %proved.readers,
            "joined console input device"
        );
        reported.extend(proved.reported.iter().cloned());
    }
    let proofs: Vec<_> = proved
        .into_iter()
        .map(|proved| (proved.key, proved.proof))
        .collect();
    let (guard, pen_gap) = JoinedInputGuard::start(&proofs).map_err(|refusal| {
        tracing::warn!(
            target: INPUT,
            role = ?refusal.role,
            gap = %refusal.gap,
            "joined console input refused: a device is not proved to reach this desktop alone"
        );
        refusal.deck_reason()
    })?;
    if let Some(gap) = pen_gap {
        tracing::warn!(
            target: INPUT,
            %gap,
            "the typed pen is not proved to reach this desktop alone; released, the session has no pen"
        );
        if let PierInput::Uinput(controller) = input {
            controller.release_tablet_device();
        }
        devices.retain(|(role, _)| *role != VirtualInputRole::TabletPen);
    }
    // Every injection also checks the joined session's terminal is in
    // front, so a terminal switch shuts input before anything is written.
    let joined_vt = joined_devices::session_vt(&joined.session_id);
    if joined_vt.is_none() {
        // Without its terminal, a switch away is seen only by the in-front
        // poll: up to IN_FRONT_POLL of input could follow it.
        tracing::warn!(
            target: INPUT,
            session = %joined.session_id,
            poll_ms = IN_FRONT_POLL.as_millis(),
            "joined console input: the session's terminal is unknown (no VTNr in its logind record); every injection cannot check it is in front, only the in-front poll does"
        );
    }
    let gate = Arc::new(JoinedInputGate::new(joined_vt));
    gate.follow(&guard);
    input.confine(&gate);
    let mut plan = JoinedInputPlan {
        guard,
        gate,
        devices,
        joined,
        reported: BTreeSet::new(),
    };
    plan.report(reported);
    Ok(plan)
}

/// Watches a joined console's input for one attachment; ends with it.
#[derive(Debug)]
pub(crate) struct JoinedInputWatch {
    events: mpsc::Receiver<JoinedInputEvent>,
    task: tokio::task::JoinHandle<()>,
    ended: bool,
}

/// Why the attachment ends when its input watch stopped on its own.
const WATCH_STOPPED: &str =
    "session input stopped: the host could no longer check its input devices";

impl JoinedInputWatch {
    /// The next event. A watch that stopped without ending the attachment
    /// (it failed) ends it: its devices are shut and nobody watches them.
    pub(crate) async fn next(&mut self) -> JoinedInputEvent {
        match self.events.recv().await {
            Some(JoinedInputEvent::End(reason)) => {
                self.ended = true;
                JoinedInputEvent::End(reason)
            }
            Some(event) => event,
            None if self.ended => std::future::pending().await,
            None => {
                self.ended = true;
                tracing::warn!(target: INPUT, "the joined console input watch stopped; the attachment ends");
                JoinedInputEvent::End(WATCH_STOPPED.to_owned())
            }
        }
    }
}

impl Drop for JoinedInputWatch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Starts watching `plan`'s devices; `input` is the session's injector.
pub(crate) fn watch(
    plan: JoinedInputPlan,
    input: Arc<Mutex<Option<PierInput>>>,
) -> JoinedInputWatch {
    let (sender, events) = mpsc::channel(4);
    let task = tokio::spawn(run(plan, input, sender));
    JoinedInputWatch {
        events,
        task,
        ended: false,
    }
}

/// Shuts every device when the watch stops for any reason (the attachment
/// ended, or the task failed): input is never left admitted unwatched.
struct ShutOnDrop(Arc<JoinedInputGate>);

impl Drop for ShutOnDrop {
    fn drop(&mut self) {
        self.0.shut_all();
    }
}

/// A full proof running as its own task.
type Proving = tokio::task::JoinHandle<Vec<joined_devices::DeviceProof<VirtualInputRole>>>;

#[allow(
    clippy::disallowed_methods,
    reason = "plain /proc, sysfs and logind file reads that finish; the handle is polled and dropped with the watch"
)]
fn start_recheck(plan: &JoinedInputPlan, input: &Mutex<Option<PierInput>>) -> Proving {
    // The devices' nodes are read again each time: a handler that attached
    // since is part of the proof.
    let devices: Vec<_> = {
        let mut input = input.lock().unwrap_or_else(PoisonError::into_inner);
        plan.devices
            .iter()
            .map(|(role, nodes)| {
                let fresh = input
                    .as_mut()
                    .and_then(|input| input.device_nodes(*role))
                    .and_then(Result::ok);
                (*role, fresh.unwrap_or_else(|| nodes.clone()))
            })
            .collect()
    };
    let joined = plan.joined.clone();
    tokio::task::spawn_blocking(move || {
        joined_devices::prove_devices(&devices, &joined, Duration::ZERO)
    })
}

async fn run(
    mut plan: JoinedInputPlan,
    input: Arc<Mutex<Option<PierInput>>>,
    events: mpsc::Sender<JoinedInputEvent>,
) {
    let _shut = ShutOnDrop(Arc::clone(&plan.gate));
    let mut in_front_poll = tokio::time::interval(IN_FRONT_POLL);
    in_front_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first full proof runs at once: the admission's is from before
    // the handshake finished.
    let mut recheck = tokio::time::interval(JOINED_INPUT_RECHECK);
    recheck.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut proving: Option<Proving> = None;
    loop {
        // The in-front poll never waits for a full proof: the proof runs as
        // its own task.
        let step = tokio::select! {
            _ = in_front_poll.tick() => {
                let in_front = joined_devices::joined_in_front(&plan.joined);
                plan.guard.observe_in_front(in_front, Instant::now())
            }
            _ = recheck.tick(), if proving.is_none() => {
                proving = Some(start_recheck(&plan, &input));
                continue;
            }
            proved = async {
                match proving.as_mut() {
                    Some(task) => task.await,
                    None => std::future::pending().await,
                }
            }, if proving.is_some() => {
                proving = None;
                let proofs: Vec<_> = match proved {
                    Ok(proved) => proved
                        .into_iter()
                        .map(|proved| {
                            if proved.proof.is_err() {
                                tracing::debug!(target: INPUT, role = ?proved.key, proof = ?proved.proof, readers = %proved.readers, "joined console input recheck");
                            } else {
                                plan.report(proved.reported);
                            }
                            (proved.key, proved.proof)
                        })
                        .collect(),
                    // A proof that could not run proves nothing: every
                    // device lapses.
                    Err(_) => plan
                        .devices
                        .iter()
                        .map(|(role, _)| (*role, Err(JoinedDeviceGap::NoJoinedServer)))
                        .collect(),
                };
                plan.guard.observe(&proofs, Instant::now())
            }
        };
        if apply(&mut plan, step, &input, &events).await {
            return;
        }
    }
}

/// Acts on one step; `true` once the attachment must end. The gate changes
/// with the injector held, so it never shuts or opens in the middle of one
/// injection, and held input is released before anything new goes in.
async fn apply(
    plan: &mut JoinedInputPlan,
    step: JoinedInputStep,
    input: &Mutex<Option<PierInput>>,
    events: &mpsc::Sender<JoinedInputEvent>,
) -> bool {
    {
        let mut input = input.lock().unwrap_or_else(PoisonError::into_inner);
        plan.gate.follow(&plan.guard);
        if step.release_held {
            if let Some(input) = input.as_mut() {
                if let Err(error) = input.reset_held() {
                    tracing::warn!(target: INPUT, %error, "held input not released after the proof held again");
                }
            }
        }
        if step.pen_withdrawn.is_some() {
            // Destroying the device releases whatever it held to its
            // readers (release events reach nobody harmful) and takes it
            // from any reader that should not have it.
            if let Some(PierInput::Uinput(controller)) = input.as_mut() {
                controller.release_tablet_device();
            }
            plan.devices
                .retain(|(role, _)| *role != VirtualInputRole::TabletPen);
        }
    }
    if let Some((role, gap)) = step.lapsed.as_ref() {
        tracing::warn!(
            target: INPUT,
            ?role,
            %gap,
            "joined console input stopped: a device is not proved to reach this desktop alone"
        );
    }
    if step.release_held {
        tracing::info!(
            target: INPUT,
            dropped = plan.gate.dropped(),
            "joined console input resumed: every device is proved to reach this desktop alone again"
        );
    }
    if let Some(gap) = step.pen_withdrawn {
        tracing::warn!(target: INPUT, %gap, "the typed pen is withdrawn for the rest of the attachment");
        let _ = events.send(JoinedInputEvent::PenWithdrawn(gap)).await;
    }
    if let Some(gap) = step.end {
        tracing::warn!(
            target: INPUT,
            %gap,
            dropped = plan.gate.dropped(),
            "joined console input stayed unproved past the grace; the attachment ends"
        );
        let _ = events.send(JoinedInputEvent::End(deck_reason(&gap))).await;
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    fn console_metadata(
        display: &str,
        session_id: &str,
    ) -> crate::session::lifecycle::SessionMetadata {
        crate::session::lifecycle::SessionMetadata {
            username: "lab".into(),
            uid: joined_devices::session_uid(session_id).unwrap_or(u32::MAX),
            session_id: session_id.into(),
            session_type: "x11".into(),
            desktop: "gnome".into(),
            display: display.into(),
            agent_pid: 1,
            generation: 1,
            reconnected: false,
            timezone: None,
            multi_monitor_plan: None,
            multi_monitor_carrier: None,
            wayland: None,
            audio: None,
            requested_outputs: None,
            x11_desktop_depth: None,
            native_seat: None,
            console: true,
            console_guest: false,
            console_shown_outputs: None,
        }
    }

    #[cfg(target_os = "linux")]
    fn input_for(display: &str) -> PierInput {
        let identity =
            arcen_input::virtual_device::SessionInputIdentity::for_joined_console(display)
                .expect("display");
        let (controller, _) =
            crate::input::InputController::new(1920, 1080, None, identity).expect("uinput");
        PierInput::Uinput(controller)
    }

    /// Native: the attachment's own admission on a real kernel and a real
    /// console X server. Creates devices, injects nothing.
    ///
    /// - Devices named for `:88` (no desktop) judged as that desktop are
    ///   refused: no X server serves `:88`, and the console's X server reads
    ///   them too.
    /// - Devices named for the console's own display (`ARCEN_I1B_DISPLAY`,
    ///   its logind session in `ARCEN_I1B_SESSION`) are admitted, every one.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires /dev/uinput and a console X server; creates virtual devices without injecting input"]
    fn native_joined_console_input_is_admitted_only_when_confined() {
        let display = std::env::var("ARCEN_I1B_DISPLAY").expect("ARCEN_I1B_DISPLAY");
        let session = std::env::var("ARCEN_I1B_SESSION").expect("ARCEN_I1B_SESSION");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let mut foreign = input_for(":88");
        let refused = runtime.block_on(admit(&mut foreign, &console_metadata(":88", &session)));
        eprintln!("devices named for :88, judged as :88 -> {refused:?}");
        assert!(
            refused.is_err(),
            "a device no joined server owns is refused"
        );
        drop(foreign);
        let mut own = input_for(&display);
        std::thread::sleep(Duration::from_millis(1500));
        let joined = JoinedDesktop {
            display: display
                .trim_start_matches(':')
                .parse()
                .expect("display number"),
            session_id: session.clone(),
            uid: joined_devices::session_uid(&session).expect("the session's account"),
        };
        for role in VirtualInputRole::ALL {
            if let Some(Ok(nodes)) = own.device_nodes(role) {
                let facts = joined_devices::device_facts(&nodes, &joined);
                eprintln!(
                    "{display} {role:?}: readers {}; proof {:?}",
                    joined_devices::readers_summary(&facts),
                    arcen_input::joined_input::prove_joined_device_confinement(&facts)
                );
            }
        }
        let admitted = runtime
            .block_on(admit(&mut own, &console_metadata(&display, &session)))
            .expect("the console desktop's own devices are admitted");
        for role in VirtualInputRole::ALL {
            eprintln!(
                "{display} {role:?} admitted: {}",
                admitted.gate.admitted(role)
            );
        }
        assert!(admitted.gate.admitted(VirtualInputRole::Absolute));
        assert!(admitted.gate.admitted(VirtualInputRole::Relative));
        // The session's own terminal is in front, so injection is admitted
        // at this instant too.
        eprintln!(
            "session {session} terminal {:?}",
            joined_devices::session_vt(&session)
        );
        assert!(admitted.gate.admits(VirtualInputRole::Absolute));
    }

    /// A transient unit this test started, stopped when it ends.
    #[cfg(target_os = "linux")]
    struct TransientUnit(String);

    #[cfg(target_os = "linux")]
    impl TransientUnit {
        /// Runs `command` as unit `name` (system.slice, no session).
        fn start(name: String, command: &[&str]) -> Self {
            let status = std::process::Command::new("systemd-run")
                .args(["--quiet", "--collect", "--unit", &name])
                .args(command)
                .status()
                .expect("systemd-run");
            assert!(status.success(), "{name} did not start");
            Self(name)
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for TransientUnit {
        fn drop(&mut self) {
            let _ = std::process::Command::new("systemctl")
                .args(["stop", &self.0])
                .status();
        }
    }

    /// Who besides the joined desktop may read its devices, on a real
    /// kernel and a real console X server. Creates devices named for the
    /// console's display (`ARCEN_I1B_DISPLAY`, session `ARCEN_I1B_SESSION`),
    /// injects nothing, and holds their keyboard node open from:
    ///
    /// - a system service (a transient unit, root, no session): allowed and
    ///   reported, as `acpid` or `gpm` would be;
    /// - a process in another logind session (`runuser -l` of the throwaway
    ///   account `ARCEN_I1B_PROBE_USER` from a transient unit, which
    ///   `pam_systemd` gives a session of its own; it is granted read access
    ///   to that one node): refused, and the reason names it.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "lab only: root, /dev/uinput, a console X server and a throwaway account; injects nothing"]
    fn native_system_services_may_read_and_another_session_may_not() {
        let display = std::env::var("ARCEN_I1B_DISPLAY").expect("ARCEN_I1B_DISPLAY");
        let session = std::env::var("ARCEN_I1B_SESSION").expect("ARCEN_I1B_SESSION");
        let probe = std::env::var("ARCEN_I1B_PROBE_USER").expect("ARCEN_I1B_PROBE_USER");
        let joined = JoinedDesktop {
            display: display
                .trim_start_matches(':')
                .parse()
                .expect("display number"),
            session_id: session.clone(),
            uid: joined_devices::session_uid(&session).expect("the session's account"),
        };
        let mut own = input_for(&display);
        let nodes = own
            .device_nodes(VirtualInputRole::Absolute)
            .expect("device")
            .expect("nodes");
        let event = nodes
            .iter()
            .find(|node| node.name.starts_with("event"))
            .expect("event node")
            .name
            .clone();
        let node = format!("/dev/input/{event}");
        let prove = || {
            joined_devices::prove_devices(
                &[(VirtualInputRole::Absolute, nodes.clone())],
                &joined,
                Duration::from_secs(3),
            )
            .pop()
            .expect("one proof")
        };
        let baseline = prove();
        eprintln!(
            "{display} baseline: readers {}; proof {:?}",
            baseline.readers, baseline.proof
        );
        assert_eq!(baseline.proof, Ok(()), "the console's X server adds it");

        let hold = format!("import time; f = open('{node}', 'rb'); time.sleep(60)");
        let holds = |pid: u32| {
            std::fs::read_dir(format!("/proc/{pid}/fd")).is_ok_and(|fds| {
                fds.flatten().any(|fd| {
                    std::fs::read_link(fd.path())
                        .is_ok_and(|target| target == std::path::Path::new(&node))
                })
            })
        };
        let held = |unit: &str| {
            for _ in 0..50 {
                let held = std::process::Command::new("systemctl")
                    .args(["show", "-p", "MainPID", "--value", unit])
                    .output()
                    .ok()
                    .and_then(|out| String::from_utf8(out.stdout).ok())
                    .and_then(|pid| pid.trim().parse::<u32>().ok())
                    .filter(|pid| *pid != 0);
                if let Some(pid) = held {
                    let children = fs_children(pid);
                    if let Some(reader) =
                        std::iter::once(pid).chain(children).find(|pid| holds(*pid))
                    {
                        return reader;
                    }
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            panic!("{unit} never opened {node}");
        };

        // A system service.
        let unit = format!("arcen-i1b-sysreader-{}", std::process::id());
        let service = TransientUnit::start(unit.clone(), &["/usr/bin/python3", "-c", &hold]);
        let pid = held(&unit);
        let proved = prove();
        eprintln!(
            "system service reader pid {pid}: readers {}; reported {:?}; proof {:?}",
            proved.readers, proved.reported, proved.proof
        );
        assert_eq!(proved.proof, Ok(()), "a system service may read");
        assert!(
            proved.reported.contains("python3 (system service, uid 0)"),
            "{:?}",
            proved.reported
        );
        drop(service);

        // A reader in another logind session.
        let granted = std::process::Command::new("setfacl")
            .args(["-m", &format!("u:{probe}:r"), &node])
            .status()
            .expect("setfacl");
        assert!(granted.success());
        let unit = format!("arcen-i1b-otherreader-{}", std::process::id());
        let script = format!("exec /usr/bin/python3 -c \"{hold}\"");
        let other = TransientUnit::start(
            unit.clone(),
            &["/usr/sbin/runuser", "-l", &probe, "-c", &script],
        );
        let pid = held(&unit);
        let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap_or_default();
        let proved = prove();
        eprintln!(
            "other session reader pid {pid} (cgroup {}): readers {}; proof {:?}",
            cgroup.trim(),
            proved.readers,
            proved.proof
        );
        drop(other);
        let _ = std::process::Command::new("setfacl")
            .args(["-x", &format!("u:{probe}"), &node])
            .status();
        let Err(gap) = proved.proof else {
            panic!("a reader in another session was admitted");
        };
        assert!(
            matches!(
                &gap,
                JoinedDeviceGap::ForeignReader {
                    reader: arcen_input::joined_input::DeviceNodeReader::OtherSession { pid: held, .. },
                    ..
                } if *held == pid
            ),
            "{gap:?}"
        );
        let reason = deck_reason(&gap);
        eprintln!("deck reason: {reason}");
        assert!(reason.contains(&format!("python3 (pid {pid}) in host session")));
        drop(own);
    }

    /// Native: a reader is found by the device it holds, not by its path.
    /// Devices named for `:87` are put on a seat nobody runs (no X server
    /// reads them); a second node for the keyboard's event device is made
    /// with `mknod` in a scratch directory, and this test holds the device
    /// open only through it. The proof finds this process as the reader
    /// and refuses. The node and directory are removed. Needs root and
    /// `ARCEN_I1B_LAB_SEAT=1`; injects nothing.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "lab only: root, /dev/uinput, udev, mknod; devices on a seat nobody runs"]
    fn native_a_reader_through_another_node_is_found() {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::env::var("ARCEN_I1B_LAB_SEAT").as_deref(),
            Ok("1"),
            "lab only"
        );
        let _rule = crate::input::joined_devices::lab_seat::LabSeatRule::install(":87");
        let mut input = input_for(":87");
        let nodes = input
            .device_nodes(VirtualInputRole::Absolute)
            .expect("device")
            .expect("nodes");
        let event = nodes
            .iter()
            .find(|node| node.name.starts_with("event"))
            .expect("event node")
            .clone();
        std::thread::sleep(Duration::from_millis(1000));
        let lab = JoinedDesktop {
            display: 87,
            session_id: String::new(),
            uid: u32::MAX,
        };
        // A task-owned tmpfs that allows device nodes (/run does not),
        // unmounted and removed whatever happens.
        struct Scratch(std::path::PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::process::Command::new("umount").arg(&self.0).status();
                let _ = std::fs::remove_dir(&self.0);
            }
        }
        let scratch = Scratch(std::path::PathBuf::from(format!(
            "/run/arcen-i1b-scratch-{}",
            std::process::id()
        )));
        std::fs::create_dir_all(&scratch.0).expect("scratch directory");
        let mounted = std::process::Command::new("mount")
            .args([
                "-t",
                "tmpfs",
                "-o",
                "mode=0700,size=64k",
                "arcen-i1b-scratch",
            ])
            .arg(&scratch.0)
            .status()
            .expect("mount");
        assert!(mounted.success());
        let scratch_dir = scratch.0.clone();
        let alias = scratch_dir.join("alias");
        let (major, minor) = event.device;
        let made = std::process::Command::new("mknod")
            .args([
                alias.to_str().expect("path"),
                "c",
                &major.to_string(),
                &minor.to_string(),
            ])
            .status()
            .expect("mknod");
        assert!(made.success());
        let held = std::fs::File::open(&alias).expect("open the second node");
        assert_eq!(
            std::fs::metadata(&alias).expect("stat").rdev(),
            std::fs::metadata(format!("/dev/input/{}", event.name))
                .expect("stat")
                .rdev()
        );
        let facts = joined_devices::device_facts(&nodes, &lab);
        let proof = arcen_input::joined_input::prove_joined_device_confinement(&facts);
        eprintln!(
            "held through {}: readers {}; proof {:?}",
            alias.display(),
            joined_devices::readers_summary(&facts),
            proof
        );
        drop(held);
        drop(scratch);
        let me = std::process::id();
        let event_readers = &facts
            .nodes
            .iter()
            .find(|node| node.name == event.name)
            .expect("event node facts")
            .readers;
        assert!(
            event_readers.iter().any(|reader| matches!(
                reader,
                arcen_input::joined_input::DeviceNodeReader::OtherSession { pid, .. }
                    | arcen_input::joined_input::DeviceNodeReader::OtherAccount { pid, .. }
                    | arcen_input::joined_input::DeviceNodeReader::Unattributable { pid, .. }
                    if *pid == me
            )),
            "the reader through another node was not found: {event_readers:?}"
        );
        assert!(matches!(proof, Err(JoinedDeviceGap::ForeignReader { .. })));
        assert!(!scratch_dir.exists());
        drop(input);
    }

    /// Native: an account that cannot read the joined desktop's keyboard
    /// node swaps an `O_PATH` handle on it (search permission is enough)
    /// with a readable open of `/dev/null` (the same devtmpfs mount) on one
    /// descriptor, as fast as it can, from a session of its own
    /// (`ARCEN_I1B_PROBE_USER`, as in the reader test). The session stays
    /// admitted through every proof: the swapped descriptor is never a
    /// reader. Devices named for the console's display; injects nothing.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "lab only: root, /dev/uinput, a console X server and a throwaway account; injects nothing"]
    fn native_an_o_path_swapper_does_not_refuse_the_session() {
        let display = std::env::var("ARCEN_I1B_DISPLAY").expect("ARCEN_I1B_DISPLAY");
        let session = std::env::var("ARCEN_I1B_SESSION").expect("ARCEN_I1B_SESSION");
        let probe = std::env::var("ARCEN_I1B_PROBE_USER").expect("ARCEN_I1B_PROBE_USER");
        let joined = JoinedDesktop {
            display: display
                .trim_start_matches(':')
                .parse()
                .expect("display number"),
            session_id: session.clone(),
            uid: joined_devices::session_uid(&session).expect("the session's account"),
        };
        let mut own = input_for(&display);
        let nodes = own
            .device_nodes(VirtualInputRole::Absolute)
            .expect("device")
            .expect("nodes");
        let event = nodes
            .iter()
            .find(|node| node.name.starts_with("event"))
            .expect("event node")
            .name
            .clone();
        let node = format!("/dev/input/{event}");
        let prove = |settle| {
            joined_devices::prove_devices(
                &[(VirtualInputRole::Absolute, nodes.clone())],
                &joined,
                settle,
            )
            .pop()
            .expect("one proof")
        };
        assert_eq!(
            prove(Duration::from_secs(3)).proof,
            Ok(()),
            "the console's X server adds it"
        );
        let swap = format!(
            "import os; a = os.open('/dev/null', os.O_RDONLY); p = os.open('{node}', os.O_PATH); n = os.dup(a)\nwhile True:\n    os.dup2(p, n); os.dup2(a, n)"
        );
        let script = format!("exec /usr/bin/python3 -c \"{swap}\"");
        let unit = format!("arcen-i1b-swapper-{}", std::process::id());
        let swapper = TransientUnit::start(
            unit.clone(),
            &["/usr/sbin/runuser", "-l", &probe, "-c", &script],
        );
        // Wait until it holds the node (an O_PATH handle shows in fd/).
        let mut swapping = None;
        for _ in 0..50 {
            let main = std::process::Command::new("systemctl")
                .args(["show", "-p", "MainPID", "--value", &unit])
                .output()
                .ok()
                .and_then(|out| String::from_utf8(out.stdout).ok())
                .and_then(|pid| pid.trim().parse::<u32>().ok())
                .filter(|pid| *pid != 0);
            swapping = main.and_then(|pid| {
                std::iter::once(pid).chain(fs_children(pid)).find(|pid| {
                    std::fs::read_dir(format!("/proc/{pid}/fd")).is_ok_and(|fds| {
                        fds.flatten().any(|fd| {
                            std::fs::read_link(fd.path())
                                .is_ok_and(|target| target == std::path::Path::new(&node))
                        })
                    })
                })
            });
            if swapping.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let pid = swapping.expect("the swapper holds an O_PATH handle on the node");
        let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap_or_default();
        let mut refused = Vec::new();
        let rounds = 40;
        for _ in 0..rounds {
            let proved = prove(Duration::ZERO);
            if proved.proof.is_err() {
                refused.push(format!("{:?} ({})", proved.proof, proved.readers));
            }
        }
        let still = std::path::Path::new(&format!("/proc/{pid}")).exists();
        drop(swapper);
        eprintln!(
            "O_PATH swapper pid {pid} ({}), alive {still}: {} of {rounds} proofs refused {refused:?}",
            cgroup.trim(),
            refused.len()
        );
        assert!(still, "the swapper kept running through the proofs");
        assert!(refused.is_empty(), "{refused:?}");
        drop(own);
    }

    /// The descendants of `pid` (from `/proc/<pid>/task/*/children`).
    #[cfg(target_os = "linux")]
    fn fs_children(pid: u32) -> Vec<u32> {
        let mut found = Vec::new();
        let mut queue = vec![pid];
        while let Some(parent) = queue.pop() {
            let Ok(tasks) = std::fs::read_dir(format!("/proc/{parent}/task")) else {
                continue;
            };
            for task in tasks.flatten() {
                let children =
                    std::fs::read_to_string(task.path().join("children")).unwrap_or_default();
                for child in children
                    .split_whitespace()
                    .filter_map(|pid| pid.parse().ok())
                {
                    found.push(child);
                    queue.push(child);
                }
            }
        }
        found
    }

    /// KEY events read from `reader` now, as (code, value).
    #[cfg(target_os = "linux")]
    fn read_keys(reader: &mut evdev::Device) -> Vec<(u16, i32)> {
        std::thread::sleep(Duration::from_millis(50));
        reader
            .fetch_events()
            .map(|events| {
                events
                    .filter(|event| event.event_type() == evdev::EventType::KEY)
                    .map(|event| (event.code(), event.value()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Native: the gate every injection passes. Devices named for `:87` are
    /// put on a seat nobody runs first (a volatile udev rule), so no X
    /// server reads them whatever happens: an open gate writes (read back
    /// from the event node), a shut one refuses without writing. Needs root
    /// and `ARCEN_I1B_LAB_SEAT=1`.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "lab only: root, /dev/uinput, udev; devices on a seat nobody runs"]
    fn native_a_shut_gate_writes_nothing() {
        use evdev::{AbsoluteAxisCode, EventType};
        assert_eq!(
            std::env::var("ARCEN_I1B_LAB_SEAT").as_deref(),
            Ok("1"),
            "lab only"
        );
        let rule = crate::input::joined_devices::lab_seat::LabSeatRule::install(":87");
        let mut input = input_for(":87");
        let nodes = input
            .device_nodes(VirtualInputRole::Absolute)
            .expect("device")
            .expect("nodes");
        let event = nodes
            .iter()
            .find(|node| node.name.starts_with("event"))
            .expect("event node");
        std::thread::sleep(Duration::from_millis(1000));
        let lab = JoinedDesktop {
            display: 87,
            session_id: String::new(),
            uid: u32::MAX,
        };
        let facts = joined_devices::device_facts(&nodes, &lab);
        eprintln!(
            "isolated readers: {}",
            joined_devices::readers_summary(&facts)
        );
        // No display server and no other process reads it; logind, which
        // watches keyboards for their power keys, may.
        assert!(
            facts
                .nodes
                .iter()
                .all(|node| node.readers.iter().all(|reader| reader.admitted())),
            "something besides the input broker reads a device on a seat nobody runs"
        );
        let mut reader =
            evdev::Device::open(format!("/dev/input/{}", event.name)).expect("open node");
        reader.set_nonblocking(true).expect("nonblocking");
        let gate = Arc::new(JoinedInputGate::default());
        input.confine(&gate);
        let mut read_x = || -> Vec<i32> {
            std::thread::sleep(Duration::from_millis(50));
            reader
                .fetch_events()
                .map(|events| {
                    events
                        .filter(|event| {
                            event.event_type() == EventType::ABSOLUTE
                                && event.code() == AbsoluteAxisCode::ABS_X.0
                        })
                        .map(|event| event.value())
                        .collect()
                })
                .unwrap_or_default()
        };
        let to = |x: f64| arcen_protocol::messages::MouseMoveMsg {
            x,
            y: 0.5,
            ..arcen_protocol::messages::MouseMoveMsg::default()
        };
        // Shut: refused, nothing written.
        let refused = input.mouse_move(&to(0.25));
        eprintln!("shut gate -> {refused:?}");
        assert!(matches!(
            refused,
            Err(crate::input::InputError::NotConfined)
        ));
        assert!(read_x().is_empty(), "nothing reached the device");
        assert_eq!(gate.dropped(), 1);
        // Open: written.
        gate.set(VirtualInputRole::Absolute, true);
        input
            .mouse_move(&to(0.75))
            .expect("written through an open gate");
        let written = read_x();
        eprintln!("open gate -> ABS_X {written:?}");
        assert!(!written.is_empty(), "the move reached the device");
        // Shut again.
        gate.set(VirtualInputRole::Absolute, false);
        assert!(input.mouse_move(&to(0.25)).is_err());
        assert!(read_x().is_empty());
        let _ = read_keys(&mut reader);
        // A key held when the gate shuts and let go while it is shut (the
        // release is refused) is released once input resumes.
        let key = |pressed| arcen_protocol::messages::KeyEventMsg {
            scan_code: 0x41,
            pressed,
            ..arcen_protocol::messages::KeyEventMsg::default()
        };
        gate.set(VirtualInputRole::Absolute, true);
        input
            .key_event(&key(true))
            .expect("pressed through an open gate");
        assert!(read_keys(&mut reader).contains(&(30, 1)), "KEY_A down");
        gate.set(VirtualInputRole::Absolute, false);
        assert!(matches!(
            input.key_event(&key(false)),
            Err(crate::input::InputError::NotConfined)
        ));
        assert!(
            read_keys(&mut reader).is_empty(),
            "the release was not written"
        );
        gate.set(VirtualInputRole::Absolute, true);
        input.reset_held().expect("released on recovery");
        let released = read_keys(&mut reader);
        eprintln!("released on recovery: {released:?}");
        assert!(released.contains(&(30, 0)), "KEY_A up on recovery");
        drop(input);
        drop(rule);
    }
}
