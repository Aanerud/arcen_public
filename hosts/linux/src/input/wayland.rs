//! Session-bound Wayland input injector of the Linux Pier.
//!
//! A thin adapter. Deck input arrives on the input lane as protocol
//! messages; the [`WaylandInputPlanner`] turns them into the session's
//! [`InputCommand`]s (evdev keycodes, stream-relative pointer pixels stamped
//! with the session's output generation); the [`InjectionQueue`] keeps them
//! in order; and one task per attachment writes them to the session's
//! `arcen-input.sock`, where the session agent validates each one again and
//! injects it through the session's own Mutter `RemoteDesktop` session.
//!
//! The Pier (root) connects only after proving the socket is the session's:
//! it lies in the session's private runtime directory, both owned by the
//! session user and closed to everyone else, and the peer on the other end
//! is the session user's agent process itself. The session's `hello` reply
//! must name the same output generation and streams the agent reported at
//! start. Inside a Wayland session there is no uinput or Xorg fallback: if
//! this injector cannot be proven, the attachment is refused rather than
//! injecting into the physical seat or another session.
//!
//! Ordering: every command for one attachment goes through one queue and
//! one connection, so injection order is arrival order. Motion directly
//! behind motion of the same kind is coalesced (an absolute position
//! replaces the previous one, relative deltas and continuous scroll sum);
//! edges are never dropped, merged or reordered. A queue that fills up —
//! the compositor stopped answering — collapses into one `release_all`, so
//! a key or button release is never silently lost.
//!
//! Pen goes only to an emulated stylus on the compositor's EIS seat (libei
//! `ei_stylus`), which the session probes at start and reports in its
//! `hello` ([`crate::session::wayland::StylusFacts`]); Mutter `RemoteDesktop`
//! has no tablet device type. Without a proven stylus the hello advertises
//! pen unavailable with the session's reason, and typed pen is refused,
//! exactly as a Windows host without a synthetic pen device does. The Deck's
//! mouse compatibility mode stays available.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arcen_input::pen_emulation::{PenCapabilityTruth, StylusUnavailable};
use arcen_protocol::messages::{
    validate_commit_text, HostKeyCharsMsg, HostKeymapMsg, KeyEventMsg, MouseButtonMsg,
    MouseMoveMsg, MouseMoveRelativeMsg, MouseScrollMsg, PenEventMsg, PenToolMsg, PointerMotionMode,
    TextCommitMsg, TextCommitResultMsg, TextRefusalReason, MAX_HOST_KEYMAP_KEYS,
};
use arcen_session::desktop_provider::{CapabilityFact, WaylandCapabilityFacts};
use arcen_session::wayland_session::{device_type, MutterInputCapabilities, RemoteDesktopEvidence};
use thiserror::Error;

use super::edges::{
    commit_button_edge, legacy_scroll_to_wheel_units, mouse_button_code, plan_button_edge,
    plan_key_edges, plan_modifier_edges, split_legacy_detents, WHEEL_UNITS_PER_DETENT,
};
use super::keymap::{qt_key_to_evdev, LOCK_CODES};
use super::{InputError, InputStats};
use crate::logging::target;
use crate::session::wayland::text::{SessionKeymap, TextOutcome};
use crate::session::wayland::{
    is_private_runtime_dir, AxisSource, InputBackendKind, InputCommand, InputReply, InputRequest,
    InputSessionFacts, ReadyStream, WaylandAgentReady, SUPERSEDED_REASON,
};

/// Commands waiting for the session; beyond this the queue collapses into
/// one `release_all`.
pub const INJECTION_QUEUE_CAPACITY: usize = 512;
/// Connect, verify and `hello` must finish within this.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// One command's reply must arrive within this, or the injector is lost.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// What one typed character adds to a `text` command's budget: a keysym is
/// a press and a release, each a compositor round trip.
pub const TEXT_BUDGET_PER_CHAR: Duration = Duration::from_millis(50);

/// How long the session may take with `command`: committed text gets time
/// for every character it types, on top of the fixed budget.
#[must_use]
pub fn command_budget(command: &InputCommand) -> Duration {
    match command {
        InputCommand::Text { text, .. } => {
            let chars = u32::try_from(text.chars().count()).unwrap_or(u32::MAX);
            COMMAND_TIMEOUT.saturating_add(TEXT_BUDGET_PER_CHAR.saturating_mul(chars))
        }
        _ => COMMAND_TIMEOUT,
    }
}
/// After the attachment ends, queued commands (ending with the release of
/// everything held) are still delivered for at most this long.
pub const DRAIN_BUDGET: Duration = Duration::from_secs(2);
/// Waits before each reconnection attempt after a connection broke; each
/// attempt is bounded by [`CONNECT_TIMEOUT`].
pub const RECONNECT_BACKOFF: [Duration; 3] = [
    Duration::from_millis(250),
    Duration::from_secs(1),
    Duration::from_secs(2),
];
/// How long a failed write waits for a superseded notice still buffered.
const NOTICE_READ_TIMEOUT: Duration = Duration::from_millis(200);
/// Mutter smooth-scroll units per wheel detent.
const MUTTER_SCROLL_UNITS_PER_DETENT: f64 = 10.0;
/// Most committed-text parts ([`arcen_input::text::MAX_TEXT_PART_CHARS`]
/// characters each) accepted per second per attachment.
pub const MAX_TEXT_PARTS_PER_SECOND: u32 = 64;
/// Text and layout notices waiting for the Deck's input lane.
const TEXT_NOTICE_CAPACITY: usize = 32;

/// The Qt key positions a Deck sends for printable keys, the positions a
/// [`HostKeymapMsg`] describes.
const PRINTABLE_QT_KEYS: &[u32] = &[
    0x20, 0x27, 0x2C, 0x2D, 0x2E, 0x2F, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39,
    0x3B, 0x3D, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x4B, 0x4C, 0x4D, 0x4E,
    0x4F, 0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x5B, 0x5C, 0x5D, 0x60,
];

/// The notice a Deck is told about one commit, when something was refused
/// or typed without proof.
#[must_use]
pub fn text_result_notice(sequence: u64, outcome: &TextOutcome) -> Option<TextCommitResultMsg> {
    let delivered = outcome
        .keymap_chars
        .saturating_add(outcome.unverified_chars)
        .saturating_add(outcome.utf8_chars);
    if outcome.refused_chars == 0 && outcome.unverified_chars == 0 {
        return None;
    }
    Some(TextCommitResultMsg {
        sequence,
        delivered_chars: delivered,
        refused_chars: outcome.refused_chars,
        refused_text: outcome.refused_text.clone(),
        reason: outcome.reason.unwrap_or(TextRefusalReason::KeymapUnknown),
        unverified: outcome.unverified_chars > 0,
        ..TextCommitResultMsg::default()
    })
}

/// A commit refused before it reached the session.
#[must_use]
pub fn text_refusal_notice(
    message: &TextCommitMsg,
    reason: TextRefusalReason,
) -> TextCommitResultMsg {
    let refused_text: String = message
        .text
        .chars()
        .take(arcen_protocol::messages::MAX_REFUSED_TEXT_CHARS)
        .collect();
    TextCommitResultMsg {
        sequence: message.sequence,
        refused_chars: u32::try_from(message.text.chars().count()).unwrap_or(u32::MAX),
        refused_text,
        reason,
        ..TextCommitResultMsg::default()
    }
}

/// The Deck's view of the session's layout: characters per Qt key position.
#[must_use]
pub fn host_keymap_notice(keymap: &SessionKeymap) -> HostKeymapMsg {
    let keys = PRINTABLE_QT_KEYS
        .iter()
        .filter_map(|&qt_key| {
            let evdev = u32::from(qt_key_to_evdev(qt_key, 0)?);
            let position = keymap.keys.iter().find(|key| key.evdev == evdev)?;
            Some(HostKeyCharsMsg {
                qt_key,
                base: position.base.clone(),
                shift: position.shift.clone(),
            })
        })
        .take(MAX_HOST_KEYMAP_KEYS)
        .collect();
    HostKeymapMsg {
        generation: keymap.generation,
        source: "compositor_keymap".to_owned(),
        layouts: keymap.layouts.clone(),
        active_layout: keymap.active_layout.clone(),
        keys,
        ..HostKeymapMsg::default()
    }
}

fn send_notice(notices: &tokio::sync::mpsc::Sender<String>, notice: &impl serde::Serialize) {
    if let Ok(json) = serde_json::to_string(notice) {
        if notices.try_send(json).is_err() {
            tracing::debug!(target: target::INPUT, "text notice dropped: the Deck's input lane is not keeping up");
        }
    }
}

/// Why the session's input socket could not be used.
#[derive(Debug, Error)]
pub enum WaylandInputError {
    #[error("the Wayland session's ready message is inconsistent")]
    Inconsistent,
    #[error("the input socket is not inside the session's private runtime directory")]
    RuntimeDir,
    #[error("the input socket or its directory is not private to the session user")]
    NotPrivate,
    #[error("the input socket's peer is not the session's agent (uid {uid:?}, pid {pid:?})")]
    ForeignPeer { uid: Option<u32>, pid: Option<i32> },
    #[error("the session refused hello: {0}")]
    HelloRefused(String),
    #[error("a newer input connection took the session's input")]
    Superseded,
    #[error("the session's input facts differ from the session it started: {0}")]
    FactsMismatch(&'static str),
    #[error("the session did not answer in time")]
    Timeout,
    #[error("the session sent a malformed reply")]
    MalformedReply,
    #[error("the session closed the input connection")]
    Closed,
    #[error("input socket I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

/// The session an injector may connect to: what the session agent reported
/// when the desktop started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaylandInputTarget {
    socket: PathBuf,
    runtime_dir: PathBuf,
    uid: u32,
    agent_pid: Option<u32>,
    generation: u64,
    streams: Vec<ReadyStream>,
}

impl WaylandInputTarget {
    /// The target of one started session.
    ///
    /// # Errors
    ///
    /// [`WaylandInputError::Inconsistent`] when the ready message does not
    /// hold together, [`WaylandInputError::RuntimeDir`] when its socket is
    /// not in this session's private runtime directory.
    pub fn from_session(
        ready: &WaylandAgentReady,
        uid: u32,
        session_id: &str,
        agent_pid: u32,
    ) -> Result<Self, WaylandInputError> {
        if !ready.is_consistent() {
            return Err(WaylandInputError::Inconsistent);
        }
        if !is_private_runtime_dir(&ready.runtime_dir, uid, session_id) {
            return Err(WaylandInputError::RuntimeDir);
        }
        Ok(Self {
            socket: ready.input_socket.clone(),
            runtime_dir: ready.runtime_dir.clone(),
            uid,
            agent_pid: Some(agent_pid),
            generation: ready.generation,
            streams: ready.streams.clone(),
        })
    }

    /// A target outside `/run/user`, for tests that serve a socket from a
    /// scratch directory. `agent_pid` `None` skips the peer pid check.
    #[cfg(test)]
    pub(crate) fn for_tests(
        socket: PathBuf,
        uid: u32,
        agent_pid: Option<u32>,
        generation: u64,
        streams: Vec<ReadyStream>,
    ) -> Self {
        Self {
            runtime_dir: socket.parent().map(Path::to_path_buf).unwrap_or_default(),
            socket,
            uid,
            agent_pid,
            generation,
            streams,
        }
    }

    /// Output generation every coordinate carries.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// The same session after an output reconfiguration: `successor` must
    /// name the same socket, runtime directory, user and agent, and a newer
    /// generation.
    #[must_use]
    pub fn accepts_successor(&self, successor: &Self) -> bool {
        self.socket == successor.socket
            && self.runtime_dir == successor.runtime_dir
            && self.uid == successor.uid
            && self.agent_pid == successor.agent_pid
            && successor.generation > self.generation
            && !successor.streams.is_empty()
    }
}

/// The socket and its directory belong to the session user alone, and the
/// socket is directly inside the directory. A path check only: the peer
/// check after connecting is the authority.
fn verify_socket(target: &WaylandInputTarget) -> Result<(), WaylandInputError> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    if target.socket.parent() != Some(target.runtime_dir.as_path()) {
        return Err(WaylandInputError::RuntimeDir);
    }
    let private = |path: &Path, is_kind: fn(&std::fs::Metadata) -> bool| {
        std::fs::symlink_metadata(path).is_ok_and(|metadata| {
            is_kind(&metadata) && metadata.uid() == target.uid && metadata.mode() & 0o077 == 0
        })
    };
    if private(&target.runtime_dir, std::fs::Metadata::is_dir)
        && private(&target.socket, |metadata| metadata.file_type().is_socket())
    {
        Ok(())
    } else {
        Err(WaylandInputError::NotPrivate)
    }
}

fn verify_peer(
    stream: &tokio::net::UnixStream,
    target: &WaylandInputTarget,
) -> Result<(), WaylandInputError> {
    let credentials = stream.peer_cred()?;
    let uid = credentials.uid();
    let pid = credentials.pid();
    let pid_matches = target
        .agent_pid
        .is_none_or(|agent| pid.and_then(|pid| u32::try_from(pid).ok()) == Some(agent));
    if uid == target.uid && pid_matches {
        Ok(())
    } else {
        Err(WaylandInputError::ForeignPeer {
            uid: Some(uid),
            pid,
        })
    }
}

/// What a connected session proved about its input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaylandInputProof {
    facts: InputSessionFacts,
}

