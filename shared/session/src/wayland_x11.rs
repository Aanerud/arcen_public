//! X11 applications inside a headless Wayland session (Xwayland).
//!
//! A headless Mutter or GNOME Shell started without `--no-x11` runs Xwayland
//! for the session's X11 clients. Mutter owns that server: it picks two free
//! displays (a public `:n` for applications and a private one "for managed
//! services"), writes `/tmp/.X<n>-lock` holding the compositor's own pid,
//! binds `/tmp/.X11-unix/X<n>` plus an abstract socket, keeps an
//! MIT-MAGIC-COOKIE-1 in `$XDG_RUNTIME_DIR/.mutter-Xwaylandauth.*`, and
//! exports `DISPLAY`/`XAUTHORITY` to the applications it launches and to the
//! session bus's activation environment.
//!
//! This module holds the portable part: the operator setting, the proof a
//! qualification records, the decision whether a session runs Xwayland, and
//! pure parsers for the facts the Linux adapter reads (lock files, the
//! Xwayland command line, the authority file, which libraries Xwayland
//! mapped). Nothing here touches a file, a process or a socket.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The Xwayland executable Mutter and GNOME Shell run (their build-time
/// `XWAYLAND_PATH` on every distribution Arcen targets).
pub const XWAYLAND_BINARY: &str = "/usr/bin/Xwayland";

/// Directory holding X11 lock files.
pub const X11_LOCK_DIR: &str = "/tmp";

/// Directory holding X11 unix sockets.
pub const X11_UNIX_DIR: &str = "/tmp/.X11-unix";

/// Prefix of the authority file Mutter creates in the runtime directory.
pub const MUTTER_XAUTHORITY_PREFIX: &str = ".mutter-Xwaylandauth.";

/// The only X authorization protocol Mutter's Xwayland is given.
pub const MIT_MAGIC_COOKIE: &[u8] = b"MIT-MAGIC-COOKIE-1";

/// Highest display number Arcen accepts as a fact. Mutter tries at most 50
/// displays from 0 (or from 1024 under GDM); anything far beyond is not one
/// of its displays.
pub const MAX_X11_DISPLAY: u32 = 65_535;

/// Whether X11 applications may run in a headless Wayland session
/// (`platform.session.wayland_x11_apps`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum X11AppsSetting {
    /// On where this host's qualification proved Xwayland, otherwise off
    /// with the reason stated.
    #[default]
    Auto,
    /// Required: the Pier refuses to start when it cannot be honoured.
    On,
    /// Never: the compositor runs with `--no-x11`.
    Off,
}

impl X11AppsSetting {
    /// Stable token for configuration and logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::On => "on",
            Self::Off => "off",
        }
    }

    /// Whether a qualification run should try Xwayland: whenever the
    /// operator did not turn it off and it is installed.
    #[must_use]
    pub const fn qualifies(self, xwayland_installed: bool) -> bool {
        !matches!(self, Self::Off) && xwayland_installed
    }
}

/// Where Xwayland's glamor renders, from the libraries it mapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GlamorRoute {
    /// NVIDIA's EGL through its GBM backend: X11 GL clients get NVIDIA
    /// direct rendering.
    NvidiaGbm,
    /// Mesa's GBM and EGL: a Mesa hardware driver or llvmpipe.
    MesaGbm,
    /// No EGL or GBM at all: glamor is off and X11 rendering is software.
    Software,
    /// The maps could not be read or name no known stack.
    Unknown,
}

impl GlamorRoute {
    /// Stable token for records and logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::NvidiaGbm => "nvidia-gbm",
            Self::MesaGbm => "mesa-gbm",
            Self::Software => "software",
            Self::Unknown => "unknown",
        }
    }
}

