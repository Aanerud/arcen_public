//! The SELinux context PAM assigns to a desktop session, and where it applies.
//!
//! `pam_selinux open` (in the stock `login`, `sshd` and `gdm-*` stacks) sets
//! the calling thread's *exec* context to the user's default context, for
//! example `unconfined_u:unconfined_r:unconfined_t:s0`, because PAM expects
//! the next `execve` to be the user's session. The launcher is not that: once
//! the PAM session is open it still runs its own root helpers (`loginctl`,
//! `busctl`, Xorg, `xauth`) before the session user's first process. Left in
//! place, the exec context makes every one of those `execve`s a domain
//! transition into the user's context, which a launcher running as a systemd
//! service (`unconfined_service_t`) is not allowed to make.
//!
//! So the launcher takes the context back off its thread as soon as
//! `pam_open_session` returns, runs its root helpers in its own domain, and
//! applies the context only in the forked child that drops to the session
//! user — exactly where a display manager's session worker applies it. The
//! context is never discarded: if the policy refuses the transition, the
//! session is refused rather than run in the service's domain.

use std::ffi::{CStr, CString};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SelinuxError {
    #[error("the SELinux exec context PAM assigned could not be read")]
    Read,
    #[error("the SELinux exec context PAM assigned is malformed")]
    Malformed,
    #[error("the SELinux exec context could not be cleared from the launcher")]
    Clear,
}

/// A security context PAM chose for the session user's processes.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionSecurityContext(CString);

impl std::fmt::Debug for SessionSecurityContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("SessionSecurityContext")
            .field(&self.as_str())
            .finish()
    }
}

impl std::fmt::Display for SessionSecurityContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl SessionSecurityContext {
    /// The longest context accepted; the kernel's own limit is a page.
    const MAX_LEN: usize = 1024;

    /// Parses a `/proc/<pid>/attr/*` value. An empty value means no
    /// context is set and is `Ok(None)`; anything that is not
    /// `user:role:type[:level]` in printable ASCII is refused.
    pub fn parse(raw: &[u8]) -> Result<Option<Self>, SelinuxError> {
        let trimmed = raw
            .iter()
            .rposition(|byte| !matches!(byte, b'\0' | b'\n'))
            .map_or(&raw[..0], |last| &raw[..=last]);
        if trimmed.is_empty() {
            return Ok(None);
        }
        if trimmed.len() > Self::MAX_LEN || !trimmed.iter().all(u8::is_ascii_graphic) {
            return Err(SelinuxError::Malformed);
        }
        let text = std::str::from_utf8(trimmed).map_err(|_| SelinuxError::Malformed)?;
        let mut fields = text.splitn(4, ':');
        let complete = (0..3).all(|_| fields.next().is_some_and(|field| !field.is_empty()));
        if !complete {
            return Err(SelinuxError::Malformed);
        }
        CString::new(trimmed)
            .map(|context| Some(Self(context)))
            .map_err(|_| SelinuxError::Malformed)
    }

    pub fn as_str(&self) -> &str {
        // Constructed only from validated ASCII.
        self.0.to_str().unwrap_or_default()
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn as_c_str(&self) -> &CStr {
        &self.0
    }
}

/// What the kernel's access-vector decision says about one permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessDecision {
    Allowed,
    Denied,
}

/// Decodes `/sys/fs/selinux/access`'s answer (`allowed decided auditallow
/// auditdeny seqno flags`, hex but for the decimal sequence number) for the
/// permission at 1-based `permission_index`. A permissive domain is allowed.
pub fn decode_access_response(response: &str, permission_index: u32) -> Option<AccessDecision> {
    const AVD_FLAGS_PERMISSIVE: u32 = 0x0001;
    let bit = 1u32.checked_shl(permission_index.checked_sub(1)?)?;
    let mut fields = response.split_ascii_whitespace();
    let allowed = u32::from_str_radix(fields.next()?, 16).ok()?;
    let decided = u32::from_str_radix(fields.next()?, 16).ok()?;
    let flags = fields
        .nth(3)
        .and_then(|flags| u32::from_str_radix(flags, 16).ok())
        .unwrap_or(0);
    if decided & bit == 0 {
        return None;
    }
    if allowed & bit != 0 || flags & AVD_FLAGS_PERMISSIVE != 0 {
        Some(AccessDecision::Allowed)
    } else {
        Some(AccessDecision::Denied)
    }
}