impl WaylandInputProof {
    /// Checks the session's `hello` facts against the session it started.
    ///
    /// # Errors
    ///
    /// [`WaylandInputError::FactsMismatch`] naming the first difference.
    pub fn verify(
        target: &WaylandInputTarget,
        facts: InputSessionFacts,
    ) -> Result<Self, WaylandInputError> {
        let required = device_type::KEYBOARD | device_type::POINTER;
        if facts.generation != target.generation {
            return Err(WaylandInputError::FactsMismatch("output generation"));
        }
        if facts.streams != target.streams {
            return Err(WaylandInputError::FactsMismatch("streams"));
        }
        if facts.device_types & required != required {
            return Err(WaylandInputError::FactsMismatch("device types"));
        }
        if !facts.remote_desktop_started {
            return Err(WaylandInputError::FactsMismatch(
                "RemoteDesktop not started",
            ));
        }
        if facts.backend != InputBackendKind::RemoteDesktopDbus {
            return Err(WaylandInputError::FactsMismatch("backend"));
        }
        Ok(Self { facts })
    }

    /// The session's input facts.
    #[must_use]
    pub const fn facts(&self) -> &InputSessionFacts {
        &self.facts
    }

    /// What this session can inject. Pen and EIS are never claimed by the
    /// `RemoteDesktop` D-Bus path.
    #[must_use]
    pub const fn capabilities(&self) -> MutterInputCapabilities {
        RemoteDesktopEvidence {
            device_types: self.facts.device_types,
            started: self.facts.remote_desktop_started,
            linked_to_screencast: true,
            absolute_pointer_accepted: true,
        }
        .capabilities()
    }

    /// What the session's pen delivers, axis by axis: only what its
    /// emulated stylus bound.
    #[must_use]
    pub fn pen_capabilities(&self) -> PenCapabilityTruth {
        self.facts
            .stylus
            .as_ref()
            .and_then(|stylus| stylus.binding)
            .map_or(
                PenCapabilityTruth::UNAVAILABLE,
                PenCapabilityTruth::emulated_stylus,
            )
    }

    /// Why the session has no pen; `None` when it has one.
    #[must_use]
    pub fn pen_unavailable(&self) -> Option<StylusUnavailable> {
        match &self.facts.stylus {
            Some(stylus) if stylus.binding.is_some() => None,
            Some(stylus) => Some(
                stylus
                    .unavailable
                    .unwrap_or(StylusUnavailable::StylusNotOffered),
            ),
            None if self.facts.eis_offered == Some(false) => {
                Some(StylusUnavailable::NoEmulatedInput)
            }
            None => Some(StylusUnavailable::NotProbed),
        }
    }

    /// Host facts upgraded with this proof: session-bound input injection
    /// is available once keyboard and pointer are. Never hides another
    /// fact.
    #[must_use]
    pub const fn proven_facts(&self, host: WaylandCapabilityFacts) -> WaylandCapabilityFacts {
        let capabilities = self.capabilities();
        let mut facts = host;
        if matches!(capabilities.keyboard, CapabilityFact::Available)
            && matches!(capabilities.pointer, CapabilityFact::Available)
        {
            facts.input_injection = CapabilityFact::Available;
        }
        facts
    }
}

/// What a key event became.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyPlan {
    /// Commands to send, in order.
    pub commands: Vec<InputCommand>,
    /// The key has neither an evdev code nor a Latin-1 keysym.
    pub unmapped: bool,
}

/// Deck input messages to session input commands, with the held-state
/// mirror that keyboard resets and idempotent edges need. Pure: it performs
/// no I/O.
#[derive(Debug, Clone)]
pub struct WaylandInputPlanner {
    output: ReadyStream,
    /// Every stream of the session, for input that addresses the whole
    /// desktop (several outputs).
    outputs: Vec<ReadyStream>,
    generation: u64,
    held_keys: HashSet<u16>,
    held_keysyms: HashMap<u32, u32>,
    held_buttons: HashSet<u16>,
    /// The lock states last sent to the session; `None` when unknown (never
    /// sent, or a queue collapse or reconnection may have dropped it).
    lock_state: [Option<bool>; 3],
    vertical_remainder: i64,
    horizontal_remainder: i64,
}

impl WaylandInputPlanner {
    /// Follows an output reconfiguration: positions address the new
    /// primary stream and generation; held keys and buttons are kept (the
    /// session released them before it reconfigured, and the Deck releases
    /// its own).
    pub fn retarget(&mut self, target: &WaylandInputTarget) {
        if let Some(output) = target.streams.first() {
            self.output = output.clone();
            self.outputs.clone_from(&target.streams);
            self.generation = target.generation;
        }
    }

    /// A planner addressing the session's primary (first) stream.
    #[must_use]
    pub fn new(target: &WaylandInputTarget) -> Option<Self> {
        Some(Self {
            output: target.streams.first()?.clone(),
            outputs: target.streams.clone(),
            generation: target.generation,
            held_keys: HashSet::new(),
            held_keysyms: HashMap::new(),
            held_buttons: HashSet::new(),
            lock_state: [None; 3],
            vertical_remainder: 0,
            horizontal_remainder: 0,
        })
    }

    /// One Deck key event: lock states reconciled, then the modifier mask,
    /// then the key — by evdev keycode, or by keysym for a Latin-1
    /// character the keycode table lacks.
    ///
    /// Locks are reconciled by the session, which tracks the lock states
    /// its compositor actually has: the planner sends the Deck's states
    /// whenever they differ from what it last sent, or that is unknown.
    pub fn key_event(&mut self, message: &KeyEventMsg) -> KeyPlan {
        let mut commands = Vec::new();
        let requested = [
            message.caps_lock_on,
            message.num_lock_on,
            message.scroll_lock_on,
        ];
        let code = qt_key_to_evdev(message.scan_code, message.modifiers);
        let mut locks = [None; 3];
        for (index, wanted) in requested.into_iter().enumerate() {
            if code == Some(LOCK_CODES[index]) {
                // The lock key itself toggles this lock: reconciling it
                // first would toggle twice. Its state is unknown until the
                // next key event reconciles it.
                self.lock_state[index] = None;
                continue;
            }
            if wanted.is_some() && wanted != self.lock_state[index] {
                locks[index] = wanted;
                self.lock_state[index] = wanted;
            }
        }
        if locks.iter().any(Option::is_some) {
            commands.push(InputCommand::Locks {
                caps_lock: locks[0],
                num_lock: locks[1],
                scroll_lock: locks[2],
            });
        }
        if let Some(code) = code {
            commands.extend(
                plan_key_edges(
                    &mut self.held_keys,
                    code,
                    message.pressed,
                    message.modifiers,
                )
                .into_iter()
                .map(|(code, pressed)| key(code, pressed)),
            );
            return KeyPlan {
                commands,
                unmapped: false,
            };
        }
        let Some(keysym) = latin1_keysym(message.scan_code) else {
            return KeyPlan {
                commands,
                unmapped: true,
            };
        };
        commands.extend(
            plan_modifier_edges(&mut self.held_keys, message.modifiers)
                .into_iter()
                .map(|(code, pressed)| key(code, pressed)),
        );
        let held = self.held_keysyms.contains_key(&message.scan_code);
        if message.pressed && !held {
            self.held_keysyms.insert(message.scan_code, keysym);
            commands.push(InputCommand::Keysym {
                keysym,
                pressed: true,
            });
        } else if !message.pressed {
            if let Some(keysym) = self.held_keysyms.remove(&message.scan_code) {
                commands.push(InputCommand::Keysym {
                    keysym,
                    pressed: false,
                });
            }
        }
        KeyPlan {
            commands,
            unmapped: false,
        }
    }

    /// Releases every held key and keysym, never a button.
    pub fn reset_keyboard(&mut self) -> Vec<InputCommand> {
        let mut codes = self.held_keys.drain().collect::<Vec<_>>();
        codes.sort_unstable();
        let mut keysyms = self
            .held_keysyms
            .drain()
            .map(|(_, keysym)| keysym)
            .collect::<Vec<_>>();
        keysyms.sort_unstable();
        codes
            .into_iter()
            .map(|code| key(code, false))
            .chain(keysyms.into_iter().map(|keysym| InputCommand::Keysym {
                keysym,
                pressed: false,
            }))
            .collect()
    }

    /// Releases everything this connection holds in the session, and
    /// forgets it here.
    pub fn reset_all(&mut self) -> InputCommand {
        self.forget_held();
        InputCommand::ReleaseAll
    }

    /// Forgets the held-state mirror after the session released everything,
    /// and the lock states sent, which may not have arrived: the next key
    /// event reconciles them again.
    pub fn forget_held(&mut self) {
        self.held_keys.clear();
        self.held_keysyms.clear();
        self.held_buttons.clear();
        self.lock_state = [None; 3];
        self.vertical_remainder = 0;
        self.horizontal_remainder = 0;
    }

    fn output_rects(&self) -> Vec<arcen_input::OutputRect> {
        self.outputs
            .iter()
            .map(|stream| arcen_input::OutputRect {
                x: i64::from(stream.x),
                y: i64::from(stream.y),
                width: stream.width,
                height: stream.height,
            })
            .collect()
    }

    /// An absolute position on the output that contains desktop pixel
    /// `(x, y)`, in that output's pixels and the current generation.
    ///
    /// # Errors
    ///
    /// [`InputError::InvalidCoordinate`] for a point on no output (a gap
    /// between outputs, or outside the desktop).
    pub fn desktop_position(&self, x: i32, y: i32) -> Result<InputCommand, InputError> {
        let point = arcen_input::locate_output(
            arcen_media::AppliedPoint::new(i64::from(x), i64::from(y)),
            &self.output_rects(),
        )
        .ok_or(InputError::InvalidCoordinate)?;
        let output = &self.outputs[point.output];
        Ok(InputCommand::PointerAbsolute {
            output: output.connector.clone(),
            generation: self.generation,
            x: f64::from(point.x),
            y: f64::from(point.y),
        })
    }

    /// A normalised (`0.0..=1.0`) position: on the primary output, or with
    /// several outputs across the whole desktop's bounding box.
    fn position(&self, x: f64, y: f64) -> Result<InputCommand, InputError> {
        if !x.is_finite() || !y.is_finite() {
            return Err(InputError::InvalidCoordinate);
        }
        if self.outputs.len() > 1 {
            let rects = self.output_rects();
            let width = rects
                .iter()
                .map(|rect| rect.x + i64::from(rect.width))
                .max()
                .unwrap_or(0);
            let height = rects
                .iter()
                .map(|rect| rect.y + i64::from(rect.height))
                .max()
                .unwrap_or(0);
            let point = arcen_input::locate_normalized(
                x,
                y,
                u32::try_from(width).unwrap_or(0),
                u32::try_from(height).unwrap_or(0),
                &rects,
            )
            .ok_or(InputError::InvalidCoordinate)?;
            let output = &self.outputs[point.output];
            return Ok(InputCommand::PointerAbsolute {
                output: output.connector.clone(),
                generation: self.generation,
                x: f64::from(point.x),
                y: f64::from(point.y),
            });
        }
        let scale =
            |value: f64, extent: u32| value.clamp(0.0, 1.0) * f64::from(extent.saturating_sub(1));
        Ok(InputCommand::PointerAbsolute {
            output: self.output.connector.clone(),
            generation: self.generation,
            x: scale(x, self.output.width),
            y: scale(y, self.output.height),
        })
    }

    /// Absolute motion.
    ///
    /// # Errors
    ///
    /// [`InputError::InvalidCoordinate`] for a non-finite coordinate.
    pub fn mouse_move(&self, message: &MouseMoveMsg) -> Result<InputCommand, InputError> {
        self.position(message.x, message.y)
    }

    /// One validated pen sample on the primary output, for the session's
    /// emulated stylus. The session plans the stylus frames; this only
    /// places the sample.
    ///
    /// # Errors
    ///
    /// [`InputError::InvalidCoordinate`] for a non-finite coordinate.
    pub fn pen(&self, message: &PenEventMsg) -> Result<InputCommand, InputError> {
        let InputCommand::PointerAbsolute {
            output,
            generation,
            x,
            y,
        } = self.position(message.x, message.y)?
        else {
            return Err(InputError::InvalidCoordinate);
        };
        Ok(InputCommand::Pen {
            output,
            generation,
            x,
            y,
            eraser: message.tool == PenToolMsg::Eraser,
            in_proximity: message.in_proximity,
            touching: message.in_proximity && message.touching,
            buttons: if message.in_proximity {
                message.buttons
            } else {
                0
            },
            pressure: message.pressure,
            tilt_x: message.tilt_x_degrees,
            tilt_y: message.tilt_y_degrees,
            rotation: message.rotation_degrees,
        })
    }

    /// Relative motion.
    #[must_use]
    pub fn mouse_move_relative(message: &MouseMoveRelativeMsg) -> InputCommand {
        InputCommand::PointerRelative {
            dx: f64::from(message.dx),
            dy: f64::from(message.dy),
        }
    }