/// What a qualification session proved about Xwayland.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XwaylandProof {
    /// A client authenticated with the session's cookie and was served.
    pub display_proven: bool,
    /// The compositor manages the X11 screen (EWMH
    /// `_NET_SUPPORTING_WM_CHECK` on the root window).
    pub window_manager: bool,
    /// Pointer motion injected through the session's input socket moved the
    /// X server's pointer over an X11 window. Absent in records written
    /// before the check existed, which read as not proven.
    #[serde(default)]
    pub pointer: bool,
    /// Where Xwayland's glamor renders.
    pub glamor: GlamorRoute,
    /// `Xwayland -version`, informational: a security update of Xwayland
    /// does not make a record stale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xwayland_version: Option<String>,
    /// Why the proof failed, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

impl XwaylandProof {
    /// Whether X11 applications are proven to run and to take the session's
    /// pointer.
    #[must_use]
    pub const fn proven(&self) -> bool {
        self.display_proven && self.window_manager && self.pointer
    }
}

/// Why a Pier runs its Wayland sessions without Xwayland.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum X11AppsDisabledReason {
    /// The operator set `off`.
    OperatorOff,
    /// No Xwayland executable is installed.
    XwaylandNotInstalled,
    /// The qualification record has no Xwayland proof.
    NotQualified,
    /// The qualification record says Xwayland did not work.
    QualificationFailed,
}

impl X11AppsDisabledReason {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::OperatorOff => "operator_off",
            Self::XwaylandNotInstalled => "xwayland_not_installed",
            Self::NotQualified => "not_qualified",
            Self::QualificationFailed => "qualification_failed",
        }
    }

    /// What an operator can do about it.
    #[must_use]
    pub const fn remedy(self) -> &'static str {
        match self {
            Self::OperatorOff => "set platform.session.wayland_x11_apps to auto or on",
            Self::XwaylandNotInstalled => {
                "install the distribution's Xwayland package (xorg-x11-server-Xwayland or xwayland), then requalify with `arcen-pier wayland qualify --write`"
            }
            Self::NotQualified => "requalify with `arcen-pier wayland qualify --write`",
            Self::QualificationFailed => {
                "read the qualification's x11_apps failure, fix it, then requalify with `arcen-pier wayland qualify --write`"
            }
        }
    }
}

/// Whether this Pier's Wayland sessions run Xwayland.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum X11AppsPlan {
    /// Sessions start their compositor with Xwayland.
    Enabled,
    /// Sessions start their compositor with `--no-x11`.
    Disabled(X11AppsDisabledReason),
}

impl X11AppsPlan {
    /// Whether sessions run Xwayland.
    #[must_use]
    pub const fn enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }

    /// `enabled`, or the reason token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled(reason) => reason.token(),
        }
    }
}

/// `on` could not be honoured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct X11AppsRefusal {
    /// What is missing.
    pub reason: X11AppsDisabledReason,
}

impl std::fmt::Display for X11AppsRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "platform.session.wayland_x11_apps is on, but X11 applications cannot be served ({}): {}",
            self.reason.token(),
            self.reason.remedy()
        )
    }
}

impl std::error::Error for X11AppsRefusal {}

/// Decides whether this Pier's Wayland sessions run Xwayland.
///
/// `auto` turns it on only where Xwayland is installed and the host's
/// qualification proved it, so the default never risks a desktop on an
/// unproven X server (Mutter ends the whole compositor when its Xwayland
/// fails). `on` makes the same requirement a refusal; `off` always wins.
///
/// # Errors
///
/// [`X11AppsRefusal`] for `on` when a requirement is missing.
pub fn resolve_x11_apps(
    setting: X11AppsSetting,
    xwayland_installed: bool,
    proof: Option<&XwaylandProof>,
) -> Result<X11AppsPlan, X11AppsRefusal> {
    let missing = if setting == X11AppsSetting::Off {
        return Ok(X11AppsPlan::Disabled(X11AppsDisabledReason::OperatorOff));
    } else if !xwayland_installed {
        X11AppsDisabledReason::XwaylandNotInstalled
    } else {
        match proof {
            None => X11AppsDisabledReason::NotQualified,
            Some(proof) if !proof.proven() => X11AppsDisabledReason::QualificationFailed,
            Some(_) => return Ok(X11AppsPlan::Enabled),
        }
    };
    match setting {
        X11AppsSetting::On => Err(X11AppsRefusal { reason: missing }),
        X11AppsSetting::Auto | X11AppsSetting::Off => Ok(X11AppsPlan::Disabled(missing)),
    }
}