#[cfg(target_os = "linux")]
const EXEC_ATTR_PATH: &str = "/proc/thread-self/attr/exec";
#[cfg(target_os = "linux")]
const EXEC_ATTR: &CStr = c"/proc/thread-self/attr/exec";
#[cfg(target_os = "linux")]
const CURRENT_ATTR: &str = "/proc/thread-self/attr/current";
#[cfg(target_os = "linux")]
const SELINUXFS: &str = "/sys/fs/selinux";

/// Takes the exec context `pam_selinux open` left on this thread and clears
/// it, so the launcher's own helpers keep the launcher's domain. `None`
/// when SELinux is not enabled or PAM set no context.
///
/// Call on the thread that called `pam_open_session`, before it spawns
/// anything: the exec context is a per-thread attribute.
#[cfg(target_os = "linux")]
pub fn take_pam_exec_context() -> Result<Option<SessionSecurityContext>, SelinuxError> {
    // `/proc/*/attr/exec` belongs to whichever LSM owns it (AppArmor on
    // Ubuntu), so it is read as an SELinux context only where selinuxfs
    // shows SELinux enabled, as `is_selinux_enabled` does.
    if !std::path::Path::new(SELINUXFS).join("enforce").exists() {
        return Ok(None);
    }
    let raw = match std::fs::read(EXEC_ATTR_PATH) {
        Ok(raw) => raw,
        // No SELinux (disabled, or another LSM owns the attribute): PAM
        // cannot have set an SELinux exec context.
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(nix::libc::EINVAL | nix::libc::ENOENT | nix::libc::EOPNOTSUPP)
            ) =>
        {
            return Ok(None);
        }
        Err(_) => return Err(SelinuxError::Read),
    };
    let Some(context) = SessionSecurityContext::parse(&raw)? else {
        return Ok(None);
    };
    clear_exec_context()?;
    Ok(Some(context))
}

/// Resets this thread's exec context: an empty write, as `setexeccon(NULL)`.
#[cfg(target_os = "linux")]
fn clear_exec_context() -> Result<(), SelinuxError> {
    use std::os::fd::AsRawFd;

    let attribute = std::fs::OpenOptions::new()
        .write(true)
        .open(EXEC_ATTR_PATH)
        .map_err(|_| SelinuxError::Clear)?;
    // SAFETY: `attribute` is an open descriptor for the call's duration; a
    // zero-length write reads no memory. `std`'s writers skip empty writes,
    // so the syscall is made directly.
    let written = unsafe { nix::libc::write(attribute.as_raw_fd(), c"".as_ptr().cast(), 0) };
    if written != 0 {
        return Err(SelinuxError::Clear);
    }
    match std::fs::read(EXEC_ATTR_PATH) {
        Ok(raw) if SessionSecurityContext::parse(&raw).is_ok_and(|context| context.is_none()) => {
            Ok(())
        }
        _ => Err(SelinuxError::Clear),
    }
}

/// This thread's current context, when SELinux reports one.
#[cfg(target_os = "linux")]
pub fn current_context() -> Option<SessionSecurityContext> {
    let raw = std::fs::read(CURRENT_ATTR).ok()?;
    SessionSecurityContext::parse(&raw).ok().flatten()
}

/// Whether the loaded policy lets this process's domain become `target` on
/// `execve` (`process:transition`), asked of the kernel the way
/// `security_compute_av` does. `None` when the question cannot be answered
/// (no selinuxfs, or the query itself is not permitted); the `execve` then
/// decides, fail-closed.
#[cfg(target_os = "linux")]
pub fn transition_decision(target: &SessionSecurityContext) -> Option<AccessDecision> {
    use std::io::{Read, Write};

    let enforcing = std::fs::read_to_string(format!("{SELINUXFS}/enforce")).ok()?;
    if enforcing.trim() == "0" {
        return Some(AccessDecision::Allowed);
    }
    let source = current_context()?;
    let class = std::fs::read_to_string(format!("{SELINUXFS}/class/process/index"))
        .ok()?
        .trim()
        .parse::<u16>()
        .ok()?;
    let permission = std::fs::read_to_string(format!("{SELINUXFS}/class/process/perms/transition"))
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()?;
    let mut access = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!("{SELINUXFS}/access"))
        .ok()?;
    access
        .write_all(format!("{source} {target} {class}").as_bytes())
        .ok()?;
    let mut response = String::new();
    access.read_to_string(&mut response).ok()?;
    decode_access_response(&response, permission)
}