    /// A button edge, at the message's position in absolute mode.
    ///
    /// # Errors
    ///
    /// [`InputError::InvalidButton`] or [`InputError::InvalidCoordinate`].
    pub fn mouse_button(
        &mut self,
        message: &MouseButtonMsg,
    ) -> Result<Vec<InputCommand>, InputError> {
        let code = mouse_button_code(message.button)?;
        let mut commands = Vec::with_capacity(2);
        if matches!(message.motion_mode, PointerMotionMode::Absolute) {
            commands.push(self.position(message.x, message.y)?);
        }
        if let Some((code, pressed)) = plan_button_edge(&self.held_buttons, code, message.pressed) {
            commit_button_edge(&mut self.held_buttons, code, pressed);
            commands.push(InputCommand::PointerButton {
                button: i32::from(code),
                pressed,
            });
        }
        Ok(commands)
    }

    /// Scroll: whole detents for line units, continuous scroll for point
    /// units. The Deck's positive `dy` scrolls up; Mutter's scrolls down.
    ///
    /// # Errors
    ///
    /// [`InputError::InvalidCoordinate`] in absolute mode.
    pub fn mouse_scroll(
        &mut self,
        message: &MouseScrollMsg,
    ) -> Result<Vec<InputCommand>, InputError> {
        let mut commands = Vec::with_capacity(3);
        if matches!(message.motion_mode, PointerMotionMode::Absolute) {
            commands.push(self.position(message.x, message.y)?);
        }
        let unit = arcen_input::ScrollUnit::from(message.unit);
        let vertical = legacy_scroll_to_wheel_units(message.dy, unit);
        let horizontal = legacy_scroll_to_wheel_units(message.dx, unit);
        self.wheel(unit, vertical, horizontal, &mut commands);
        Ok(commands)
    }

    /// A region-scoped pointer button: the position on its output, then the
    /// edge (idempotent like [`Self::mouse_button`]).
    ///
    /// # Errors
    ///
    /// [`InputError::InvalidButton`], or [`InputError::InvalidCoordinate`]
    /// for a position on no output.
    pub fn region_button(
        &mut self,
        mapped: &super::region_adapter::MappedRegionButton,
    ) -> Result<Vec<InputCommand>, InputError> {
        let code = mouse_button_code(mapped.button)?;
        let mut commands = vec![self.desktop_position(mapped.position.x, mapped.position.y)?];
        if let Some((code, pressed)) = plan_button_edge(&self.held_buttons, code, mapped.pressed) {
            commit_button_edge(&mut self.held_buttons, code, pressed);
            commands.push(InputCommand::PointerButton {
                button: i32::from(code),
                pressed,
            });
        }
        Ok(commands)
    }

    /// A region-scoped scroll sample (wheel units, positive up/right): the
    /// position on its output, then the scroll; an ended gesture drops the
    /// partial-detent remainder.
    ///
    /// # Errors
    ///
    /// [`InputError::InvalidCoordinate`] for a position on no output.
    pub fn region_scroll(
        &mut self,
        mapped: &super::region_adapter::MappedRegionScroll,
    ) -> Result<Vec<InputCommand>, InputError> {
        let mut commands = vec![self.desktop_position(mapped.position.x, mapped.position.y)?];
        self.wheel(mapped.unit, mapped.delta_y, mapped.delta_x, &mut commands);
        if matches!(
            mapped.phase,
            arcen_input::ScrollPhase::Ended | arcen_input::ScrollPhase::Cancelled
        ) {
            self.vertical_remainder = 0;
            self.horizontal_remainder = 0;
        }
        Ok(commands)
    }

    /// Scroll in wheel units (positive up and right): whole detents for
    /// line units, continuous scroll for point units. Mutter's vertical
    /// axis points down.
    fn wheel(
        &mut self,
        unit: arcen_input::ScrollUnit,
        vertical: i64,
        horizontal: i64,
        commands: &mut Vec<InputCommand>,
    ) {
        match unit {
            arcen_input::ScrollUnit::Line => {
                let up = split_legacy_detents(&mut self.vertical_remainder, vertical);
                let right = split_legacy_detents(&mut self.horizontal_remainder, horizontal);
                if up != 0 {
                    commands.push(InputCommand::PointerAxisDiscrete {
                        axis: 0,
                        steps: up.saturating_neg(),
                    });
                }
                if right != 0 {
                    commands.push(InputCommand::PointerAxisDiscrete {
                        axis: 1,
                        steps: right,
                    });
                }
            }
            arcen_input::ScrollUnit::Point => {
                if vertical != 0 || horizontal != 0 {
                    #[allow(clippy::cast_precision_loss)] // Bounded by the session's checks.
                    let units = |value: i64| {
                        value as f64 * MUTTER_SCROLL_UNITS_PER_DETENT
                            / WHEEL_UNITS_PER_DETENT as f64
                    };
                    commands.push(InputCommand::PointerAxis {
                        dx: units(horizontal),
                        dy: -units(vertical),
                        finish: false,
                        source: AxisSource::Continuous,
                    });
                }
            }
        }
    }
}

const fn key(code: u16, pressed: bool) -> InputCommand {
    InputCommand::Key {
        keycode: code as u32,
        pressed,
    }
}

/// The X keysym of a Latin-1 Qt key the evdev table lacks: Latin-1 keysyms
/// equal their code points. Qt names letters by their capital; the keysym
/// is the small letter, so Shift in the modifier mask decides the case.
#[must_use]
pub fn latin1_keysym(qt_key: u32) -> Option<u32> {
    match qt_key {
        0x20..=0x7E | 0xA0..=0xBF | 0xD7 | 0xDF..=0xFF => Some(qt_key),
        0xC0..=0xDE => Some(qt_key + 0x20),
        _ => None,
    }
}

/// How a pushed command was queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enqueued {
    /// Queued, or coalesced with the motion ahead of it.
    Queued,
    /// The queue was full: everything waiting was replaced by one
    /// `release_all`, and this command was not queued.
    Collapsed,
}

/// Why the queue did not take a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum QueueRefusal {
    /// The injector is closed or lost for good.
    #[error("the Wayland input queue is closed")]
    Closed,
    /// The session connection broke and is being restored.
    #[error("the Wayland input connection is being restored")]
    Interrupted,
    /// The command was planned before the connection broke.
    #[error("the command was planned for a connection that broke")]
    Stale,
}

#[derive(Debug)]
struct QueueState {
    pending: VecDeque<InputCommand>,
    closed: bool,
    accepting: bool,
    epoch: u64,
    coalesced: u64,
    collapsed: u64,
}

/// Ordered, bounded commands for one session connection.
///
/// The epoch changes whenever the connection breaks: everything queued is
/// then dropped (the session releases what the broken connection held), and
/// a command planned against the old epoch is refused, so the planner's
/// held-state mirror can never disagree with what was delivered.
#[derive(Debug)]
pub struct InjectionQueue {
    state: Mutex<QueueState>,
    ready: tokio::sync::Notify,
    capacity: usize,
}

impl InjectionQueue {
    /// An empty queue holding at most `capacity` commands.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(QueueState {
                pending: VecDeque::new(),
                closed: false,
                accepting: true,
                epoch: 0,
                coalesced: 0,
                collapsed: 0,
            }),
            ready: tokio::sync::Notify::new(),
            capacity: capacity.max(1),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The current connection epoch.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.lock().epoch
    }

    /// Queues `command`, planned in `epoch`, behind everything waiting,
    /// coalescing it with motion of the same kind that is last in line.
    ///
    /// # Errors
    ///
    /// [`QueueRefusal`] when the queue is closed, interrupted, or the epoch
    /// moved on.
    pub fn push_at(&self, epoch: u64, command: InputCommand) -> Result<Enqueued, QueueRefusal> {
        let mut state = self.lock();
        if state.closed {
            return Err(QueueRefusal::Closed);
        }
        if state.epoch != epoch {
            return Err(QueueRefusal::Stale);
        }
        if !state.accepting {
            return Err(QueueRefusal::Interrupted);
        }
        if let Some(last) = state.pending.back_mut() {
            if coalesce(last, &command) {
                state.coalesced = state.coalesced.saturating_add(1);
                return Ok(Enqueued::Queued);
            }
        }
        if state.pending.len() >= self.capacity {
            state.pending.clear();
            state.pending.push_back(InputCommand::ReleaseAll);
            state.collapsed = state.collapsed.saturating_add(1);
            drop(state);
            self.ready.notify_one();
            return Ok(Enqueued::Collapsed);
        }
        state.pending.push_back(command);
        drop(state);
        self.ready.notify_one();
        Ok(Enqueued::Queued)
    }

    /// [`Self::push_at`] in the current epoch.
    ///
    /// # Errors
    ///
    /// [`QueueRefusal`] when the queue is closed or interrupted.
    pub fn push(&self, command: InputCommand) -> Result<Enqueued, QueueRefusal> {
        let epoch = self.epoch();
        self.push_at(epoch, command)
    }

    /// The connection broke: drops everything queued, starts a new epoch
    /// and accepts nothing until [`Self::resume`]. Returns how many
    /// commands were dropped.
    pub fn interrupt(&self) -> usize {
        let mut state = self.lock();
        let dropped = state.pending.len();
        state.pending.clear();
        state.accepting = false;
        state.epoch = state.epoch.wrapping_add(1);
        dropped
    }

    /// A new connection was proven: accepts commands again.
    pub fn resume(&self) {
        let mut state = self.lock();
        state.pending.clear();
        state.accepting = true;
    }

    /// The next command, or `None` once the queue is closed and empty.
    /// Cancel safe.
    pub async fn next(&self) -> Option<InputCommand> {
        loop {
            let notified = self.ready.notified();
            {
                let mut state = self.lock();
                if let Some(command) = state.pending.pop_front() {
                    return Some(command);
                }
                if state.closed {
                    return None;
                }
            }
            notified.await;
        }
    }

    /// Accepts nothing more; what is queued is still delivered.
    pub fn close(&self) {
        {
            let mut state = self.lock();
            state.closed = true;
            state.accepting = false;
        }
        self.ready.notify_waiters();
        self.ready.notify_one();
    }

    /// Whether the queue was closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// Commands waiting.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().pending.len()
    }

    /// Whether nothing is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Motion merged into motion ahead of it, and collapses, so far.
    #[must_use]
    pub fn counters(&self) -> (u64, u64) {
        let state = self.lock();
        (state.coalesced, state.collapsed)
    }
}

/// Merges `next` into `last` when both are motion of the same kind.
fn coalesce(last: &mut InputCommand, next: &InputCommand) -> bool {
    // Pen motion with the same tool, contact and buttons is only the latest
    // position and axes; an edge is never merged away.
    if let (Some(previous), Some(following)) = (pen_edge_state(last), pen_edge_state(next)) {
        if previous == following {
            *last = next.clone();
            return true;
        }
        return false;
    }
    match (last, next) {
        (
            InputCommand::PointerAbsolute {
                output,
                generation,
                x,
                y,
            },
            InputCommand::PointerAbsolute {
                output: next_output,
                generation: next_generation,
                x: next_x,
                y: next_y,
            },
        ) if output == next_output && generation == next_generation => {
            *x = *next_x;
            *y = *next_y;
            true
        }
        (
            InputCommand::PointerRelative { dx, dy },
            InputCommand::PointerRelative {
                dx: next_dx,
                dy: next_dy,
            },
        ) => {
            *dx += next_dx;
            *dy += next_dy;
            true
        }
        (
            InputCommand::PointerAxis {
                dx,
                dy,
                finish: false,
                source: AxisSource::Continuous,
            },
            InputCommand::PointerAxis {
                dx: next_dx,
                dy: next_dy,
                finish: false,
                source: AxisSource::Continuous,
            },
        ) => {
            *dx += next_dx;
            *dy += next_dy;
            true
        }
        _ => false,
    }
}

/// Everything of a pen sample but its position and axes.
fn pen_edge_state(command: &InputCommand) -> Option<(&str, u64, bool, bool, bool, u16)> {
    match command {
        InputCommand::Pen {
            output,
            generation,
            eraser,
            in_proximity,
            touching,
            buttons,
            ..
        } => Some((
            output.as_str(),
            *generation,
            *eraser,
            *in_proximity,
            *touching,
            *buttons,
        )),
        _ => None,
    }
}