/// `:<n>` for display `n`.
#[must_use]
pub fn display_name(display: u32) -> String {
    format!(":{display}")
}

/// The display number of `:<n>` (no host, no screen).
#[must_use]
pub fn parse_display_name(name: &str) -> Option<u32> {
    parse_display_number(name.strip_prefix(':')?)
}

fn parse_display_number(digits: &str) -> Option<u32> {
    if digits.is_empty() || digits.len() > 5 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits
        .parse()
        .ok()
        .filter(|display| *display <= MAX_X11_DISPLAY)
}

/// The socket name of display `n` inside [`X11_UNIX_DIR`].
#[must_use]
pub fn x11_socket_name(display: u32) -> String {
    format!("X{display}")
}

/// The lock file name of display `n` inside [`X11_LOCK_DIR`].
#[must_use]
pub fn x11_lock_name(display: u32) -> String {
    format!(".X{display}-lock")
}

/// The display number a lock file name stands for (`.X<n>-lock`).
#[must_use]
pub fn parse_lock_name(name: &str) -> Option<u32> {
    parse_display_number(name.strip_prefix(".X")?.strip_suffix("-lock")?)
}

/// The pid an X lock file holds. Xorg and Mutter write it as `%10d\n`
/// (right-aligned in ten columns); anything else is refused.
#[must_use]
pub fn parse_x_lock(contents: &[u8]) -> Option<u32> {
    let text = std::str::from_utf8(contents).ok()?;
    let line = text.strip_suffix('\n').unwrap_or(text);
    if line.len() > 10 || line.contains('\n') {
        return None;
    }
    let digits = line.trim_start_matches(' ');
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().filter(|pid| *pid > 0)
}

/// What Mutter's Xwayland command line names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XwaylandArgs {
    /// The public display it serves.
    pub display: u32,
    /// The authority file it checks clients against.
    pub auth: PathBuf,
}

/// Parses `/proc/<pid>/cmdline` of Mutter's Xwayland:
/// `Xwayland :<n> -rootless … -auth <file> …`. `None` for any other
/// process or a command line without both.
#[must_use]
pub fn parse_xwayland_cmdline(cmdline: &[u8]) -> Option<XwaylandArgs> {
    let mut arguments = cmdline
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(std::str::from_utf8);
    let program = arguments.next()?.ok()?;
    if program.rsplit('/').next()? != "Xwayland" {
        return None;
    }
    let display = parse_display_name(arguments.next()?.ok()?)?;
    let mut rootless = false;
    let mut auth = None;
    while let Some(argument) = arguments.next() {
        match argument.ok()? {
            "-rootless" => rootless = true,
            "-auth" => auth = Some(arguments.next()?.ok()?),
            _ => {}
        }
    }
    // Unix-absolute on every build host, so the parser behaves the same
    // where `Path::is_absolute` would want a drive letter.
    let auth = auth.filter(|auth| auth.starts_with('/') && !auth.contains("/../"))?;
    rootless.then(|| XwaylandArgs {
        display,
        auth: PathBuf::from(auth),
    })
}

/// Whether `path` is the authority file Mutter creates directly inside the
/// session's private runtime directory.
#[must_use]
pub fn is_mutter_xauthority(path: &Path, runtime_dir: &Path) -> bool {
    path.parent() == Some(runtime_dir)
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix(MUTTER_XAUTHORITY_PREFIX))
            .is_some_and(|suffix| {
                !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
            })
}