/// Sets this (forked, single-threaded) child's exec context so its next
/// `execve` enters `context`. For `pre_exec` only: it allocates nothing and
/// makes only async-signal-safe calls. Must run before the credential drop,
/// which makes the process's `/proc` attributes root-owned.
#[cfg(target_os = "linux")]
pub(crate) fn apply_exec_context_in_child(context: &CStr) -> std::io::Result<()> {
    let bytes = context.to_bytes_with_nul();
    // SAFETY: `EXEC_ATTR` and `bytes` are valid NUL-terminated/sized buffers
    // that outlive the calls; open/write/close are async-signal-safe.
    unsafe {
        let fd = nix::libc::open(
            EXEC_ATTR.as_ptr(),
            nix::libc::O_WRONLY | nix::libc::O_CLOEXEC,
        );
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let written = nix::libc::write(fd, bytes.as_ptr().cast(), bytes.len());
        let error = std::io::Error::last_os_error();
        nix::libc::close(fd);
        if written < 0 {
            return Err(error);
        }
        if written as usize != bytes.len() {
            return Err(std::io::Error::other("short SELinux exec context write"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_or_terminator_only_attributes_mean_no_context() {
        assert_eq!(SessionSecurityContext::parse(b"").unwrap(), None);
        assert_eq!(SessionSecurityContext::parse(b"\0").unwrap(), None);
        assert_eq!(SessionSecurityContext::parse(b"\n").unwrap(), None);
    }

    #[test]
    fn pam_selinux_contexts_parse_with_or_without_terminator() {
        let context = SessionSecurityContext::parse(b"unconfined_u:unconfined_r:unconfined_t:s0\0")
            .unwrap()
            .unwrap();
        assert_eq!(
            context.as_str(),
            "unconfined_u:unconfined_r:unconfined_t:s0"
        );
        let ranged =
            SessionSecurityContext::parse(b"unconfined_u:unconfined_r:unconfined_t:s0-s0:c0.c1023")
                .unwrap()
                .unwrap();
        assert_eq!(
            ranged.as_c_str().to_bytes(),
            b"unconfined_u:unconfined_r:unconfined_t:s0-s0:c0.c1023"
        );
        assert!(SessionSecurityContext::parse(b"user_u:user_r:user_t")
            .unwrap()
            .is_some());
    }

    #[test]
    fn malformed_contexts_are_refused_rather_than_dropped() {
        for raw in [
            &b"unconfined_t"[..],
            b"unconfined_u:unconfined_r",
            b"unconfined_u::unconfined_t:s0",
            b"unconfined_u:unconfined_r:unconfined_t:s0 extra",
            b"unconfined_u:unconfined_r:\0unconfined_t",
            b"unconfined_u:unconfined_r:unconfined_t:s\xc3\xa9",
        ] {
            assert!(
                SessionSecurityContext::parse(raw).is_err(),
                "{:?}",
                String::from_utf8_lossy(raw)
            );
        }
        let oversized = format!("u:r:{}", "t".repeat(2048));
        assert!(SessionSecurityContext::parse(oversized.as_bytes()).is_err());
    }

    #[test]
    fn access_responses_decode_the_requested_permission() {
        // `transition` is permission 2 of `process` in the reference policy.
        assert_eq!(
            decode_access_response("ffffffff ffffffff 0 ffffffff 12 0", 2),
            Some(AccessDecision::Allowed)
        );
        assert_eq!(
            decode_access_response("fffffffd ffffffff 0 ffffffff 12 0", 2),
            Some(AccessDecision::Denied)
        );
        // A permissive domain is not blocked.
        assert_eq!(
            decode_access_response("0 ffffffff 0 ffffffff 12 1", 2),
            Some(AccessDecision::Allowed)
        );
        // Undecided, malformed or impossible permission indices are unknown.
        assert_eq!(decode_access_response("ffffffff 0 0 0 12 0", 2), None);
        assert_eq!(decode_access_response("zz ffffffff", 2), None);
        assert_eq!(decode_access_response("ffffffff ffffffff", 0), None);
        assert_eq!(decode_access_response("ffffffff ffffffff", 33), None);
        // An old kernel without the flags field still decodes.
        assert_eq!(
            decode_access_response("2 2 0 2 7", 2),
            Some(AccessDecision::Allowed)
        );
    }
}
