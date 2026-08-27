//! Authenticated OS-user identity and fail-closed child process setup.

use std::collections::BTreeMap;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;

use arcen_session::restore_lease::IanaTimeZone;
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

        Self { values }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    pub fn display(&self) -> &str {
        self.get("DISPLAY").expect("DISPLAY is always populated")
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
    !key.is_empty()
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
    // SAFETY: this closure runs after fork and immediately before exec. It only
    // performs credential syscalls with parent-resolved numeric IDs.
    unsafe {
        command.as_std_mut().pre_exec(move || {
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

    #[cfg(target_os = "linux")]
    #[test]
    fn root_is_never_a_valid_graphical_identity() {
        assert!(matches!(
            UserIdentity::resolve("root"),
            Err(IdentityError::RootForbidden)
        ));
    }
}