/// The MIT-MAGIC-COOKIE-1 an Xauthority file holds for `display`: an entry
/// whose display number is `display` or empty (any display, as Mutter
/// writes it). The address and family are not checked; the caller chooses
/// the socket.
#[must_use]
pub fn parse_xauthority_cookie(contents: &[u8], display: u32) -> Option<Vec<u8>> {
    fn field<'a>(bytes: &mut &'a [u8]) -> Option<&'a [u8]> {
        let (length, rest) = bytes.split_first_chunk::<2>()?;
        let length = usize::from(u16::from_be_bytes(*length));
        let (value, rest) = rest.split_at_checked(length)?;
        *bytes = rest;
        Some(value)
    }
    let wanted = display.to_string();
    let mut rest = contents;
    while !rest.is_empty() {
        let (_family, after) = rest.split_first_chunk::<2>()?;
        rest = after;
        let _address = field(&mut rest)?;
        let number = field(&mut rest)?;
        let name = field(&mut rest)?;
        let data = field(&mut rest)?;
        if name == MIT_MAGIC_COOKIE
            && data.len() == 16
            && (number.is_empty() || number == wanted.as_bytes())
        {
            return Some(data.to_vec());
        }
    }
    None
}

/// One `/tmp/.X<n>-lock` as found on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockFact {
    /// Display number from the file name.
    pub display: u32,
    /// Owner of the file (never followed through a symlink).
    pub owner_uid: u32,
    /// Pid the file holds, when it parsed.
    pub pid: Option<u32>,
}

/// The two displays one compositor holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDisplays {
    /// The display applications use (`DISPLAY`).
    pub public: u32,
    /// The display Mutter keeps for managed services
    /// (`GNOME_SETUP_DISPLAY`).
    pub private: u32,
    /// The compositor pid both lock files hold.
    pub lock_pid: u32,
}

impl SessionDisplays {
    /// Both display numbers.
    #[must_use]
    pub const fn numbers(&self) -> [u32; 2] {
        [self.public, self.private]
    }
}

/// The displays the compositor `compositor_pid` of `uid` locked: exactly two
/// lock files owned by `uid` that hold that pid. Mutter locks its public
/// display first and its private one after it, so the lower number is the
/// public one. Any other count is not a Mutter Xwayland of this session.
#[must_use]
pub fn session_displays(
    locks: &[LockFact],
    uid: u32,
    compositor_pid: u32,
) -> Option<SessionDisplays> {
    let mut ours = locks
        .iter()
        .filter(|lock| lock.owner_uid == uid && lock.pid == Some(compositor_pid))
        .map(|lock| lock.display)
        .collect::<Vec<_>>();
    ours.sort_unstable();
    ours.dedup();
    match ours.as_slice() {
        [public, private] => Some(SessionDisplays {
            public: *public,
            private: *private,
            lock_pid: compositor_pid,
        }),
        _ => None,
    }
}

/// The kind of directory entry an X11 artefact is, from `lstat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactType {
    /// A unix socket.
    Socket,
    /// A regular file.
    Regular,
    /// A symlink, directory or anything else.
    Other,
}

/// Whether a session's leftover `/tmp/.X11-unix/X<n>` may be removed: a
/// socket (never followed) owned by the session user, for a display whose
/// `/tmp/.X<n>-lock` still holds the session compositor's pid
/// (`session_lock_held`, [`removable_lock`]). A display number the
/// compositor freed may already serve another desktop of the same user; its
/// lock then names that desktop's pid (or is not yet written), and its
/// socket is left alone.
#[must_use]
pub fn removable_socket(
    kind: ArtifactType,
    owner_uid: u32,
    uid: u32,
    session_lock_held: bool,
) -> bool {
    session_lock_held && kind == ArtifactType::Socket && owner_uid == uid
}

/// Whether a session's leftover `/tmp/.X<n>-lock` may be removed: a regular
/// file (never followed) owned by the session user that still holds the
/// session compositor's pid.
#[must_use]
pub fn removable_lock(
    kind: ArtifactType,
    owner_uid: u32,
    lock_pid: Option<u32>,
    uid: u32,
    compositor_pid: u32,
) -> bool {
    kind == ArtifactType::Regular && owner_uid == uid && lock_pid == Some(compositor_pid)
}