/// One request/reply connection to the session's input socket.
struct SessionConnection {
    reader:
        crate::bounded_io::BoundedLineReader<tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl SessionConnection {
    fn new(stream: tokio::net::UnixStream) -> Self {
        let (reader, writer) = stream.into_split();
        Self {
            reader: crate::bounded_io::BoundedLineReader::new(tokio::io::BufReader::new(reader)),
            writer,
        }
    }

    async fn read_reply(&mut self) -> Result<InputReply, WaylandInputError> {
        let reply = self
            .reader
            .next_bounded_line()
            .await?
            .ok_or(WaylandInputError::Closed)?;
        if reply.truncated {
            return Err(WaylandInputError::MalformedReply);
        }
        serde_json::from_str(&reply.text).map_err(|_| WaylandInputError::MalformedReply)
    }

    /// Sends `command`, valid for `validity` (how long its reply is waited
    /// for, so the session never injects one this side gave up on), and
    /// reads the reply.
    async fn request(
        &mut self,
        command: &InputCommand,
        validity: Duration,
    ) -> Result<InputReply, WaylandInputError> {
        use tokio::io::AsyncWriteExt;
        let request = InputRequest::new(command.clone(), validity);
        let mut line =
            serde_json::to_vec(&request).map_err(|_| WaylandInputError::MalformedReply)?;
        line.push(b'\n');
        if let Err(error) = self.writer.write_all(&line).await {
            // A superseded connection was told so before it was closed.
            return match tokio::time::timeout(NOTICE_READ_TIMEOUT, self.read_reply()).await {
                Ok(Ok(notice)) if is_superseded(&notice) => Ok(notice),
                _ => Err(error.into()),
            };
        }
        self.read_reply().await
    }

    /// Connects to the session's socket and proves it.
    async fn open(
        target: &WaylandInputTarget,
    ) -> Result<(Self, WaylandInputProof), WaylandInputError> {
        tokio::time::timeout(CONNECT_TIMEOUT, async {
            verify_socket(target)?;
            let stream = tokio::net::UnixStream::connect(&target.socket).await?;
            verify_peer(&stream, target)?;
            let mut connection = Self::new(stream);
            let reply = connection
                .request(&InputCommand::Hello, CONNECT_TIMEOUT)
                .await?;
            let facts = match reply {
                InputReply {
                    ok: true,
                    session: Some(facts),
                    ..
                } => facts,
                // A newer connection arrived before the hello was answered:
                // it owns the input now.
                other if is_superseded(&other) => return Err(WaylandInputError::Superseded),
                other => {
                    return Err(WaylandInputError::HelloRefused(
                        other.error.unwrap_or_default(),
                    ))
                }
            };
            let proof = WaylandInputProof::verify(target, facts)?;
            Ok((connection, proof))
        })
        .await
        .map_err(|_| WaylandInputError::Timeout)?
    }
}

fn is_superseded(reply: &InputReply) -> bool {
    !reply.ok && reply.error.as_deref() == Some(SUPERSEDED_REASON)
}

/// Why an injector's input is unavailable for the rest of its attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InjectorLoss {
    /// A newer connection (another attachment, or the session user) took
    /// the session's input. Reconnecting would take it back, so the
    /// injector does not.
    Superseded,
    /// The connection broke and the session could not be reached again.
    Unreachable(String),
    /// The attachment closed the injector.
    Closed,
}

impl std::fmt::Display for InjectorLoss {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Superseded => formatter
                .write_str("another connection took the session's input; not taking it back"),
            Self::Unreachable(error) => write!(
                formatter,
                "the session's input could not be restored: {error}"
            ),
            Self::Closed => formatter.write_str("the injector was closed"),
        }
    }
}

/// Where an injector's session connection stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InjectorStatus {
    /// Delivering on the session's connection with this number.
    Connected {
        /// The session's connection number.
        connection: u64,
    },
    /// The connection broke; this reconnection attempt is running.
    Reconnecting {
        /// One-based attempt.
        attempt: usize,
    },
    /// No more input for this attachment.
    Lost(InjectorLoss),
}

/// The Pier's injector for one attachment to a headless Wayland session.
///
/// Synchronous towards the input lane: every call plans and queues, never
/// waits for the compositor. When the session connection breaks, the
/// delivery task reconnects with the same target and proof, at most
/// [`RECONNECT_BACKOFF`]`.len()` times; meanwhile input is refused and
/// counted, and what the broken connection held is released by the session.
/// If the connection cannot be restored, or another connection superseded
/// it, the status turns [`InjectorStatus::Lost`] and the attachment ends.
/// Dropping the injector releases everything it holds and closes its
/// connection, after which the session releases anything left.
pub struct WaylandInjector {
    planner: WaylandInputPlanner,
    /// Region-scoped input of a committed multi-monitor topology.
    region: Option<super::region_adapter::RegionInputAdapter>,
    queue: Arc<InjectionQueue>,
    status: tokio::sync::watch::Receiver<InjectorStatus>,
    epoch: u64,
    proof: WaylandInputProof,
    stats: Arc<InputStats>,
    target: tokio::sync::watch::Receiver<WaylandInputTarget>,
    retarget: InputRetarget,
    notices: tokio::sync::mpsc::Sender<String>,
    notices_rx: Option<tokio::sync::mpsc::Receiver<String>>,
    text_window: (std::time::Instant, u32),
    text_parts: Arc<TextParts>,
    /// Lab (`wayland.lab_seat_pen`): pen samples go to this desktop's own
    /// private-seat tablet instead of the EIS seat.
    #[cfg(target_os = "linux")]
    seat_pen: Option<std::sync::Arc<super::seat_pen::SeatPen>>,
}

/// Commits typed in several parts, by sequence: their result is reported
/// once, after the last part. Shared by the injector, which splits commits,
/// and its delivery, which hears how each part went.
#[derive(Debug, Default)]
struct TextParts(std::sync::Mutex<std::collections::HashMap<u64, PartedCommit>>);

#[derive(Debug, Default)]
struct PartedCommit {
    remaining: usize,
    outcome: Option<TextOutcome>,
}

impl TextParts {
    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<u64, PartedCommit>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `parts` commands are about to carry commit `sequence`.
    fn expect(&self, sequence: u64, parts: usize) {
        self.lock().entry(sequence).or_default().remaining += parts;
    }

    /// The commit could not be queued; nothing is reported for it.
    fn withdraw(&self, sequence: u64) {
        self.lock().remove(&sequence);
    }

    /// One part was typed; the whole commit's outcome once it was the last.
    fn part_done(&self, sequence: u64, outcome: &TextOutcome) -> Option<TextOutcome> {
        let mut commits = self.lock();
        let Some(commit) = commits.get_mut(&sequence) else {
            return Some(outcome.clone());
        };
        match &mut commit.outcome {
            Some(sum) => sum.absorb(outcome),
            None => commit.outcome = Some(outcome.clone()),
        }
        commit.remaining = commit.remaining.saturating_sub(1);
        if commit.remaining > 0 {
            return None;
        }
        commits.remove(&sequence).and_then(|commit| commit.outcome)
    }

    /// The queue was interrupted: parts still waiting were dropped.
    fn clear(&self) {
        self.lock().clear();
    }
}

/// Moves a live injector to the session's next output generation.
#[derive(Clone)]
pub struct InputRetarget(Arc<tokio::sync::watch::Sender<WaylandInputTarget>>);

impl InputRetarget {
    /// Addresses `successor` from the next input on, and reconnects to it if
    /// the connection breaks.
    ///
    /// # Errors
    ///
    /// [`WaylandInputError::FactsMismatch`] when `successor` is not the same
    /// session at a newer generation.
    pub fn retarget(&self, successor: WaylandInputTarget) -> Result<(), WaylandInputError> {
        if !self.0.borrow().accepts_successor(&successor) {
            return Err(WaylandInputError::FactsMismatch("output generation"));
        }
        self.0.send_replace(successor);
        Ok(())
    }
}

impl WaylandInjector {
    /// Connects to the session's input socket, proves it is the session's,
    /// and starts delivering.
    ///
    /// # Errors
    ///
    /// [`WaylandInputError`] when the socket, its peer or its facts are not
    /// this session's, or it does not answer within [`CONNECT_TIMEOUT`].
    pub async fn connect(
        target: WaylandInputTarget,
    ) -> Result<(Self, Arc<InputStats>), WaylandInputError> {
        Self::connect_with_backoff(target, RECONNECT_BACKOFF.to_vec()).await
    }

    async fn connect_with_backoff(
        target: WaylandInputTarget,
        backoff: Vec<Duration>,
    ) -> Result<(Self, Arc<InputStats>), WaylandInputError> {
        let (connection, proof) = SessionConnection::open(&target).await?;
        let planner = WaylandInputPlanner::new(&target).ok_or(WaylandInputError::Inconsistent)?;
        let queue = Arc::new(InjectionQueue::new(INJECTION_QUEUE_CAPACITY));
        let (status_tx, status) = tokio::sync::watch::channel(InjectorStatus::Connected {
            connection: proof.facts().connection,
        });
        let (target_tx, target_rx) = tokio::sync::watch::channel(target);
        let (notices, notices_rx) = tokio::sync::mpsc::channel(TEXT_NOTICE_CAPACITY);
        let text_parts = Arc::new(TextParts::default());
        tokio::spawn(deliver(
            connection,
            target_rx.clone(),
            Arc::clone(&queue),
            status_tx,
            backoff,
            (notices.clone(), Arc::clone(&text_parts)),
        ));
        let stats = Arc::new(InputStats::default());
        Ok((
            Self {
                planner,
                region: None,
                epoch: queue.epoch(),
                queue,
                status,
                proof,
                stats: Arc::clone(&stats),
                target: target_rx,
                retarget: InputRetarget(Arc::new(target_tx)),
                notices,
                notices_rx: Some(notices_rx),
                text_window: (std::time::Instant::now(), 0),
                text_parts,
                #[cfg(target_os = "linux")]
                seat_pen: None,
            },
            stats,
        ))
    }

    /// What the session proved when this injector connected.
    #[must_use]
    pub const fn proof(&self) -> &WaylandInputProof {
        &self.proof
    }

    /// Whether the session types committed text, and characters beyond its
    /// layout: `None` when its agent does not type committed text.
    #[must_use]
    pub fn text_capability(&self) -> Option<bool> {
        let facts = self.proof.facts().text.as_ref()?;
        Some(facts.utf8 == crate::session::wayland::text::Utf8Route::Offered)
    }

    /// The text-commit results and session layouts for the Deck, as JSON
    /// for its input lane. Taken once, by an attachment whose Deck
    /// advertised committed text.
    pub fn take_text_notices(&mut self) -> Option<tokio::sync::mpsc::Receiver<String>> {
        self.notices_rx.take()
    }

