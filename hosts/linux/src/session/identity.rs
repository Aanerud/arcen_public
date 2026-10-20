//! Authenticated OS-user identity and fail-closed child process setup.

use std::collections::BTreeMap;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;

use arcen_session::restore_lease::IanaTimeZone;

use super::selinux::SessionSecurityContext;
use thiserror::Error;
use tokio::process::Command;

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("OS user does not exist")]
    UnknownUser,
    #[error("root graphical sessions are forbidden")]
    RootForbidden,
    #[error("OS user lookup failed")]
    Lookup,
    #[error("user runtime directory is unavailable")]
    RuntimeUnavailable,
    #[error("user session bus is unavailable")]
    SessionBusUnavailable,
    #[error("user runtime directory has unsafe ownership or permissions")]
    UnsafeRuntimeDirectory,
    #[error("user process setup is unavailable on this platform")]
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserIdentity {
    pub username: String,
    pub uid: u32,
    pub gid: u32,
    pub supplementary_groups: Vec<u32>,
    pub home: PathBuf,
    pub shell: PathBuf,
}

impl UserIdentity {
    #[cfg(target_os = "linux")]
    pub fn resolve(username: &str) -> Result<Self, IdentityError> {
        use std::ffi::CString;

        let user = nix::unistd::User::from_name(username)
            .map_err(|_| IdentityError::Lookup)?
            .ok_or(IdentityError::UnknownUser)?;
        if user.uid.is_root() {
            return Err(IdentityError::RootForbidden);
        }
        let c_username = CString::new(user.name.as_str()).map_err(|_| IdentityError::Lookup)?;
        let supplementary_groups = nix::unistd::getgrouplist(&c_username, user.gid)
            .map_err(|_| IdentityError::Lookup)?
            .into_iter()
            .map(|group| group.as_raw())
            .collect();
        Ok(Self {
            username: user.name,
            uid: user.uid.as_raw(),
            gid: user.gid.as_raw(),
            supplementary_groups,
            home: user.dir,
            shell: user.shell,
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn resolve(_username: &str) -> Result<Self, IdentityError> {
        Err(IdentityError::Unsupported)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEnvironment {
    values: BTreeMap<String, String>,
    /// The SELinux context PAM assigned to the session user's processes;
    /// applied only to commands configured as the session user.
    exec_context: Option<SessionSecurityContext>,
}

impl SessionEnvironment {
    pub fn build<I>(
        identity: &UserIdentity,
        display: &str,
        xauthority: Option<&str>,
        desktop: &str,
        trusted_timezone: Option<&IanaTimeZone>,
        pam_environment: I,
    ) -> Self
    where
        I: IntoIterator<Item = (String, String)>,
    {
        let mut values = BTreeMap::new();
        for (key, value) in pam_environment {
            if allowed_pam_variable(&key) && !value.contains('\0') {
                values.insert(key, value);
            }
        }

        let runtime_dir = format!("/run/user/{}", identity.uid);
        values.insert("HOME".into(), identity.home.to_string_lossy().into_owned());
        values.insert("USER".into(), identity.username.clone());
        values.insert("LOGNAME".into(), identity.username.clone());
        values.insert(
            "SHELL".into(),
            identity.shell.to_string_lossy().into_owned(),
        );
        values.insert(
            "PATH".into(),
            "/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin".into(),
        );
        values.insert("DISPLAY".into(), display.to_string());
        values.insert("XDG_RUNTIME_DIR".into(), runtime_dir.clone());
        values.insert(
            "DBUS_SESSION_BUS_ADDRESS".into(),
            format!("unix:path={runtime_dir}/bus"),
        );
        values.insert("PULSE_RUNTIME_PATH".into(), format!("{runtime_dir}/pulse"));
        values.insert("XDG_SESSION_TYPE".into(), "x11".into());
        values.insert("XDG_SESSION_CLASS".into(), "user".into());
        values.insert("XDG_SESSION_DESKTOP".into(), desktop.to_string());
        if desktop == "gnome-classic" {
            values.insert("XDG_CURRENT_DESKTOP".into(), "GNOME-Classic:GNOME".into());
            values.insert("GNOME_SHELL_SESSION_MODE".into(), "classic".into());
        } else {
            values.insert("XDG_CURRENT_DESKTOP".into(), "GNOME".into());
        }
        values.insert("GDK_BACKEND".into(), "x11".into());
        values.insert("MUTTER_DEBUG_DISABLE_UNREDIRECT".into(), "1".into());
        values.insert("__GL_SYNC_TO_VBLANK".into(), "1".into());
        values.insert("__GL_YIELD".into(), "USLEEP".into());
        if let Some(xauthority) = xauthority {
            values.insert("XAUTHORITY".into(), xauthority.to_string());
        } else {
            values.remove("XAUTHORITY");
        }
        if let Some(timezone) = trusted_timezone {
            values.insert("TZ".into(), timezone.as_str().to_string());
        }

        Self {
            values,
            exec_context: None,
        }
    }

    /// Runs this session's user commands in the SELinux context PAM
    /// assigned (see [`super::selinux`]).
    #[must_use]
    pub fn with_exec_context(mut self, context: Option<SessionSecurityContext>) -> Self {
        self.exec_context = context;
        self
    }

    pub fn exec_context(&self) -> Option<&SessionSecurityContext> {
        self.exec_context.as_ref()
    }

    /// Gives the desktop its locale (see [`super::locale`]). A value PAM
    /// already provided is kept.
    #[must_use]
    pub fn with_locale(mut self, locale: &arcen_session::session_locale::SessionLocale) -> Self {
        for (name, value) in locale.variables() {
            self.values
                .entry(name.to_owned())
                .or_insert_with(|| value.to_owned());
        }
        self
    }

    /// The environment of a headless Wayland session before its private
    /// runtime exists: the authenticated identity, logind's runtime
    /// directory and user bus, `XDG_SESSION_TYPE=wayland`, and no X11
    /// variables.
    pub fn build_wayland<I>(
        identity: &UserIdentity,
        desktop: &str,
        trusted_timezone: Option<&IanaTimeZone>,
        pam_environment: I,
    ) -> Self
    where
        I: IntoIterator<Item = (String, String)>,
    {
        let mut environment = Self::build(
            identity,
            "",
            None,
            desktop,
            trusted_timezone,
            pam_environment,
        );
        for key in [
            "DISPLAY",
            "XAUTHORITY",
            "GDK_BACKEND",
            "MUTTER_DEBUG_DISABLE_UNREDIRECT",
            "WAYLAND_DISPLAY",
        ] {
            environment.values.remove(key);
        }
        environment
            .values
            .insert("XDG_SESSION_TYPE".into(), "wayland".into());
        environment
    }

    /// Points this Wayland environment at the session's private runtime
    /// directory, bus and compositor, and adds the capture hand-off. Audio
    /// keeps the user's logind PulseAudio (`PULSE_RUNTIME_PATH`).
    pub fn with_wayland_session(
        mut self,
        ready: &super::wayland::WaylandAgentReady,
        handoff: &arcen_media::video::capture_handoff::PipeWireCaptureHandoff,
    ) -> Self {
        let runtime_dir = ready.runtime_dir.to_string_lossy().into_owned();
        self.values.insert(
            "DBUS_SESSION_BUS_ADDRESS".into(),
            format!(
                "unix:path={}",
                ready
                    .runtime_dir
                    .join(super::wayland::BUS_SOCKET_NAME)
                    .display()
            ),
        );
        self.values.insert("XDG_RUNTIME_DIR".into(), runtime_dir);
        self.values
            .insert("WAYLAND_DISPLAY".into(), ready.wayland_display.clone());
        // Only the hand-off names the encoder GPU of a Wayland session, and
        // not over an operator's CUDA device choice: capenc would prefer
        // the PCI address to it.
        self.values
            .remove(arcen_media::video::capture_handoff::ENCODER_GPU_PCI_ENV);
        // A wide stream belongs to one output generation: only this
        // hand-off's own (if any) may remain.
        self.values
            .remove(arcen_media::video::capture_handoff::PIPEWIRE_WIDE_NODE_ENV);
        let operator_chose_device = self.values.contains_key(arcen_capenc::CUDA_DEVICE_ENV);
        let handoff = if operator_chose_device {
            handoff.clone().with_encoder_gpu(None)
        } else {
            handoff.clone()
        };
        for (key, value) in handoff.to_env() {
            self.values.insert(key.to_owned(), value);
        }
        self
    }

    /// Points every helper of the session (audio capture, microphone) at
    /// the audio server its agent established: a private session graph's
    /// own PulseAudio-protocol server, or, without one, the user's.
    #[must_use]
    pub fn with_session_audio(mut self, audio: Option<&super::agent::SessionAudioReady>) -> Self {
        if let Some(path) = audio.and_then(|audio| audio.pulse_runtime_path.as_ref()) {
            self.values.insert(
                "PULSE_RUNTIME_PATH".into(),
                path.to_string_lossy().into_owned(),
            );
            self.values.remove("PULSE_SERVER");
        }
        self
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    /// The X display, or empty for a Wayland session.
    pub fn display(&self) -> &str {
        self.get("DISPLAY").unwrap_or_default()
    }

    pub fn session_id(&self) -> Option<&str> {
        self.get("XDG_SESSION_ID")
    }

    pub fn session_type(&self) -> &str {
        self.get("XDG_SESSION_TYPE")
            .expect("XDG_SESSION_TYPE is always populated")
    }

    pub fn desktop(&self) -> &str {
        self.get("XDG_SESSION_DESKTOP")
            .expect("XDG_SESSION_DESKTOP is always populated")
    }

    #[cfg(target_os = "linux")]
    pub fn validate_runtime(&self, identity: &UserIdentity) -> Result<(), IdentityError> {
        use std::os::unix::fs::MetadataExt;

        let runtime = self
            .get("XDG_RUNTIME_DIR")
            .ok_or(IdentityError::RuntimeUnavailable)?;
        let metadata = std::fs::metadata(runtime).map_err(|_| IdentityError::RuntimeUnavailable)?;
        if metadata.uid() != identity.uid || metadata.mode() & 0o077 != 0 {
            return Err(IdentityError::UnsafeRuntimeDirectory);
        }
        if !Path::new(runtime).join("bus").exists() {
            return Err(IdentityError::SessionBusUnavailable);
        }
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    pub fn validate_runtime(&self, _identity: &UserIdentity) -> Result<(), IdentityError> {
        Err(IdentityError::Unsupported)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserExecution {
    pub identity: UserIdentity,
    pub environment: SessionEnvironment,
}

impl UserExecution {
    pub fn new(identity: UserIdentity, environment: SessionEnvironment) -> Self {
        Self {
            identity,
            environment,
        }
    }

    pub fn configure(&self, command: &mut Command) -> Result<(), IdentityError> {
        configure_user_command(command, &self.identity, &self.environment)
    }
}

fn allowed_pam_variable(key: &str) -> bool {
    // The capture hand-off is set only from the session's own verified
    // ScreenCast stream, never from PAM.
    !arcen_media::video::capture_handoff::CAPTURE_HANDOFF_ENV.contains(&key)
        && !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && !matches!(
            key,
            "HOME"
                | "USER"
                | "LOGNAME"
                | "SHELL"
                | "PATH"
                | "DISPLAY"
                | "XAUTHORITY"
                | "XDG_RUNTIME_DIR"
                | "DBUS_SESSION_BUS_ADDRESS"
                | "LD_PRELOAD"
                | "LD_LIBRARY_PATH"
                | "LD_AUDIT"
                | "GCONV_PATH"
                | "NLSPATH"
                | "PYTHONPATH"
                | "RUSTC_WRAPPER"
        )
}

#[cfg(target_os = "linux")]
pub fn configure_user_command(
    command: &mut Command,
    identity: &UserIdentity,
    environment: &SessionEnvironment,
) -> Result<(), IdentityError> {
    use std::os::unix::process::CommandExt;

    if identity.uid == 0 {
        return Err(IdentityError::RootForbidden);
    }
    replace_command_environment(command, environment);
    let uid = nix::unistd::Uid::from_raw(identity.uid);
    let gid = nix::unistd::Gid::from_raw(identity.gid);
    let groups = identity
        .supplementary_groups
        .iter()
        .copied()
        .map(nix::unistd::Gid::from_raw)
        .collect::<Vec<_>>();
    let exec_context = environment
        .exec_context()
        .map(|context| context.as_c_str().to_owned());
    // SAFETY: this closure runs after fork and immediately before exec. It only
    // sets the exec context and performs credential syscalls with
    // parent-resolved values.
    unsafe {
        command.as_std_mut().pre_exec(move || {
            // Before the credential drop, which leaves this process's /proc
            // attributes owned by root.
            if let Some(context) = exec_context.as_deref() {
                super::selinux::apply_exec_context_in_child(context)?;
            }
            nix::unistd::setgroups(&groups).map_err(std::io::Error::other)?;
            nix::unistd::setgid(gid).map_err(std::io::Error::other)?;
            nix::unistd::setuid(uid).map_err(std::io::Error::other)?;
            // Credential changes clear PDEATHSIG in the kernel. Arm it last.
            arm_parent_death_signal()?;
            Ok(())
        });
    }

    Ok(())
}

fn replace_command_environment(command: &mut Command, environment: &SessionEnvironment) {
    command.env_clear().envs(environment.iter());
}

#[cfg(target_os = "linux")]
pub(crate) fn arm_parent_death_signal() -> std::io::Result<()> {
    // SAFETY: prctl/getppid have no pointer arguments here. Checking PPID after
    // arming closes the race where the parent exits between fork and prctl.
    unsafe {
        if nix::libc::prctl(nix::libc::PR_SET_PDEATHSIG, nix::libc::SIGKILL) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if nix::libc::getppid() == 1 {
            return Err(std::io::Error::other(
                "parent exited before child death signal was armed",
            ));
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn configure_user_command(
    _command: &mut Command,
    _identity: &UserIdentity,
    _environment: &SessionEnvironment,
) -> Result<(), IdentityError> {
    Err(IdentityError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artist() -> UserIdentity {
        UserIdentity {
            username: "artist".into(),
            uid: 1001,
            gid: 100,
            supplementary_groups: vec![100, 27, 44],
            home: "/home/artist".into(),
            shell: "/bin/bash".into(),
        }
    }

    #[test]
    fn session_audio_points_helpers_at_the_session_audio_server() {
        let base = SessionEnvironment::build_wayland(&artist(), "gnome", None, []);
        assert_eq!(base.get("PULSE_RUNTIME_PATH"), Some("/run/user/1001/pulse"));
        let private = super::super::agent::SessionAudioReady {
            mode: arcen_session::local_playback::SessionAudioMode::PrivateGraph,
            pulse_runtime_path: Some("/run/user/1001/arcen-wayland-c9/pulse".into()),
            capture_source: Some("arcen_session_sink.monitor".into()),
            local_playback: arcen_session::local_playback::LocalPlaybackReport::not_requested(),
            native_pipewire: true,
            degradation: None,
        };
        let environment = base.clone().with_session_audio(Some(&private));
        assert_eq!(
            environment.get("PULSE_RUNTIME_PATH"),
            Some("/run/user/1001/arcen-wayland-c9/pulse")
        );
        let user_server = super::super::agent::SessionAudioReady {
            pulse_runtime_path: None,
            ..private
        };
        assert_eq!(
            base.clone()
                .with_session_audio(Some(&user_server))
                .get("PULSE_RUNTIME_PATH"),
            Some("/run/user/1001/pulse")
        );
        assert_eq!(
            base.with_session_audio(None).get("PULSE_RUNTIME_PATH"),
            Some("/run/user/1001/pulse")
        );
    }

    #[test]
    fn environment_uses_authenticated_identity_and_fixed_session_values() {
        let environment = SessionEnvironment::build(
            &artist(),
            ":0",
            Some("/run/arcen/artist.xauth"),
            "gnome-classic",
            None,
            [
                ("XDG_SESSION_ID".into(), "c9".into()),
                ("LANG".into(), "en_US.UTF-8".into()),
            ],
        );
        assert_eq!(environment.get("USER"), Some("artist"));
        assert_eq!(environment.get("HOME"), Some("/home/artist"));
        assert_eq!(environment.get("XDG_RUNTIME_DIR"), Some("/run/user/1001"));
        assert_eq!(
            environment.get("DBUS_SESSION_BUS_ADDRESS"),
            Some("unix:path=/run/user/1001/bus")
        );
        assert_eq!(environment.session_id(), Some("c9"));
        assert_eq!(environment.desktop(), "gnome-classic");
    }

    #[test]
    fn pam_cannot_override_identity_or_inject_loader_variables() {
        let environment = SessionEnvironment::build(
            &artist(),
            ":0",
            None,
            "gnome-classic",
            None,
            [
                ("USER".into(), "root".into()),
                ("HOME".into(), "/root".into()),
                ("LD_PRELOAD".into(), "/tmp/owned.so".into()),
                ("LD_AUDIT".into(), "/tmp/audit.so".into()),
                ("LANG".into(), "nb_NO.UTF-8".into()),
            ],
        );
        assert_eq!(environment.get("USER"), Some("artist"));
        assert_eq!(environment.get("HOME"), Some("/home/artist"));
        assert_eq!(environment.get("LD_PRELOAD"), None);
        assert_eq!(environment.get("LD_AUDIT"), None);
        let injected = SessionEnvironment::build(
            &artist(),
            ":0",
            None,
            "gnome",
            None,
            [
                ("ARCEN_CAPTURE_PROVIDER".into(), "wayland".into()),
                ("ARCEN_PIPEWIRE_NODE".into(), "7".into()),
            ],
        );
        assert_eq!(injected.get("ARCEN_CAPTURE_PROVIDER"), None);
        assert_eq!(injected.get("ARCEN_PIPEWIRE_NODE"), None);
        assert_eq!(environment.get("LANG"), Some("nb_NO.UTF-8"));
    }

    #[test]
    fn trusted_timezone_overrides_pam_and_is_configured_on_children() {
        let timezone = IanaTimeZone::parse("Europe/Oslo").unwrap();
        let environment = SessionEnvironment::build(
            &artist(),
            ":0",
            None,
            "gnome",
            Some(&timezone),
            [("TZ".into(), "America/New_York".into())],
        );
        assert_eq!(environment.get("TZ"), Some("Europe/Oslo"));

        let mut command = Command::new("unused");
        command.env("INHERITED", "must-be-cleared");
        replace_command_environment(&mut command, &environment);
        let configured = command
            .as_std()
            .get_envs()
            .filter_map(|(key, value)| {
                value.map(|value| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            configured.get("TZ").map(String::as_str),
            Some("Europe/Oslo")
        );
        assert!(!configured.contains_key("INHERITED"));
    }

    #[test]
    fn wayland_environment_is_private_and_carries_the_capture_handoff() {
        let base = SessionEnvironment::build_wayland(
            &artist(),
            "gnome",
            None,
            [
                ("XDG_SESSION_ID".into(), "c9".into()),
                ("DISPLAY".into(), ":0".into()),
                ("WAYLAND_DISPLAY".into(), "wayland-9".into()),
                ("ARCEN_CAPTURE_GPU_PCI".into(), "0000:41:00.0".into()),
            ],
        );
        assert_eq!(base.get("XDG_SESSION_TYPE"), Some("wayland"));
        assert_eq!(base.display(), "");
        assert_eq!(base.get("GDK_BACKEND"), None);
        assert_eq!(base.get("WAYLAND_DISPLAY"), None);
        assert_eq!(base.get("XDG_RUNTIME_DIR"), Some("/run/user/1001"));

        let runtime_dir = super::super::wayland::private_runtime_dir(1001, "c9");
        let ready = super::super::wayland::WaylandAgentReady {
            compositor: "gnome-shell".into(),
            input_socket: runtime_dir.join(super::super::wayland::INPUT_SOCKET_NAME),
            runtime_dir: runtime_dir.clone(),
            wayland_display: "wayland-0".into(),
            outputs: vec![],
            streams: vec![],
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
        let handoff = arcen_media::video::capture_handoff::PipeWireCaptureHandoff::new(
            44,
            "Meta-0",
            1,
            (1920, 1080),
            (0, 0),
            arcen_media::video::capture_handoff::ScreenCastCursorMode::Metadata,
            &runtime_dir,
        )
        .unwrap();
        let session = base.with_wayland_session(&ready, &handoff);
        assert_eq!(
            session.get("XDG_RUNTIME_DIR"),
            Some("/run/user/1001/arcen-wayland-c9")
        );
        assert_eq!(
            session.get("DBUS_SESSION_BUS_ADDRESS"),
            Some("unix:path=/run/user/1001/arcen-wayland-c9/bus")
        );
        assert_eq!(session.get("WAYLAND_DISPLAY"), Some("wayland-0"));
        assert_eq!(
            session.get("PULSE_RUNTIME_PATH"),
            Some("/run/user/1001/pulse")
        );
        assert_eq!(session.get("ARCEN_CAPTURE_PROVIDER"), Some("wayland"));
        assert_eq!(session.get("ARCEN_PIPEWIRE_NODE"), Some("44"));
        assert_eq!(session.get("ARCEN_CAPTURE_CURSOR_MODE"), Some("metadata"));
        assert_eq!(session.get("ARCEN_CAPTURE_GPU_PCI"), None);
        // capenc reads back exactly what the session wrote.
        let parsed =
            arcen_media::video::capture_handoff::PipeWireCaptureHandoff::from_env(|name: &str| {
                session.get(name).map(str::to_owned)
            })
            .unwrap();
        assert_eq!(parsed, Some(handoff.clone()));

        // A session serving X11 applications through Mutter's Xwayland
        // still gives the Pier's own children no X display: Mutter exports
        // it to applications, and every X11-only Pier path stays off.
        let with_x11 = super::super::wayland::WaylandAgentReady {
            x11: Some(super::super::wayland::ReadyX11 {
                ready: true,
                failure: None,
                displays: Some(arcen_session::wayland_x11::SessionDisplays {
                    public: 0,
                    private: 1,
                    lock_pid: 4242,
                }),
                xauthority: Some(runtime_dir.join(".mutter-Xwaylandauth.F8GGX3")),
                glamor: arcen_session::wayland_x11::GlamorRoute::NvidiaGbm,
            }),
            ..ready.clone()
        };
        let x11_session = SessionEnvironment::build_wayland(
            &artist(),
            "gnome",
            None,
            [
                ("DISPLAY".into(), ":0".into()),
                ("XAUTHORITY".into(), "/home/artist/.Xauthority".into()),
            ],
        )
        .with_wayland_session(&with_x11, &handoff);
        assert_eq!(x11_session.display(), "");
        assert_eq!(x11_session.get("XAUTHORITY"), None);
        assert_eq!(x11_session.get("GDK_BACKEND"), None);

        let nvidia = arcen_media::video::capture_handoff::parse_pci_address("0000:00:10.0");
        let pinned = handoff.with_encoder_gpu(nvidia);
        let session = SessionEnvironment::build_wayland(&artist(), "gnome", None, [])
            .with_wayland_session(&ready, &pinned);
        assert_eq!(session.get("ARCEN_CAPTURE_GPU_PCI"), Some("0000:00:10.0"));
        let operator = SessionEnvironment::build_wayland(
            &artist(),
            "gnome",
            None,
            [("ARCEN_CUDA_DEVICE".into(), "1".into())],
        )
        .with_wayland_session(&ready, &pinned);
        assert_eq!(operator.get("ARCEN_CAPTURE_GPU_PCI"), None);
        assert_eq!(operator.get("ARCEN_CUDA_DEVICE"), Some("1"));
    }

    #[test]
    fn the_session_locale_fills_in_but_never_overrides_pam() {
        let locale = arcen_session::session_locale::SessionLocale::resolve(
            Some("LANG=en_GB.UTF-8\nLC_TIME=nb_NO.UTF-8\n"),
            None,
        );
        let wayland =
            SessionEnvironment::build_wayland(&artist(), "gnome", None, []).with_locale(&locale);
        assert_eq!(wayland.get("LANG"), Some("en_GB.UTF-8"));
        assert_eq!(wayland.get("LC_TIME"), Some("nb_NO.UTF-8"));
        let pam = SessionEnvironment::build(
            &artist(),
            ":10",
            None,
            "gnome",
            None,
            [("LANG".into(), "de_DE.UTF-8".into())],
        )
        .with_locale(&locale);
        assert_eq!(pam.get("LANG"), Some("de_DE.UTF-8"));
        assert_eq!(pam.get("LC_TIME"), Some("nb_NO.UTF-8"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn root_is_never_a_valid_graphical_identity() {
        assert!(matches!(
            UserIdentity::resolve("root"),
            Err(IdentityError::RootForbidden)
        ));
    }
}