/// Where Xwayland's glamor renders, from `/proc/<pid>/maps` text.
#[must_use]
pub fn glamor_route_from_maps(maps: &str) -> GlamorRoute {
    let libraries = maps
        .lines()
        .filter_map(|line| line.split_whitespace().nth(5))
        .filter_map(|path| path.rsplit('/').next())
        .collect::<Vec<_>>();
    if libraries.is_empty() {
        return GlamorRoute::Unknown;
    }
    let has = |prefix: &str| libraries.iter().any(|name| name.starts_with(prefix));
    if has("libnvidia-egl-gbm") || (has("libEGL_nvidia") && has("libgbm")) {
        GlamorRoute::NvidiaGbm
    } else if has("libgbm") && (has("libEGL_mesa") || has("libgallium") || has("dri_gbm")) {
        GlamorRoute::MesaGbm
    } else if !has("libgbm") && !has("libEGL") {
        GlamorRoute::Software
    } else {
        GlamorRoute::Unknown
    }
}

/// `Xwayland -version` output's version number (`… Xwayland Version 24.1.9 …`).
#[must_use]
pub fn parse_xwayland_version(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let (_, after) = line.split_once("Xwayland Version ")?;
        let version = after.split_whitespace().next()?;
        (!version.is_empty()
            && version
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'.'))
        .then(|| version.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proof(display: bool, wm: bool) -> XwaylandProof {
        XwaylandProof {
            display_proven: display,
            window_manager: wm,
            pointer: true,
            glamor: GlamorRoute::NvidiaGbm,
            xwayland_version: Some("24.1.9".into()),
            failure: None,
        }
    }

    #[test]
    fn auto_is_on_only_where_installed_and_proven() {
        let proven = proof(true, true);
        let failed = proof(true, false);
        assert_eq!(
            resolve_x11_apps(X11AppsSetting::Auto, true, Some(&proven)),
            Ok(X11AppsPlan::Enabled)
        );
        assert_eq!(
            resolve_x11_apps(X11AppsSetting::Auto, false, Some(&proven)),
            Ok(X11AppsPlan::Disabled(
                X11AppsDisabledReason::XwaylandNotInstalled
            ))
        );
        assert_eq!(
            resolve_x11_apps(X11AppsSetting::Auto, true, None),
            Ok(X11AppsPlan::Disabled(X11AppsDisabledReason::NotQualified))
        );
        assert_eq!(
            resolve_x11_apps(X11AppsSetting::Auto, true, Some(&failed)),
            Ok(X11AppsPlan::Disabled(
                X11AppsDisabledReason::QualificationFailed
            ))
        );
    }

    #[test]
    fn on_refuses_what_auto_would_disable_and_off_always_wins() {
        let proven = proof(true, true);
        assert_eq!(
            resolve_x11_apps(X11AppsSetting::On, true, Some(&proven)),
            Ok(X11AppsPlan::Enabled)
        );
        for (installed, record) in [
            (false, Some(&proven)),
            (true, None),
            (true, Some(&proof(false, true))),
        ] {
            let refusal = resolve_x11_apps(X11AppsSetting::On, installed, record)
                .expect_err("on must refuse");
            assert!(refusal.to_string().contains("wayland_x11_apps is on"));
        }
        for (installed, record) in [(true, Some(&proven)), (false, None)] {
            assert_eq!(
                resolve_x11_apps(X11AppsSetting::Off, installed, record),
                Ok(X11AppsPlan::Disabled(X11AppsDisabledReason::OperatorOff))
            );
        }
        assert!(X11AppsSetting::Auto.qualifies(true));
        assert!(X11AppsSetting::On.qualifies(true));
        assert!(!X11AppsSetting::Off.qualifies(true));
        assert!(!X11AppsSetting::Auto.qualifies(false));
    }

    #[test]
    fn the_setting_parses_its_tokens_and_refuses_others() {
        for setting in [
            X11AppsSetting::Auto,
            X11AppsSetting::On,
            X11AppsSetting::Off,
        ] {
            let json = format!("\"{}\"", setting.token());
            assert_eq!(
                serde_json::from_str::<X11AppsSetting>(&json).expect("token"),
                setting
            );
        }
        assert!(serde_json::from_str::<X11AppsSetting>("\"yes\"").is_err());
        assert_eq!(X11AppsSetting::default(), X11AppsSetting::Auto);
    }

    #[test]
    fn the_proof_round_trips_and_refuses_unknown_fields() {
        let value = proof(true, true);
        let json = serde_json::to_string(&value).expect("serialize");
        assert!(json.contains("\"glamor\":\"nvidia-gbm\""));
        assert_eq!(
            serde_json::from_str::<XwaylandProof>(&json).expect("parse"),
            value
        );
        let mut no_pointer = value.clone();
        no_pointer.pointer = false;
        assert!(!no_pointer.proven());
        let minimal = r#"{"display_proven":true,"window_manager":true,"glamor":"unknown"}"#;
        assert!(
            !serde_json::from_str::<XwaylandProof>(minimal)
                .expect("minimal")
                .proven()
        );
        assert!(
            serde_json::from_str::<XwaylandProof>(
                r#"{"display_proven":true,"window_manager":true,"glamor":"unknown","extra":1}"#
            )
            .is_err()
        );
    }

    #[test]
    fn display_names_and_lock_names_parse_strictly() {
        assert_eq!(parse_display_name(":0"), Some(0));
        assert_eq!(parse_display_name(":1024"), Some(1024));
        assert_eq!(display_name(7), ":7");
        for bad in ["0", ":", ":0.0", "host:0", ":-1", ":99999999", ": 1", ":1a"] {
            assert_eq!(parse_display_name(bad), None, "{bad}");
        }
        assert_eq!(parse_lock_name(".X0-lock"), Some(0));
        assert_eq!(parse_lock_name(".X12-lock"), Some(12));
        for bad in [".X-lock", "X0-lock", ".X0-lock~", ".Xa-lock", ".X0"] {
            assert_eq!(parse_lock_name(bad), None, "{bad}");
        }
        assert_eq!(x11_lock_name(3), ".X3-lock");
        assert_eq!(x11_socket_name(3), "X3");
    }

    #[test]
    fn lock_contents_are_the_ten_column_pid_only() {
        assert_eq!(parse_x_lock(b"    397186\n"), Some(397_186));
        assert_eq!(parse_x_lock(b"         1\n"), Some(1));
        assert_eq!(parse_x_lock(b"42"), Some(42));
        for bad in [
            &b""[..],
            b"\n",
            b"          \n",
            b"         0\n",
            b"  12a4\n",
            b"12345678901\n",
            b"  12\n13\n",
            b"\xff\n",
        ] {
            assert_eq!(parse_x_lock(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_xwayland_command_line_names_display_and_authority() {
        let mutter49 = b"/usr/bin/Xwayland\0:0\0-rootless\0-noreset\0-accessx\0-core\0-auth\0/run/user/1003/arcen-wayland-5/.mutter-Xwaylandauth.F8GGX3\0-listenfd\04\0-listenfd\05\0-displayfd\06\0-initfd\07\0-byteswappedclients\0-enable-ei-portal\0";
        assert_eq!(
            parse_xwayland_cmdline(mutter49),
            Some(XwaylandArgs {
                display: 0,
                auth: PathBuf::from("/run/user/1003/arcen-wayland-5/.mutter-Xwaylandauth.F8GGX3"),
            })
        );
        let mutter40 = b"/usr/bin/Xwayland\0:2\0-rootless\0-noreset\0-accessx\0-core\0-auth\0/run/user/1000/.mutter-Xwaylandauth.ABC123\0-listen\04\0-listen\05\0-displayfd\06\0-listen\07\0";
        assert_eq!(
            parse_xwayland_cmdline(mutter40).map(|args| args.display),
            Some(2)
        );
        for bad in [
            &b"/usr/bin/Xorg\0:0\0-rootless\0-auth\0/x\0"[..],
            b"/usr/bin/Xwayland\0:0\0-rootless\0",
            b"/usr/bin/Xwayland\0:0\0-auth\0/x\0",
            b"/usr/bin/Xwayland\0:0\0-rootless\0-auth\0relative\0",
            b"/usr/bin/Xwayland\0-rootless\0-auth\0/x\0",
            b"/usr/bin/Xwayland\0:0\0-rootless\0-auth\0",
            b"",
        ] {
            assert_eq!(parse_xwayland_cmdline(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn only_mutters_authority_file_in_the_runtime_dir_is_accepted() {
        let runtime = Path::new("/run/user/1003/arcen-wayland-5");
        assert!(is_mutter_xauthority(
            &runtime.join(".mutter-Xwaylandauth.F8GGX3"),
            runtime
        ));
        for bad in [
            runtime.join(".mutter-Xwaylandauth."),
            runtime.join(".Xauthority"),
            runtime.join("sub/.mutter-Xwaylandauth.F8GGX3"),
            PathBuf::from("/tmp/.mutter-Xwaylandauth.F8GGX3"),
            runtime.join(".mutter-Xwaylandauth.../x"),
        ] {
            assert!(!is_mutter_xauthority(&bad, runtime), "{}", bad.display());
        }
    }

    fn xauth_entry(number: &[u8], name: &[u8], data: &[u8]) -> Vec<u8> {
        let mut entry = 256u16.to_be_bytes().to_vec();
        for value in [&b"pier.example.internal"[..], number, name, data] {
            entry.extend_from_slice(&u16::try_from(value.len()).expect("len").to_be_bytes());
            entry.extend_from_slice(value);
        }
        entry
    }

    #[test]
    fn the_cookie_is_found_for_the_display_or_any_display() {
        let cookie = [7u8; 16];
        let mutter = xauth_entry(b"", MIT_MAGIC_COOKIE, &cookie);
        assert_eq!(parse_xauthority_cookie(&mutter, 0), Some(cookie.to_vec()));
        assert_eq!(parse_xauthority_cookie(&mutter, 9), Some(cookie.to_vec()));

        let mut several = xauth_entry(b"1", MIT_MAGIC_COOKIE, &[1u8; 16]);
        several.extend(xauth_entry(b"0", b"XDM-AUTHORIZATION-1", &[2u8; 16]));
        several.extend(xauth_entry(b"0", MIT_MAGIC_COOKIE, &[3u8; 16]));
        assert_eq!(parse_xauthority_cookie(&several, 0), Some(vec![3u8; 16]));
        assert_eq!(parse_xauthority_cookie(&several, 1), Some(vec![1u8; 16]));
        assert_eq!(parse_xauthority_cookie(&several, 2), None);

        assert_eq!(
            parse_xauthority_cookie(&xauth_entry(b"", MIT_MAGIC_COOKIE, &[1u8; 8]), 0),
            None
        );
        let truncated = &mutter[..mutter.len() - 3];
        assert_eq!(parse_xauthority_cookie(truncated, 0), None);
        assert_eq!(parse_xauthority_cookie(&[], 0), None);
    }

    #[test]
    fn the_session_holds_exactly_two_displays_locked_by_its_compositor() {
        let lock = |display, owner_uid, pid| LockFact {
            display,
            owner_uid,
            pid,
        };
        let locks = [
            lock(0, 1000, Some(500)),
            lock(1, 1003, Some(900)),
            lock(2, 1003, Some(900)),
            lock(3, 1003, Some(901)),
            lock(4, 0, Some(900)),
            lock(5, 1003, None),
        ];
        assert_eq!(
            session_displays(&locks, 1003, 900),
            Some(SessionDisplays {
                public: 1,
                private: 2,
                lock_pid: 900
            })
        );
        assert_eq!(session_displays(&locks, 1003, 901), None);
        assert_eq!(session_displays(&locks, 1000, 900), None);
        let three = [
            lock(1, 1003, Some(900)),
            lock(2, 1003, Some(900)),
            lock(7, 1003, Some(900)),
        ];
        assert_eq!(session_displays(&three, 1003, 900), None);
        let skipped = [lock(1024, 1003, Some(5)), lock(1026, 1003, Some(5))];
        assert_eq!(
            session_displays(&skipped, 1003, 5).map(|displays| displays.numbers()),
            Some([1024, 1026])
        );
    }

    #[test]
    fn only_the_sessions_own_artefacts_are_removable() {
        assert!(removable_socket(ArtifactType::Socket, 1003, 1003, true));
        assert!(!removable_socket(ArtifactType::Socket, 1000, 1003, true));
        assert!(!removable_socket(ArtifactType::Other, 1003, 1003, true));
        assert!(!removable_socket(ArtifactType::Regular, 1003, 1003, true));
        // The user's own socket of a display whose lock no longer holds the
        // session's compositor (freed, perhaps taken by another desktop).
        assert!(!removable_socket(ArtifactType::Socket, 1003, 1003, false));

        assert!(removable_lock(
            ArtifactType::Regular,
            1003,
            Some(900),
            1003,
            900
        ));
        assert!(!removable_lock(
            ArtifactType::Regular,
            1003,
            Some(901),
            1003,
            900
        ));
        assert!(!removable_lock(
            ArtifactType::Regular,
            1003,
            None,
            1003,
            900
        ));
        assert!(!removable_lock(
            ArtifactType::Regular,
            0,
            Some(900),
            1003,
            900
        ));
        assert!(!removable_lock(
            ArtifactType::Other,
            1003,
            Some(900),
            1003,
            900
        ));
    }

    #[test]
    fn glamor_is_classified_from_the_mapped_libraries() {
        let line = |path: &str| format!("7f00-7f01 r-xp 00000000 fd:00 1234 {path}\n");
        let nvidia = [
            "/usr/lib64/libEGL_nvidia.so.580.105.08",
            "/usr/lib64/libgbm.so.1.0.0",
            "/usr/lib64/libnvidia-egl-gbm.so.1.1.2",
        ]
        .map(line)
        .concat();
        assert_eq!(glamor_route_from_maps(&nvidia), GlamorRoute::NvidiaGbm);
        let mesa = [
            "/usr/lib64/libEGL_mesa.so.0.0.0",
            "/usr/lib64/libgbm.so.1.0.0",
            "/usr/lib64/libgallium-25.2.7.so",
        ]
        .map(line)
        .concat();
        assert_eq!(glamor_route_from_maps(&mesa), GlamorRoute::MesaGbm);
        let software = ["/usr/bin/Xwayland", "/usr/lib64/libc.so.6"]
            .map(line)
            .concat();
        assert_eq!(glamor_route_from_maps(&software), GlamorRoute::Software);
        assert_eq!(glamor_route_from_maps(""), GlamorRoute::Unknown);
        let partial = ["/usr/lib64/libEGL.so.1", "/usr/lib64/libc.so.6"]
            .map(line)
            .concat();
        assert_eq!(glamor_route_from_maps(&partial), GlamorRoute::Unknown);
    }

    #[test]
    fn the_xwayland_version_is_read_from_its_banner() {
        let banner = "\nThe X.Org Foundation Xwayland Version 24.1.9 (12401009)\nX Protocol Version 11, Revision 0\n";
        assert_eq!(parse_xwayland_version(banner).as_deref(), Some("24.1.9"));
        assert_eq!(parse_xwayland_version("Xwayland Version x;rm\n"), None);
        assert_eq!(parse_xwayland_version("nothing"), None);
    }
}