    /// Types a Deck's committed text through the session, which reports
    /// what it could not type.
    ///
    /// # Errors
    ///
    /// When the connection is down or its queue collapsed.
    pub fn text_commit(&mut self, message: &TextCommitMsg) -> Result<(), InputError> {
        let refuse = |injector: &Self, reason| {
            send_notice(&injector.notices, &text_refusal_notice(message, reason));
        };
        if self.text_capability().is_none() {
            refuse(self, TextRefusalReason::HostUnsupported);
            return Ok(());
        }
        if validate_commit_text(&message.text).is_err() {
            refuse(self, TextRefusalReason::InvalidText);
            return Ok(());
        }
        let now = std::time::Instant::now();
        if now.duration_since(self.text_window.0) >= Duration::from_secs(1) {
            self.text_window = (now, 0);
        }
        // Typed in parts, each well inside its command budget; the rate
        // counts parts, so long commits cannot flood the queue.
        let parts =
            arcen_input::text::text_parts(&message.text, arcen_input::text::MAX_TEXT_PART_CHARS);
        let count = u32::try_from(parts.len()).unwrap_or(u32::MAX);
        if self.text_window.1.saturating_add(count) > MAX_TEXT_PARTS_PER_SECOND {
            refuse(self, TextRefusalReason::RateLimited);
            return Ok(());
        }
        self.text_window.1 += count;
        let commands = parts
            .iter()
            .map(|part| InputCommand::Text {
                text: (*part).to_owned(),
                sequence: message.sequence,
            })
            .collect::<Vec<_>>();
        self.text_parts.expect(message.sequence, commands.len());
        match self.submit(|_| Ok((commands, ()))) {
            Ok(()) => {
                self.stats.key_events.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(error) => {
                self.text_parts.withdraw(message.sequence);
                refuse(self, TextRefusalReason::InputUnavailable);
                Err(error)
            }
        }
    }

    /// Accepts region-scoped input for the attachment's committed topology:
    /// each region's position lands on the output that shows it.
    pub fn set_region_input(&mut self, adapter: super::region_adapter::RegionInputAdapter) {
        self.region = Some(adapter);
    }

    /// Whether region-scoped input is accepted.
    #[must_use]
    pub const fn region_input_available(&self) -> bool {
        self.region.is_some()
    }

    fn region(&mut self) -> Result<&mut super::region_adapter::RegionInputAdapter, InputError> {
        self.region.as_mut().ok_or(InputError::RegionUnavailable)
    }

    /// A region pointer entering its region.
    ///
    /// # Errors
    ///
    /// For input the shared region state refuses, a position on no output,
    /// or when the connection is down.
    pub fn region_pointer_enter(
        &mut self,
        message: &arcen_protocol::messages::RegionPointerEnterMsg,
    ) -> Result<(), InputError> {
        let point = self.region()?.pointer_enter(message)?;
        self.submit(|planner| Ok((vec![planner.desktop_position(point.x, point.y)?], ())))?;
        self.stats.mouse_moves.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// A region pointer leaving its region.
    ///
    /// # Errors
    ///
    /// As [`Self::region_pointer_enter`].
    pub fn region_pointer_leave(
        &mut self,
        message: &arcen_protocol::messages::RegionPointerLeaveMsg,
    ) -> Result<(), InputError> {
        let point = self.region()?.pointer_leave(message)?;
        self.submit(|planner| Ok((vec![planner.desktop_position(point.x, point.y)?], ())))?;
        self.stats.mouse_moves.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Region-local absolute motion.
    ///
    /// # Errors
    ///
    /// As [`Self::region_pointer_enter`].
    pub fn region_pointer_motion(
        &mut self,
        message: &arcen_protocol::messages::RegionPointerMotionMsg,
    ) -> Result<(), InputError> {
        let point = self.region()?.pointer_motion(message)?;
        self.submit(|planner| Ok((vec![planner.desktop_position(point.x, point.y)?], ())))?;
        self.stats.mouse_moves.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// A region-scoped button edge at the latest pointer position.
    ///
    /// # Errors
    ///
    /// As [`Self::region_pointer_enter`], or for an invalid button.
    pub fn region_pointer_button(
        &mut self,
        message: &arcen_protocol::messages::RegionPointerButtonMsg,
    ) -> Result<(), InputError> {
        let mapped = self.region()?.pointer_button(message)?;
        self.submit(|planner| Ok((planner.region_button(&mapped)?, ())))?;
        self.stats.mouse_buttons.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// A region-scoped scroll sample at the latest pointer position.
    ///
    /// # Errors
    ///
    /// As [`Self::region_pointer_enter`].
    pub fn region_pointer_scroll(
        &mut self,
        message: &arcen_protocol::messages::RegionPointerScrollMsg,
    ) -> Result<(), InputError> {
        let mapped = self.region()?.pointer_scroll(message)?;
        self.submit(|planner| Ok((planner.region_scroll(&mapped)?, ())))?;
        self.stats.scroll_events.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// A handle that moves this injector to the session's next output
    /// generation (resize, topology change).
    #[must_use]
    pub fn retarget_handle(&self) -> InputRetarget {
        self.retarget.clone()
    }

    /// Follows the injector's connection state.
    #[must_use]
    pub fn status(&self) -> tokio::sync::watch::Receiver<InjectorStatus> {
        self.status.clone()
    }

    /// Whether input is unavailable for the rest of the attachment.
    #[must_use]
    pub fn is_lost(&self) -> bool {
        matches!(*self.status.borrow(), InjectorStatus::Lost(_))
    }

    /// Forgets the planner's mirror when the connection broke since the
    /// last call; reports whether input is being accepted.
    fn sync(&mut self) -> Result<(), InputError> {
        if self.target.has_changed().unwrap_or(false) {
            let target = self.target.borrow_and_update().clone();
            self.planner.retarget(&target);
        }
        let epoch = self.queue.epoch();
        if epoch != self.epoch {
            self.planner.forget_held();
            self.epoch = epoch;
        }
        match &*self.status.borrow() {
            InjectorStatus::Connected { .. } => Ok(()),
            InjectorStatus::Reconnecting { .. } => Err(InputError::InjectorReconnecting),
            InjectorStatus::Lost(_) => Err(InputError::InjectorLost),
        }
    }

    fn refused(&mut self, error: InputError) -> InputError {
        self.stats.unavailable_drops.fetch_add(1, Ordering::Relaxed);
        error
    }

    /// Plans with `plan` and queues the result, unless the connection is
    /// down.
    fn submit<T>(
        &mut self,
        plan: impl FnOnce(&mut WaylandInputPlanner) -> Result<(Vec<InputCommand>, T), InputError>,
    ) -> Result<T, InputError> {
        if let Err(error) = self.sync() {
            return Err(self.refused(error));
        }
        let (commands, value) = plan(&mut self.planner)?;
        for command in commands {
            match self.queue.push_at(self.epoch, command) {
                Ok(Enqueued::Queued) => {}
                Ok(Enqueued::Collapsed) => {
                    self.planner.forget_held();
                    self.stats.resets.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        target: target::INPUT,
                        capacity = INJECTION_QUEUE_CAPACITY,
                        "Wayland input queue overflowed: everything waiting was replaced by a release of all held input"
                    );
                    return Err(InputError::QueueCollapsed);
                }
                Err(refusal) => {
                    // The connection broke while this was planned.
                    self.planner.forget_held();
                    let error = match refusal {
                        QueueRefusal::Closed => InputError::InjectorLost,
                        QueueRefusal::Interrupted | QueueRefusal::Stale => {
                            InputError::InjectorReconnecting
                        }
                    };
                    return Err(self.refused(error));
                }
            }
        }
        Ok(value)
    }

    /// See [`WaylandInputPlanner::key_event`].
    ///
    /// # Errors
    ///
    /// When the connection is down or its queue collapsed.
    pub fn key_event(&mut self, message: &KeyEventMsg) -> Result<(), InputError> {
        let unmapped = self.submit(|planner| {
            let plan = planner.key_event(message);
            Ok((plan.commands, plan.unmapped))
        })?;
        if unmapped {
            self.stats.unmapped_keys.fetch_add(1, Ordering::Relaxed);
        }
        self.stats.key_events.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Releases keys only.
    ///
    /// # Errors
    ///
    /// When the connection is down or its queue collapsed.
    pub fn reset_keyboard_held(&mut self) -> Result<(), InputError> {
        self.stats.resets.fetch_add(1, Ordering::Relaxed);
        self.submit(|planner| Ok((planner.reset_keyboard(), ())))
    }

    /// Releases every key, button and touch this attachment holds.
    ///
    /// # Errors
    ///
    /// When the connection is down; the session released what the broken
    /// connection held by itself.
    pub fn reset_held(&mut self) -> Result<(), InputError> {
        self.stats.resets.fetch_add(1, Ordering::Relaxed);
        if let Some(region) = self.region.as_mut() {
            let _ = region.release_all();
        }
        #[cfg(target_os = "linux")]
        if let Some(pen) = self.seat_pen.as_ref() {
            pen.release();
        }
        self.submit(|planner| Ok((vec![planner.reset_all()], ())))
    }

    /// # Errors
    ///
    /// For a non-finite coordinate, or when the connection is down.
    pub fn mouse_move(&mut self, message: &MouseMoveMsg) -> Result<(), InputError> {
        self.submit(|planner| Ok((vec![planner.mouse_move(message)?], ())))?;
        self.stats.mouse_moves.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// # Errors
    ///
    /// When the connection is down.
    pub fn mouse_move_relative(
        &mut self,
        message: &MouseMoveRelativeMsg,
    ) -> Result<(), InputError> {
        self.submit(|_| Ok((vec![WaylandInputPlanner::mouse_move_relative(message)], ())))?;
        self.stats.mouse_moves.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// # Errors
    ///
    /// For an invalid button or coordinate, or when the connection is down.
    pub fn mouse_button(&mut self, message: &MouseButtonMsg) -> Result<(), InputError> {
        self.submit(|planner| Ok((planner.mouse_button(message)?, ())))?;
        self.stats.mouse_buttons.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// # Errors
    ///
    /// For an invalid coordinate, or when the connection is down.
    pub fn mouse_scroll(&mut self, message: &MouseScrollMsg) -> Result<(), InputError> {
        self.submit(|planner| Ok((planner.mouse_scroll(message)?, ())))?;
        self.stats.scroll_events.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Lab (`wayland.lab_seat_pen`): routes pen samples to `pen`, the
    /// private-seat tablet of the desktop this attachment serves, which that
    /// desktop's compositor opened.
    #[cfg(target_os = "linux")]
    pub fn use_seat_pen(&mut self, pen: std::sync::Arc<super::seat_pen::SeatPen>) {
        self.seat_pen = Some(pen);
    }

    fn has_seat_pen(&self) -> bool {
        #[cfg(target_os = "linux")]
        return self.seat_pen.is_some();
        #[cfg(not(target_os = "linux"))]
        return false;
    }

    /// What the session's pen delivers: the private-seat tablet's fixed set
    /// (lab), or what the emulated stylus bound.
    #[must_use]
    pub fn pen_capabilities(&self) -> PenCapabilityTruth {
        if self.has_seat_pen() {
            return PenCapabilityTruth::linux_uinput(true);
        }
        self.proof.pen_capabilities()
    }

    /// Why the session has no pen; `None` when it has one.
    #[must_use]
    pub fn pen_unavailable(&self) -> Option<StylusUnavailable> {
        if self.has_seat_pen() {
            return None;
        }
        self.proof.pen_unavailable()
    }

    /// A typed pen sample, for the session's emulated stylus.
    ///
    /// # Errors
    ///
    /// [`InputError::PenUnavailable`] when the session proved no stylus;
    /// otherwise for an invalid coordinate, or when the connection is down.
    pub fn pen_event(&mut self, message: &PenEventMsg) -> Result<(), InputError> {
        #[cfg(target_os = "linux")]
        if let Some(pen) = self.seat_pen.as_ref() {
            pen.pen_event(message)?;
            self.stats.pen_events.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        if !self.pen_capabilities().pen_available() {
            return Err(InputError::PenUnavailable);
        }
        self.submit(|planner| Ok((vec![planner.pen(message)?], ())))?;
        self.stats.pen_events.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

impl Drop for WaylandInjector {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(pen) = self.seat_pen.as_ref() {
            pen.release();
        }
        let _ = self.queue.push(InputCommand::ReleaseAll);
        self.queue.close();
    }
}

/// How one connection's delivery ended.
enum Served {
    /// The queue closed and drained (or its drain budget ran out).
    Closed,
    /// The session said a newer connection superseded this one.
    Superseded,
    /// The connection broke or stopped answering.
    Broken(WaylandInputError),
}

/// Delivers queued commands in order, one request at a time, reconnecting
/// with the same target when a connection breaks.
async fn deliver(
    mut connection: SessionConnection,
    target: tokio::sync::watch::Receiver<WaylandInputTarget>,
    queue: Arc<InjectionQueue>,
    status: tokio::sync::watch::Sender<InjectorStatus>,
    backoff: Vec<Duration>,
    (notices, text_parts): (tokio::sync::mpsc::Sender<String>, Arc<TextParts>),
) {
    let mut refused = 0_u64;
    let mut keymap_seen = 0_u64;
    let loss = loop {
        let mut text = TextNotices {
            notices: &notices,
            keymap_seen: &mut keymap_seen,
            parts: &text_parts,
        };
        let error = match serve(&mut connection, &queue, &mut refused, &mut text).await {
            Served::Closed => break InjectorLoss::Closed,
            Served::Superseded => {
                queue.interrupt();
                text_parts.clear();
                break InjectorLoss::Superseded;
            }
            Served::Broken(error) => error,
        };
        let dropped = queue.interrupt();
        text_parts.clear();
        tracing::warn!(
            target: target::INPUT,
            %error,
            dropped,
            "Wayland input connection broke; the session released what it held; reconnecting"
        );
        let mut restored = None;
        let mut superseded = false;
        let mut last_error = error.to_string();
        for (index, delay) in backoff.iter().enumerate() {
            status.send_replace(InjectorStatus::Reconnecting { attempt: index + 1 });
            tokio::time::sleep(*delay).await;
            if queue.is_closed() {
                break;
            }
            let current = target.borrow().clone();
            match SessionConnection::open(&current).await {
                Ok(opened) => {
                    restored = Some(opened);
                    break;
                }
                // Retrying would take the input back from its new owner.
                Err(WaylandInputError::Superseded) => {
                    superseded = true;
                    break;
                }
                Err(error) => {
                    tracing::info!(
                        target: target::INPUT,
                        attempt = index + 1,
                        %error,
                        "Wayland input reconnection failed"
                    );
                    last_error = error.to_string();
                }
            }
        }
        if queue.is_closed() {
            break InjectorLoss::Closed;
        }
        if superseded {
            break InjectorLoss::Superseded;
        }
        let Some((reopened, proof)) = restored else {
            break InjectorLoss::Unreachable(last_error);
        };
        connection = reopened;
        queue.resume();
        let number = proof.facts().connection;
        status.send_replace(InjectorStatus::Connected { connection: number });
        tracing::info!(
            target: target::INPUT,
            connection = number,
            "Wayland input connection restored and proven"
        );
    };
    queue.close();
    if loss != InjectorLoss::Closed {
        tracing::warn!(target: target::INPUT, reason = %loss, "Wayland session input lost");
    }
    status.send_replace(InjectorStatus::Lost(loss));
}

/// Delivers on one connection until the queue closes or the connection
/// ends. While idle it watches the connection, so a superseded notice or a
/// closed socket is noticed before the next command.
/// Where a connection's text results and layout changes go.
struct TextNotices<'a> {
    notices: &'a tokio::sync::mpsc::Sender<String>,
    keymap_seen: &'a mut u64,
    parts: &'a TextParts,
}

/// Whether delivery goes on after a reply's notices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AfterReply {
    Continue,
    /// The session answered the layout request with the superseded notice.
    Superseded,
}

impl TextNotices<'_> {
    /// Reports a commit's result once its last part was typed, and fetches
    /// the session's layout when the reply says it changed.
    async fn after_reply(
        &mut self,
        connection: &mut SessionConnection,
        command: &InputCommand,
        reply: &InputReply,
    ) -> Result<AfterReply, WaylandInputError> {
        if let (InputCommand::Text { sequence, .. }, Some(outcome)) = (command, &reply.text) {
            if let Some(outcome) = self.parts.part_done(*sequence, outcome) {
                if let Some(notice) = text_result_notice(*sequence, &outcome) {
                    send_notice(self.notices, &notice);
                }
            }
        }
        let Some(generation) = reply.keymap_generation else {
            return Ok(AfterReply::Continue);
        };
        if generation <= *self.keymap_seen {
            return Ok(AfterReply::Continue);
        }
        *self.keymap_seen = generation;
        let layout = connection
            .request(&InputCommand::Keymap, COMMAND_TIMEOUT)
            .await?;
        if is_superseded(&layout) {
            return Ok(AfterReply::Superseded);
        }
        if let Some(keymap) = layout.keymap {
            tracing::info!(
                target: target::INPUT,
                generation = keymap.generation,
                layouts = ?keymap.layouts,
                active_layout = ?keymap.active_layout,
                group = keymap.group,
                positions = keymap.keys.len(),
                groups = keymap.groups,
                symbols_section = ?keymap.symbols_section,
                group_names = ?keymap.group_names,
                input_sources = ?keymap.input_sources,
                "session keyboard layout read"
            );
            send_notice(self.notices, &host_keymap_notice(&keymap));
        }
        Ok(AfterReply::Continue)
    }
}

async fn serve(
    connection: &mut SessionConnection,
    queue: &InjectionQueue,
    refused: &mut u64,
    text: &mut TextNotices<'_>,
) -> Served {
    let mut drain_deadline = None;
    loop {
        let next = async {
            match drain_deadline {
                Some(deadline) => tokio::time::timeout_at(deadline, queue.next())
                    .await
                    .unwrap_or(None),
                None => queue.next().await,
            }
        };
        let command = tokio::select! {
            biased;
            command = next => command,
            readable = connection.reader.readable() => {
                return match readable {
                    Ok(true) => match connection.read_reply().await {
                        Ok(notice) if is_superseded(&notice) => Served::Superseded,
                        Ok(_) => Served::Broken(WaylandInputError::MalformedReply),
                        Err(error) => Served::Broken(error),
                    },
                    Ok(false) => Served::Broken(WaylandInputError::Closed),
                    Err(error) => Served::Broken(error.into()),
                };
            }
        };
        let Some(command) = command else {
            return Served::Closed;
        };
        if drain_deadline.is_none() && queue.is_closed() {
            drain_deadline = Some(tokio::time::Instant::now() + DRAIN_BUDGET);
        }
        let budget = drain_deadline.map_or_else(
            || command_budget(&command),
            |deadline| deadline.saturating_duration_since(tokio::time::Instant::now()),
        );
        let replied = tokio::time::timeout(budget, connection.request(&command, budget)).await;
        if let Ok(Ok(reply)) = &replied {
            if !is_superseded(reply) {
                match text.after_reply(connection, &command, reply).await {
                    Ok(AfterReply::Continue) => {}
                    Ok(AfterReply::Superseded) => return Served::Superseded,
                    Err(error) => return Served::Broken(error),
                }
            }
        }
        match replied {
            Ok(Ok(reply)) if reply.ok => {}
            Ok(Ok(reply)) if is_superseded(&reply) => return Served::Superseded,
            Ok(Ok(reply)) => {
                *refused = refused.saturating_add(1);
                if refused.is_power_of_two() {
                    tracing::warn!(
                        target: target::INPUT,
                        refused = *refused,
                        reason = reply.error.as_deref().unwrap_or_default(),
                        "the Wayland session refused input"
                    );
                }
            }
            Ok(Err(error)) => return Served::Broken(error),
            Err(_) if drain_deadline.is_some() => return Served::Closed,
            Err(_) => return Served::Broken(WaylandInputError::Timeout),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream() -> ReadyStream {
        ReadyStream {
            connector: "Meta-0".into(),
            node_id: 44,
            width: 1920,
            height: 1080,
            x: 0,
            y: 0,
        }
    }

    fn target() -> WaylandInputTarget {
        WaylandInputTarget::for_tests(
            PathBuf::from("/nonexistent/arcen-input.sock"),
            1000,
            None,
            1,
            vec![stream()],
        )
    }

    fn planner() -> WaylandInputPlanner {
        WaylandInputPlanner::new(&target()).unwrap()
    }

    fn facts() -> InputSessionFacts {
        InputSessionFacts {
            generation: 1,
            streams: vec![stream()],
            device_types: 7,
            remote_desktop_started: true,
            backend: InputBackendKind::RemoteDesktopDbus,
            eis_offered: Some(false),
            stylus: None,
            connection: 1,
            text: None,
        }
    }

    fn key_message(scan_code: u32, pressed: bool, modifiers: u32) -> KeyEventMsg {
        KeyEventMsg {
            scan_code,
            pressed,
            modifiers,
            ..KeyEventMsg::default()
        }
    }

    #[test]
    fn text_results_reach_the_deck_only_when_something_needs_saying() {
        let typed = TextOutcome {
            keymap_chars: 3,
            keymap_generation: 1,
            group: "single_group".to_owned(),
            ..TextOutcome::default()
        };
        assert_eq!(text_result_notice(5, &typed), None);
        let refused = TextOutcome {
            keymap_chars: 1,
            refused_chars: 1,
            refused_text: "😀".to_owned(),
            reason: Some(TextRefusalReason::CompositorLacksTextInput),
            ..typed.clone()
        };
        let notice = text_result_notice(5, &refused).unwrap();
        assert_eq!(notice.sequence, 5);
        assert_eq!(notice.delivered_chars, 1);
        assert_eq!(notice.refused_text, "😀");
        assert_eq!(notice.reason, TextRefusalReason::CompositorLacksTextInput);
        assert!(!notice.unverified);
        let unverified = TextOutcome {
            unverified_chars: 2,
            reason: Some(TextRefusalReason::KeymapUnknown),
            ..typed
        };
        assert!(text_result_notice(6, &unverified).unwrap().unverified);
        let early =
            text_refusal_notice(&TextCommitMsg::new("æø", 9), TextRefusalReason::RateLimited);
        assert_eq!((early.sequence, early.refused_chars), (9, 2));
    }

    #[test]
    fn the_session_layout_is_described_by_deck_key_positions() {
        use crate::session::wayland::text::KeyPositionChars;
        let keymap = SessionKeymap {
            generation: 2,
            layouts: vec!["no".to_owned()],
            active_layout: Some("no".to_owned()),
            group: "single_group".to_owned(),
            groups: 1,
            symbols_section: None,
            group_names: Vec::new(),
            input_sources: None,
            keys: vec![
                KeyPositionChars {
                    evdev: 40,
                    base: Some("æ".to_owned()),
                    shift: Some("Æ".to_owned()),
                },
                KeyPositionChars {
                    evdev: 30,
                    base: Some("a".to_owned()),
                    shift: Some("A".to_owned()),
                },
            ],
        };
        let notice = host_keymap_notice(&keymap);
        assert_eq!(notice.generation, 2);
        assert_eq!(notice.active_layout.as_deref(), Some("no"));
        assert!(notice.keys.contains(&HostKeyCharsMsg {
            qt_key: 0x27,
            base: Some("æ".to_owned()),
            shift: Some("Æ".to_owned()),
        }));
        assert!(notice.keys.contains(&HostKeyCharsMsg {
            qt_key: 0x41,
            base: Some("a".to_owned()),
            shift: Some("A".to_owned()),
        }));
        assert_eq!(notice.keys.len(), 2);
    }

    #[test]
    fn keys_go_by_evdev_code_with_modifiers_reconciled_first() {
        let mut planner = planner();
        let plan = planner.key_event(&key_message(0x41, true, super::super::keymap::MOD_CTRL));
        assert_eq!(plan.commands, vec![key(29, true), key(30, true)]);
        assert!(!plan.unmapped);
        // A Deck autorepeat of a held key emits nothing: the compositor's
        // clients repeat by themselves.
        assert!(planner
            .key_event(&key_message(0x41, true, super::super::keymap::MOD_CTRL))
            .commands
            .is_empty());
        assert_eq!(
            planner.key_event(&key_message(0x41, false, 0)).commands,
            vec![key(29, false), key(30, false)]
        );
    }

    #[test]
    fn latin1_keys_without_a_code_fall_back_to_their_keysym() {
        let mut planner = planner();
        // Qt::Key_AE: the keysym is the small letter; Shift decides the case.
        let press = planner.key_event(&key_message(0xC6, true, 0));
        assert_eq!(
            press.commands,
            vec![InputCommand::Keysym {
                keysym: 0xE6,
                pressed: true
            }]
        );
        assert!(planner
            .key_event(&key_message(0xC6, true, 0))
            .commands
            .is_empty());
        assert_eq!(
            planner.key_event(&key_message(0xC6, false, 0)).commands,
            vec![InputCommand::Keysym {
                keysym: 0xE6,
                pressed: false
            }]
        );
        let unmapped = planner.key_event(&key_message(0x0100_FFFF, true, 0));
        assert!(unmapped.unmapped);
        assert!(unmapped.commands.is_empty());
        assert_eq!(latin1_keysym(0xD7), Some(0xD7));
        assert_eq!(latin1_keysym(0x0100_0000), None);
    }

    #[test]
    fn lock_states_are_sent_on_change_and_again_once_unknown() {
        let mut planner = planner();
        let locks = |caps_lock| InputCommand::Locks {
            caps_lock,
            num_lock: None,
            scroll_lock: None,
        };
        let message = KeyEventMsg {
            caps_lock_on: Some(true),
            ..key_message(0x41, true, 0)
        };
        assert_eq!(
            planner.key_event(&message).commands,
            vec![locks(Some(true)), key(30, true)]
        );
        let again = KeyEventMsg {
            caps_lock_on: Some(true),
            ..key_message(0x41, false, 0)
        };
        assert_eq!(planner.key_event(&again).commands, vec![key(30, false)]);
        // A collapse or reconnection may have dropped the lock command: the
        // state is unknown, and the next key event sends it again for the
        // session to reconcile against what its compositor has.
        planner.forget_held();
        assert_eq!(planner.lock_state, [None; 3]);
        assert_eq!(
            planner.key_event(&message).commands,
            vec![locks(Some(true)), key(30, true)]
        );
        // The Caps Lock key itself: no reconciliation of Caps Lock before
        // it (that would toggle twice), and its state is unknown after.
        let caps_key = KeyEventMsg {
            caps_lock_on: Some(false),
            num_lock_on: Some(true),
            ..key_message(0x0100_0024, true, 0)
        };
        assert_eq!(
            planner.key_event(&caps_key).commands,
            vec![
                InputCommand::Locks {
                    caps_lock: None,
                    num_lock: Some(true),
                    scroll_lock: None
                },
                key(58, true)
            ]
        );
        assert_eq!(planner.lock_state[0], None);
        // A Deck that cannot observe a lock never sends it.
        let unobserved = key_message(0x42, true, 0);
        assert_eq!(planner.key_event(&unobserved).commands, vec![key(48, true)]);
    }

    #[test]
    fn keyboard_reset_releases_keys_and_keysyms_but_not_buttons() {
        let mut planner = planner();
        planner.key_event(&key_message(0x41, true, super::super::keymap::MOD_SHIFT));
        planner.key_event(&key_message(0xC6, true, super::super::keymap::MOD_SHIFT));
        planner
            .mouse_button(&MouseButtonMsg {
                msg_type: "mouse_button".into(),
                x: 0.5,
                y: 0.5,
                button: 1,
                pressed: true,
                ..MouseButtonMsg::default()
            })
            .unwrap();
        assert_eq!(
            planner.reset_keyboard(),
            vec![
                key(30, false),
                key(42, false),
                InputCommand::Keysym {
                    keysym: 0xE6,
                    pressed: false
                }
            ]
        );
        assert!(planner.reset_keyboard().is_empty());
        assert_eq!(planner.held_buttons.len(), 1);
        assert_eq!(planner.reset_all(), InputCommand::ReleaseAll);
        assert!(planner.held_buttons.is_empty());
    }

    fn two_outputs() -> Vec<ReadyStream> {
        vec![
            ReadyStream {
                connector: "Meta-1".into(),
                node_id: 44,
                width: 1920,
                height: 1080,
                x: 1280,
                y: 0,
            },
            ReadyStream {
                connector: "Meta-2".into(),
                node_id: 45,
                width: 1280,
                height: 1024,
                x: 0,
                y: 0,
            },
        ]
    }

    #[test]
    fn desktop_positions_land_on_the_output_that_shows_them() {
        let streams = two_outputs();
        let target = WaylandInputTarget::for_tests(
            PathBuf::from("/nonexistent/arcen-input.sock"),
            1000,
            None,
            4,
            streams.clone(),
        );
        let mut planner = WaylandInputPlanner::new(&target).unwrap();
        assert_eq!(
            planner.desktop_position(1280, 10).unwrap(),
            InputCommand::PointerAbsolute {
                output: "Meta-1".into(),
                generation: 4,
                x: 0.0,
                y: 10.0
            }
        );
        assert_eq!(
            planner.desktop_position(1279, 1023).unwrap(),
            InputCommand::PointerAbsolute {
                output: "Meta-2".into(),
                generation: 4,
                x: 1279.0,
                y: 1023.0
            }
        );
        // Below the shorter left output: on no output.
        assert!(matches!(
            planner.desktop_position(100, 1050),
            Err(InputError::InvalidCoordinate)
        ));
        // A normalised position spans the whole desktop.
        let right_edge = planner
            .mouse_move(&MouseMoveMsg {
                msg_type: "mouse_move".into(),
                x: 1.0,
                y: 0.5,
                ..MouseMoveMsg::default()
            })
            .unwrap();
        assert!(matches!(
            &right_edge,
            InputCommand::PointerAbsolute { output, x, .. } if output == "Meta-1" && *x == 1919.0
        ));
        // Every coordinate produced is one the session accepts.
        for command in [planner.desktop_position(3199, 1079).unwrap(), right_edge] {
            assert_eq!(
                crate::session::wayland::validate_input(&command, &streams, 4),
                Ok(())
            );
        }
        // A button names its position first, then the edge, once.
        let press = arcen_input::MappedRegionButton {
            position: super::super::region_adapter::XorgAxisPoint { x: 1300, y: 20 },
            button: 1,
            pressed: true,
        };
        let commands = planner.region_button(&press).unwrap();
        assert_eq!(commands.len(), 2);
        assert!(matches!(
            commands[1],
            InputCommand::PointerButton { pressed: true, .. }
        ));
        assert_eq!(planner.region_button(&press).unwrap().len(), 1);
    }

    #[test]
    fn normalised_positions_map_to_the_primary_stream_with_its_generation() {
        let planner = planner();
        let at = |x, y| {
            planner.mouse_move(&MouseMoveMsg {
                msg_type: "mouse_move".into(),
                x,
                y,
                ..MouseMoveMsg::default()
            })
        };
        assert_eq!(
            at(0.5, 1.0).unwrap(),
            InputCommand::PointerAbsolute {
                output: "Meta-0".into(),
                generation: 1,
                x: 959.5,
                y: 1079.0
            }
        );
        assert_eq!(
            at(-3.0, 7.0).unwrap(),
            InputCommand::PointerAbsolute {
                output: "Meta-0".into(),
                generation: 1,
                x: 0.0,
                y: 1079.0
            }
        );
        assert!(matches!(
            at(f64::NAN, 0.0),
            Err(InputError::InvalidCoordinate)
        ));
        // The session accepts every coordinate the planner produces.
        let command = at(1.0, 1.0).unwrap();
        assert_eq!(
            crate::session::wayland::validate_input(&command, &[stream()], 1),
            Ok(())
        );
    }

    #[test]
    fn buttons_carry_their_position_and_are_idempotent() {
        let mut planner = planner();
        let press = MouseButtonMsg {
            msg_type: "mouse_button".into(),
            x: 0.0,
            y: 0.0,
            button: 3,
            pressed: true,
            ..MouseButtonMsg::default()
        };
        let commands = planner.mouse_button(&press).unwrap();
        assert_eq!(commands.len(), 2);
        assert_eq!(
            commands[1],
            InputCommand::PointerButton {
                button: 0x111,
                pressed: true
            }
        );
        assert_eq!(
            planner.mouse_button(&press).unwrap().len(),
            1,
            "position only"
        );
        let relative = MouseButtonMsg {
            pressed: false,
            motion_mode: PointerMotionMode::Relative,
            ..press
        };
        assert_eq!(
            planner.mouse_button(&relative).unwrap(),
            vec![InputCommand::PointerButton {
                button: 0x111,
                pressed: false
            }]
        );
        assert!(matches!(
            planner.mouse_button(&MouseButtonMsg {
                button: 9,
                ..relative
            }),
            Err(InputError::InvalidButton(9))
        ));
    }

    #[test]
    fn line_scroll_becomes_detents_and_point_scroll_continuous_units() {
        let mut planner = planner();
        let scroll = |dx, dy, unit| MouseScrollMsg {
            dx,
            dy,
            unit,
            motion_mode: PointerMotionMode::Relative,
            ..MouseScrollMsg::default()
        };
        use arcen_protocol::messages::ScrollUnitMsg;
        assert_eq!(
            planner
                .mouse_scroll(&scroll(-1.0, 2.0, ScrollUnitMsg::Line))
                .unwrap(),
            vec![
                InputCommand::PointerAxisDiscrete { axis: 0, steps: -2 },
                InputCommand::PointerAxisDiscrete { axis: 1, steps: -1 }
            ]
        );
        assert_eq!(
            planner
                .mouse_scroll(&scroll(0.0, 60.0, ScrollUnitMsg::Point))
                .unwrap(),
            vec![InputCommand::PointerAxis {
                dx: 0.0,
                dy: -5.0,
                finish: false,
                source: AxisSource::Continuous
            }]
        );
        assert!(planner
            .mouse_scroll(&scroll(0.0, 0.0, ScrollUnitMsg::Point))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn pen_samples_map_to_the_primary_stream_and_only_motion_coalesces() {
        let planner = planner();
        let sample = |x, touching, pressure| {
            planner
                .pen(&PenEventMsg {
                    x,
                    y: 0.5,
                    pressure,
                    tilt_x_degrees: 30.0,
                    rotation_degrees: 90.0,
                    tool: PenToolMsg::Eraser,
                    in_proximity: true,
                    touching,
                    buttons: 0b10,
                    ..PenEventMsg::default()
                })
                .unwrap()
        };
        let down = sample(0.5, true, 0.4);
        assert_eq!(
            down,
            InputCommand::Pen {
                output: "Meta-0".into(),
                generation: 1,
                x: 959.5,
                y: 539.5,
                eraser: true,
                in_proximity: true,
                touching: true,
                buttons: 0b10,
                pressure: 0.4,
                tilt_x: 30.0,
                tilt_y: 0.0,
                rotation: 90.0,
            }
        );
        assert_eq!(
            crate::session::wayland::validate_input(&down, &[stream()], 1),
            Ok(())
        );
        // An out-of-proximity sample holds nothing, whatever it claims.
        let away = planner
            .pen(&PenEventMsg {
                in_proximity: false,
                touching: true,
                buttons: 0b11,
                ..PenEventMsg::default()
            })
            .unwrap();
        assert!(matches!(
            away,
            InputCommand::Pen {
                touching: false,
                buttons: 0,
                ..
            }
        ));

        let queue = InjectionQueue::new(16);
        for command in [
            sample(0.1, false, 0.0),
            sample(0.2, false, 0.0),
            sample(0.3, true, 0.5),
            sample(0.4, true, 0.6),
            sample(0.5, false, 0.0),
        ] {
            queue.push(command).unwrap();
        }
        // Hover motion merged, the contact edge kept, contact motion merged,
        // the lift kept.
        assert_eq!(queue.len(), 3);
    }

    #[test]
    fn motion_coalesces_but_edges_are_kept_in_order() {
        let queue = InjectionQueue::new(16);
        let at = |x| InputCommand::PointerAbsolute {
            output: "Meta-0".into(),
            generation: 1,
            x,
            y: 1.0,
        };
        let relative = |dx| InputCommand::PointerRelative { dx, dy: 1.0 };
        for command in [
            at(1.0),
            at(2.0),
            InputCommand::PointerButton {
                button: 272,
                pressed: true,
            },
            at(3.0),
            relative(1.0),
            relative(2.0),
            at(4.0),
            InputCommand::PointerButton {
                button: 272,
                pressed: false,
            },
        ] {
            assert_eq!(queue.push(command), Ok(Enqueued::Queued));
        }
        let mut drained = Vec::new();
        while let Some(command) = queue.lock().pending.pop_front() {
            drained.push(command);
        }
        assert_eq!(
            drained,
            vec![
                at(2.0),
                InputCommand::PointerButton {
                    button: 272,
                    pressed: true
                },
                at(3.0),
                InputCommand::PointerRelative { dx: 3.0, dy: 2.0 },
                at(4.0),
                InputCommand::PointerButton {
                    button: 272,
                    pressed: false
                },
            ]
        );
        assert_eq!(queue.counters(), (2, 0));
    }

    #[test]
    fn motion_of_another_generation_is_never_merged() {
        let queue = InjectionQueue::new(4);
        for generation in [1, 2] {
            queue
                .push(InputCommand::PointerAbsolute {
                    output: "Meta-0".into(),
                    generation,
                    x: 1.0,
                    y: 1.0,
                })
                .unwrap();
        }
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn a_full_queue_collapses_into_one_release_of_everything() {
        let queue = InjectionQueue::new(3);
        for keycode in [30, 31, 32] {
            queue
                .push(InputCommand::Key {
                    keycode,
                    pressed: true,
                })
                .unwrap();
        }
        assert_eq!(
            queue.push(InputCommand::Key {
                keycode: 30,
                pressed: false
            }),
            Ok(Enqueued::Collapsed)
        );
        assert_eq!(queue.len(), 1);
        assert_eq!(
            queue.lock().pending.front(),
            Some(&InputCommand::ReleaseAll)
        );
        queue.close();
        assert_eq!(
            queue.push(InputCommand::ReleaseAll),
            Err(QueueRefusal::Closed),
            "a closed queue accepts nothing"
        );
    }

    #[test]
    fn a_broken_connection_drops_the_queue_and_refuses_stale_plans() {
        let queue = InjectionQueue::new(8);
        let epoch = queue.epoch();
        queue
            .push_at(
                epoch,
                InputCommand::Key {
                    keycode: 30,
                    pressed: true,
                },
            )
            .unwrap();
        assert_eq!(queue.interrupt(), 1, "the press waiting is dropped");
        assert!(queue.is_empty());
        assert_ne!(queue.epoch(), epoch);
        assert_eq!(
            queue.push_at(epoch, InputCommand::ReleaseAll),
            Err(QueueRefusal::Stale)
        );
        assert_eq!(
            queue.push_at(queue.epoch(), InputCommand::ReleaseAll),
            Err(QueueRefusal::Interrupted)
        );
        queue.resume();
        assert_eq!(
            queue.push_at(queue.epoch(), InputCommand::ReleaseAll),
            Ok(Enqueued::Queued)
        );
        assert_eq!(
            queue.push_at(epoch, InputCommand::ReleaseAll),
            Err(QueueRefusal::Stale),
            "a plan from before the break never reaches the new connection"
        );
    }

    #[test]
    fn committed_text_gets_time_for_every_character() {
        let part = InputCommand::Text {
            text: "æ".repeat(arcen_input::text::MAX_TEXT_PART_CHARS),
            sequence: 1,
        };
        assert_eq!(
            command_budget(&part),
            COMMAND_TIMEOUT + TEXT_BUDGET_PER_CHAR * 64
        );
        assert_eq!(command_budget(&InputCommand::ReleaseAll), COMMAND_TIMEOUT);
    }

    #[test]
    fn a_parted_commit_is_reported_once_after_its_last_part() {
        let parts = TextParts::default();
        let refused = |refused_chars| TextOutcome {
            keymap_chars: 1,
            refused_chars,
            ..TextOutcome::default()
        };
        parts.expect(7, 2);
        assert_eq!(parts.part_done(7, &refused(1)), None);
        let whole = parts.part_done(7, &refused(2)).unwrap();
        assert_eq!((whole.keymap_chars, whole.refused_chars), (2, 3));
        // A commit typed in one part, or one the injector never split.
        assert_eq!(parts.part_done(7, &refused(0)), Some(refused(0)));
        parts.expect(8, 3);
        parts.clear();
        assert_eq!(parts.part_done(8, &refused(0)), Some(refused(0)));
    }

    #[tokio::test]
    async fn a_superseded_answer_to_the_layout_request_ends_delivery() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let (pier, agent) = tokio::net::UnixStream::pair().unwrap();
        let agent = tokio::spawn(async move {
            let (read, mut write) = agent.into_split();
            let mut lines = tokio::io::BufReader::new(read).lines();
            // The key, whose reply says the layout changed.
            lines.next_line().await.unwrap().unwrap();
            write
                .write_all(b"{\"ok\":true,\"keymap_generation\":1}\n")
                .await
                .unwrap();
            // The layout request, answered by a newer connection's arrival.
            let request = lines.next_line().await.unwrap().unwrap();
            assert!(request.contains("\"keymap\""), "{request}");
            let notice = serde_json::to_string(&InputReply {
                ok: false,
                error: Some(SUPERSEDED_REASON.to_owned()),
                ..InputReply::accepted()
            })
            .unwrap();
            write
                .write_all(format!("{notice}\n").as_bytes())
                .await
                .unwrap();
        });
        let mut connection = SessionConnection::new(pier);
        let queue = InjectionQueue::new(4);
        queue
            .push(InputCommand::Key {
                keycode: 30,
                pressed: true,
            })
            .unwrap();
        let (notices, _receiver) = tokio::sync::mpsc::channel(4);
        let mut keymap_seen = 0;
        let parts = TextParts::default();
        let mut text = TextNotices {
            notices: &notices,
            keymap_seen: &mut keymap_seen,
            parts: &parts,
        };
        let served = tokio::time::timeout(
            Duration::from_secs(5),
            serve(&mut connection, &queue, &mut 0, &mut text),
        )
        .await
        .unwrap();
        assert!(matches!(served, Served::Superseded));
        agent.await.unwrap();
    }

    #[tokio::test]
    async fn a_closed_queue_still_delivers_what_it_holds() {
        let queue = InjectionQueue::new(4);
        queue.push(InputCommand::ReleaseAll).unwrap();
        queue.close();
        assert_eq!(queue.next().await, Some(InputCommand::ReleaseAll));
        assert_eq!(queue.next().await, None);
    }

    #[test]
    fn pen_truth_comes_only_from_a_bound_stylus() {
        use crate::session::wayland::StylusFacts;
        use arcen_input::pen_emulation::StylusBinding;
        let target = target();
        let proof = |stylus, eis_offered| {
            WaylandInputProof::verify(
                &target,
                InputSessionFacts {
                    stylus,
                    eis_offered,
                    ..facts()
                },
            )
            .unwrap()
        };
        // An agent that predates the probe, on a compositor with EIS.
        let old = proof(None, Some(true));
        assert!(!old.pen_capabilities().pen_available());
        assert_eq!(old.pen_unavailable(), Some(StylusUnavailable::NotProbed));
        assert_eq!(
            proof(None, Some(false)).pen_unavailable(),
            Some(StylusUnavailable::NoEmulatedInput)
        );
        // Mutter 49: EIS without a stylus.
        let mutter = proof(
            Some(StylusFacts::unavailable(
                StylusUnavailable::StylusNotOffered,
            )),
            Some(true),
        );
        assert!(!mutter.pen_capabilities().pen_available());
        assert_eq!(
            mutter.pen_unavailable(),
            Some(StylusUnavailable::StylusNotOffered)
        );
        // A bound stylus advertises exactly what it bound.
        let bound = proof(
            Some(StylusFacts {
                binding: Some(StylusBinding {
                    pressure: true,
                    tilt: true,
                    rotation: true,
                    erase: true,
                    barrel_buttons: 2,
                }),
                unavailable: None,
                ei_stylus_version: Some(1),
                seat_interfaces: vec!["ei_stylus".into()],
            }),
            Some(true),
        );
        assert!(bound.pen_capabilities().pen_available());
        assert_eq!(bound.pen_unavailable(), None);
    }

    #[test]
    fn hello_facts_must_match_the_started_session() {
        let target = target();
        let proof = WaylandInputProof::verify(&target, facts()).unwrap();
        let capabilities = proof.capabilities();
        assert_eq!(capabilities.keyboard, CapabilityFact::Available);
        assert_eq!(capabilities.pointer, CapabilityFact::Available);
        assert_eq!(capabilities.touch, CapabilityFact::Available);
        assert_eq!(capabilities.pen, CapabilityFact::Missing);
        assert_eq!(capabilities.eis, CapabilityFact::Missing);
        let host = WaylandCapabilityFacts::default();
        let proven = proof.proven_facts(host);
        assert_eq!(proven.input_injection, CapabilityFact::Available);
        assert_eq!(proven.gpu_import, CapabilityFact::Unknown);

        let stale = InputSessionFacts {
            generation: 2,
            ..facts()
        };
        assert!(matches!(
            WaylandInputProof::verify(&target, stale),
            Err(WaylandInputError::FactsMismatch("output generation"))
        ));
        let mut moved = facts();
        moved.streams[0].width = 1280;
        assert!(WaylandInputProof::verify(&target, moved).is_err());
        let pointer_only = InputSessionFacts {
            device_types: device_type::POINTER,
            ..facts()
        };
        assert!(WaylandInputProof::verify(&target, pointer_only).is_err());
        let eis = InputSessionFacts {
            backend: InputBackendKind::Eis,
            ..facts()
        };
        assert!(WaylandInputProof::verify(&target, eis).is_err());
    }

    #[test]
    fn a_target_must_be_the_sessions_private_runtime_dir() {
        let runtime_dir = crate::session::wayland::private_runtime_dir(1000, "c7");
        let ready = WaylandAgentReady {
            compositor: "gnome-shell".into(),
            input_socket: runtime_dir.join(crate::session::wayland::INPUT_SOCKET_NAME),
            runtime_dir,
            wayland_display: crate::session::wayland::WAYLAND_DISPLAY_NAME.into(),
            outputs: vec![crate::session::wayland::ReadyOutput {
                connector: "Meta-0".into(),
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            }],
            streams: vec![stream()],
            input_device_types: 7,
            generation: 1,
            compositor_render_node: None,
            compositor_gpu_pci: None,
            compositor_gpu_vendor: None,
            output_mode: arcen_session::wayland_outputs::VirtualOutputMode::StartupMonitors,
            output_socket: None,
            x11: None,
            cursor_mode: Default::default(),
            clipboard: None,
        };
        let target = WaylandInputTarget::from_session(&ready, 1000, "c7", 4242).unwrap();
        assert_eq!(target.agent_pid, Some(4242));
        assert_eq!(target.generation(), 1);
        assert!(matches!(
            WaylandInputTarget::from_session(&ready, 1001, "c7", 4242),
            Err(WaylandInputError::RuntimeDir)
        ));
        assert!(matches!(
            WaylandInputTarget::from_session(&ready, 1000, "c8", 4242),
            Err(WaylandInputError::RuntimeDir)
        ));
        let mut inconsistent = ready;
        inconsistent.streams.clear();
        assert!(matches!(
            WaylandInputTarget::from_session(&inconsistent, 1000, "c7", 4242),
            Err(WaylandInputError::Inconsistent)
        ));
    }

    mod reconnection {
        use std::sync::Mutex as StdMutex;

        use super::*;
        use crate::session::wayland::input::{CompositorInput, InputService};

        #[derive(Default)]
        struct Recording(StdMutex<Vec<InputCommand>>);

        impl CompositorInput for Recording {
            type Error = std::convert::Infallible;

            fn notify(
                &self,
                command: &InputCommand,
            ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
                self.0.lock().unwrap().push(command.clone());
                std::future::ready(Ok(()))
            }
        }

        impl Recording {
            fn has(&self, wanted: &InputCommand) -> bool {
                self.0.lock().unwrap().contains(wanted)
            }
        }

        struct Session {
            dir: PathBuf,
            socket: PathBuf,
            uid: u32,
            service: Option<Arc<InputService>>,
            serving: Option<tokio::task::JoinHandle<()>>,
            backend: Arc<Recording>,
        }

        impl Session {
            fn new(name: &str) -> Self {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                let dir = std::env::current_exe()
                    .unwrap()
                    .with_file_name(format!("arcen-wayland-{name}-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir(&dir).unwrap();
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
                let uid = std::fs::metadata(&dir).unwrap().uid();
                let socket = dir.join(crate::session::wayland::INPUT_SOCKET_NAME);
                let service = Arc::new(InputService::bind(&socket, uid).unwrap());
                let mut session = Self {
                    dir,
                    socket,
                    uid,
                    service: Some(service),
                    serving: None,
                    backend: Arc::new(Recording::default()),
                };
                session.serve();
                session
            }

            fn serve(&mut self) {
                let service = Arc::clone(self.service.as_ref().unwrap());
                let backend = Arc::clone(&self.backend);
                self.serving = Some(tokio::spawn(async move {
                    service.serve(&*backend, &facts().into()).await;
                }));
            }

            /// The agent stops serving: every connection it held closes.
            fn break_connections(&mut self) {
                if let Some(serving) = self.serving.take() {
                    serving.abort();
                }
            }

            /// The session is gone: no socket, nobody listening.
            fn end(&mut self) {
                self.break_connections();
                self.service = None;
                let _ = std::fs::remove_file(&self.socket);
            }

            fn target(&self) -> WaylandInputTarget {
                WaylandInputTarget::for_tests(
                    self.socket.clone(),
                    self.uid,
                    Some(std::process::id()),
                    1,
                    vec![stream()],
                )
            }
        }

        impl Drop for Session {
            fn drop(&mut self) {
                self.end();
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }

        const FAST: [Duration; 3] = [
            Duration::from_millis(50),
            Duration::from_millis(100),
            Duration::from_millis(100),
        ];

        async fn wait_status(
            injector: &WaylandInjector,
            wanted: impl Fn(&InjectorStatus) -> bool,
        ) -> InjectorStatus {
            let mut status = injector.status();
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let current = status.borrow_and_update().clone();
                    if wanted(&current) {
                        return current;
                    }
                    status.changed().await.unwrap();
                }
            })
            .await
            .expect("status in time")
        }

        async fn eventually(check: impl Fn() -> bool) {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !check() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("in time");
        }

        fn press(scan_code: u32) -> KeyEventMsg {
            key_message(scan_code, true, 0)
        }

        #[tokio::test]
        async fn a_broken_connection_is_restored_and_proven_again() {
            let mut session = Session::new("restore");
            let (mut injector, stats) =
                WaylandInjector::connect_with_backoff(session.target(), FAST.to_vec())
                    .await
                    .unwrap();
            injector
                .key_event(&KeyEventMsg {
                    caps_lock_on: Some(true),
                    ..press(0x41)
                })
                .unwrap();
            eventually(|| session.backend.has(&key(30, true))).await;

            session.break_connections();
            wait_status(&injector, |status| {
                matches!(status, InjectorStatus::Reconnecting { .. })
            })
            .await;
            assert!(matches!(
                injector.key_event(&press(0x42)),
                Err(InputError::InjectorReconnecting)
            ));
            assert_eq!(stats.unavailable_drops(), 1);
            session.serve();
            wait_status(&injector, |status| {
                matches!(status, InjectorStatus::Connected { .. })
            })
            .await;

            // Restored: the mirror was forgotten, so the held 'a' (which
            // the session released with the broken connection) is not
            // released again, the lock state is sent again, and input flows.
            injector
                .key_event(&KeyEventMsg {
                    caps_lock_on: Some(true),
                    ..press(0x43)
                })
                .unwrap();
            eventually(|| session.backend.has(&key(46, true))).await;
            let recorded = session.backend.0.lock().unwrap().clone();
            let caps_toggles = recorded
                .iter()
                .filter(|command| **command == key(58, true))
                .count();
            assert_eq!(caps_toggles, 1, "the session reconciled, not toggled twice");
            assert!(
                !recorded.contains(&key(48, true)),
                "nothing typed while down arrives late"
            );
            assert!(!injector.is_lost());
        }

        #[tokio::test]
        async fn a_long_commit_is_typed_in_parts_and_reported_once() {
            let session = Session::new("parts");
            let (mut injector, _) =
                WaylandInjector::connect_with_backoff(session.target(), FAST.to_vec())
                    .await
                    .unwrap();
            let mut notices = injector.take_text_notices().unwrap();
            let text = "a".repeat(150);
            injector
                .text_commit(&TextCommitMsg {
                    text: text.clone(),
                    sequence: 9,
                    ..TextCommitMsg::default()
                })
                .unwrap();
            // No keymap in this session: typed by keysym, unverified, which
            // the Deck is told once for the whole commit.
            let notice = tokio::time::timeout(Duration::from_secs(10), notices.recv())
                .await
                .unwrap()
                .unwrap();
            let result: TextCommitResultMsg = serde_json::from_str(&notice).unwrap();
            assert_eq!(result.sequence, 9);
            assert_eq!(result.delivered_chars + result.refused_chars, 150);
            assert!(
                tokio::time::timeout(Duration::from_millis(300), notices.recv())
                    .await
                    .is_err()
            );
            assert!(!injector.is_lost());
        }

        /// Answers every connection's first request with the superseded
        /// notice, as the session does when a newer connection arrived
        /// first.
        fn answer_superseded(socket: &std::path::Path) -> tokio::task::JoinHandle<()> {
            use std::os::unix::fs::PermissionsExt;
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            let listener = tokio::net::UnixListener::bind(socket).unwrap();
            std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)).unwrap();
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let (read, mut write) = stream.into_split();
                    let mut lines = tokio::io::BufReader::new(read).lines();
                    if lines.next_line().await.ok().flatten().is_none() {
                        continue;
                    }
                    let notice = serde_json::to_string(&InputReply {
                        ok: false,
                        error: Some(SUPERSEDED_REASON.to_owned()),
                        ..InputReply::accepted()
                    })
                    .unwrap();
                    let _ = write.write_all(format!("{notice}\n").as_bytes()).await;
                }
            })
        }

        #[tokio::test]
        async fn a_hello_answered_as_superseded_is_not_retried() {
            let mut session = Session::new("hello-superseded");
            let (injector, _) =
                WaylandInjector::connect_with_backoff(session.target(), FAST.to_vec())
                    .await
                    .unwrap();
            // The connection breaks; by the time the injector reconnects, a
            // newer connection owns the input.
            session.end();
            let newer = answer_superseded(&session.socket);
            let opened = SessionConnection::open(&session.target()).await;
            assert!(
                matches!(opened, Err(WaylandInputError::Superseded)),
                "{:?}",
                opened.err()
            );
            let status = wait_status(&injector, |status| {
                matches!(status, InjectorStatus::Lost(_))
            })
            .await;
            assert_eq!(status, InjectorStatus::Lost(InjectorLoss::Superseded));
            newer.abort();
        }

        #[tokio::test]
        async fn a_superseded_injector_does_not_take_the_input_back() {
            let session = Session::new("superseded");
            let (mut injector, _) =
                WaylandInjector::connect_with_backoff(session.target(), FAST.to_vec())
                    .await
                    .unwrap();
            let (newer, _) = WaylandInjector::connect_with_backoff(session.target(), FAST.to_vec())
                .await
                .unwrap();
            let status = wait_status(&injector, |status| {
                matches!(status, InjectorStatus::Lost(_))
            })
            .await;
            assert_eq!(status, InjectorStatus::Lost(InjectorLoss::Superseded));
            assert!(matches!(
                injector.key_event(&press(0x41)),
                Err(InputError::InjectorLost)
            ));
            tokio::time::sleep(Duration::from_millis(400)).await;
            assert!(
                matches!(*newer.status().borrow(), InjectorStatus::Connected { .. }),
                "the newer injector keeps the input"
            );
        }

        #[tokio::test]
        async fn an_unreachable_session_loses_input_for_good() {
            let mut session = Session::new("unreachable");
            let (mut injector, _) =
                WaylandInjector::connect_with_backoff(session.target(), FAST.to_vec())
                    .await
                    .unwrap();
            session.end();
            let status = wait_status(&injector, |status| {
                matches!(status, InjectorStatus::Lost(_))
            })
            .await;
            assert!(matches!(
                status,
                InjectorStatus::Lost(InjectorLoss::Unreachable(_))
            ));
            assert!(matches!(
                injector.key_event(&press(0x41)),
                Err(InputError::InjectorLost)
            ));
        }
    }
}
