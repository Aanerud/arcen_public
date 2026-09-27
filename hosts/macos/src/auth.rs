#![allow(unsafe_code)]

//! Native macOS account authentication.
//!
//! A client that reaches the port and pins the certificate has proved it is
//! talking to the right machine. It has not proved anybody is allowed to use
//! it. This module is what stands between the two.
//!
//! Authentication goes through PAM, which on macOS is backed by
//! `pam_opendirectory` and therefore honours local accounts, directory
//! accounts, disabled accounts and password policy without Arcen reimplementing
//! any of it.
//!
//! The service name is Arcen's own rather than a borrowed one. Reusing
//! `login` or `sshd` would inherit session modules that start launchd sessions
//! and mount home directories as a side effect of a password check. If the
//! service file is absent, PAM falls back to `other`, which on macOS is
//! `pam_deny` — a host with no policy installed refuses everyone rather than
//! admitting everyone.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;

use serde::Serialize;

/// PAM service Arcen authenticates against.
///
/// Installed as `/etc/pam.d/arcen`. Absent, PAM uses `other`, which denies.
pub const PAM_SERVICE: &str = "arcen";

const PAM_SUCCESS: c_int = 0;
const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_PROMPT_ECHO_ON: c_int = 2;
const PAM_CONV_ERR: c_int = 19;
/// `pam_get_item` selector for the account PAM settled on.
const PAM_USER: c_int = 2;
/// Refuse accounts with no password rather than treating blank as valid.
const PAM_DISALLOW_NULL_AUTHTOK: c_int = 0x0001;

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}

#[repr(C)]
struct PamConv {
    conv: Option<
        unsafe extern "C" fn(
            num_msg: c_int,
            msg: *mut *const PamMessage,
            resp: *mut *mut PamResponse,
            appdata: *mut c_void,
        ) -> c_int,
    >,
    appdata_ptr: *mut c_void,
}

#[link(name = "pam")]
unsafe extern "C" {
    fn pam_start(
        service: *const c_char,
        user: *const c_char,
        conv: *const PamConv,
        handle: *mut *mut c_void,
    ) -> c_int;
    fn pam_authenticate(handle: *mut c_void, flags: c_int) -> c_int;
    fn pam_acct_mgmt(handle: *mut c_void, flags: c_int) -> c_int;
    fn pam_end(handle: *mut c_void, status: c_int) -> c_int;
    fn pam_get_item(handle: *const c_void, item_type: c_int, item: *mut *const c_void) -> c_int;
    fn pam_strerror(handle: *mut c_void, code: c_int) -> *const c_char;
}

#[link(name = "c")]
unsafe extern "C" {
    fn calloc(count: usize, size: usize) -> *mut c_void;
    fn strdup(source: *const c_char) -> *mut c_char;
    fn getpwnam_r(
        name: *const c_char,
        record: *mut Passwd,
        buffer: *mut c_char,
        length: usize,
        result: *mut *mut Passwd,
    ) -> c_int;
}

/// `struct passwd` as Darwin lays it out.
#[repr(C)]
#[allow(clippy::struct_field_names)]
struct Passwd {
    pw_name: *mut c_char,
    pw_passwd: *mut c_char,
    pw_uid: u32,
    pw_gid: u32,
    pw_change: i64,
    pw_class: *mut c_char,
    pw_gecos: *mut c_char,
    pw_dir: *mut c_char,
    pw_shell: *mut c_char,
    pw_expire: i64,
}

/// An account as the system names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// The short name.
    pub name: String,
    /// The numeric identity.
    pub uid: u32,
}

/// Looks an account up by any name the directory answers to.
///
/// Directory services answer for aliases as well as short names, and the record
/// that comes back carries the short name and uid. Those are what can be
/// compared with the console owner and the agent's own account; the string a
/// client typed cannot.
#[must_use]
pub fn resolve_account(name: &str) -> Option<Account> {
    let name = CString::new(name).ok()?;
    let mut record = std::mem::MaybeUninit::<Passwd>::zeroed();
    let mut buffer = vec![0 as c_char; 16 * 1024];
    let mut result: *mut Passwd = ptr::null_mut();
    // SAFETY: every pointer is live for the call and `buffer` is as long as
    // stated. `getpwnam_r` writes only into `record` and `buffer`.
    let status = unsafe {
        getpwnam_r(
            name.as_ptr(),
            record.as_mut_ptr(),
            buffer.as_mut_ptr(),
            buffer.len(),
            &raw mut result,
        )
    };
    if status != 0 || result.is_null() {
        return None;
    }
    // SAFETY: a non-null result points at `record`, now initialised, whose
    // strings live in `buffer`.
    let record = unsafe { record.assume_init_ref() };
    if record.pw_name.is_null() {
        return None;
    }
    // SAFETY: `pw_name` is a NUL-terminated string inside `buffer`.
    let short = unsafe { CStr::from_ptr(record.pw_name) }
        .to_string_lossy()
        .into_owned();
    Some(Account {
        name: short,
        uid: record.pw_uid,
    })
}

/// The password, carried only as long as the conversation needs it.
struct Secret {
    password: CString,
}

/// Supplies the password to PAM.
///
/// # Safety
///
/// Called by PAM with its own message array; `appdata` is the [`Secret`] handed
/// to `pam_start`.
unsafe extern "C" fn converse(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata: *mut c_void,
) -> c_int {
    if num_msg <= 0 || msg.is_null() || resp.is_null() || appdata.is_null() {
        return PAM_CONV_ERR;
    }
    let count = usize::try_from(num_msg).unwrap_or(0);
    // SAFETY: PAM owns `appdata` for the lifetime of the transaction.
    let secret = unsafe { &*appdata.cast::<Secret>() };

    // PAM frees this array, so it must come from the allocator PAM expects.
    let responses = unsafe { calloc(count, size_of::<PamResponse>()) }.cast::<PamResponse>();
    if responses.is_null() {
        return PAM_CONV_ERR;
    }

    for index in 0..count {
        // SAFETY: PAM guarantees `num_msg` entries.
        let message = unsafe { *msg.add(index) };
        if message.is_null() {
            continue;
        }
        // SAFETY: the entry is a live message for this call.
        let style = unsafe { (*message).msg_style };
        if style == PAM_PROMPT_ECHO_OFF || style == PAM_PROMPT_ECHO_ON {
            // SAFETY: duplicated with the allocator PAM frees with.
            let copy = unsafe { strdup(secret.password.as_ptr()) };
            if copy.is_null() {
                return PAM_CONV_ERR;
            }
            // SAFETY: `responses` has `count` initialised entries.
            unsafe { (*responses.add(index)).resp = copy };
        }
        // Informational and error messages need no answer. They are not
        // forwarded anywhere: PAM text can name accounts and policy.
    }

    // SAFETY: the caller owns the array from here.
    unsafe { *resp = responses };
    PAM_SUCCESS
}

/// Why a client was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthFailure {
    /// The username or password was wrong.
    ///
    /// Deliberately one reason. Telling a caller which half was wrong tells an
    /// attacker which usernames exist.
    InvalidCredentials,
    /// The account exists and the password was right, but the account may not
    /// be used: expired, disabled, or outside the permitted set.
    AccountNotPermitted,
    /// The username or password could not be represented, most often an
    /// embedded NUL.
    MalformedInput,
    /// PAM itself failed to start.
    PolicyUnavailable,
    /// The account is valid, but this Mac's screen belongs to another one.
    ///
    /// Only said after the password was proved, so it reveals nothing an
    /// attacker without the credential could learn.
    NotConsoleOwner,
    /// Too many recent failures from this address or for this account; the
    /// password was not checked.
    TooManyAttempts,
    /// The account is valid, but another session already holds this host.
    HostBusy,
}

impl AuthFailure {
    /// Returns a stable code safe to log.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidCredentials => "invalid_credentials",
            Self::AccountNotPermitted => "account_not_permitted",
            Self::MalformedInput => "malformed_input",
            Self::PolicyUnavailable => "policy_unavailable",
            Self::NotConsoleOwner => "not_console_owner",
            Self::TooManyAttempts => "too_many_attempts",
            Self::HostBusy => "host_busy",
        }
    }
}

impl std::fmt::Display for AuthFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            // What a client is told. It never learns which half was wrong.
            Self::InvalidCredentials => "the username or password is incorrect",
            Self::AccountNotPermitted => "this account may not sign in here",
            Self::MalformedInput => "the username or password is not valid text",
            Self::PolicyUnavailable => "authentication policy is unavailable on this host",
            Self::NotConsoleOwner => {
                "this Mac's screen belongs to another account; sign in as the person at the \
                 console, or switch to your account at the machine first"
            }
            Self::TooManyAttempts => {
                "too many failed sign-ins; wait a few minutes before trying again"
            }
            Self::HostBusy => "another session is already using this Mac",
        })
    }
}

impl std::error::Error for AuthFailure {}

/// An authenticated account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authenticated {
    /// The account PAM accepted, by its short name — not whatever alias the
    /// client typed.
    pub user: String,
    /// Its numeric identity, when the directory could resolve one.
    pub uid: Option<u32>,
}

/// Authenticates `user` with `password` against the local account database.
///
/// The password is not logged, not returned, and is released as soon as PAM is
/// finished with it.
///
/// # Errors
///
/// Returns an [`AuthFailure`] describing only what a client may safely learn.
pub fn authenticate(user: &str, password: &str) -> Result<Authenticated, AuthFailure> {
    // A NUL would silently truncate the credential and could authenticate a
    // different string than the one supplied.
    let service = CString::new(PAM_SERVICE).map_err(|_| AuthFailure::PolicyUnavailable)?;
    let user_c = CString::new(user).map_err(|_| AuthFailure::MalformedInput)?;
    let secret = Box::new(Secret {
        password: CString::new(password).map_err(|_| AuthFailure::MalformedInput)?,
    });

    let conv = PamConv {
        conv: Some(converse),
        appdata_ptr: std::ptr::from_ref(secret.as_ref())
            .cast::<c_void>()
            .cast_mut(),
    };
    let mut handle: *mut c_void = ptr::null_mut();

    // SAFETY: all pointers are live for the duration of the call.
    let started = unsafe {
        pam_start(
            service.as_ptr(),
            user_c.as_ptr(),
            &raw const conv,
            &raw mut handle,
        )
    };
    if started != PAM_SUCCESS || handle.is_null() {
        return Err(AuthFailure::PolicyUnavailable);
    }

    // SAFETY: `handle` is a live transaction until `pam_end`.
    let authenticated = unsafe { pam_authenticate(handle, PAM_DISALLOW_NULL_AUTHTOK) };
    let account = if authenticated == PAM_SUCCESS {
        // A correct password is not permission to use the machine: expiry and
        // access policy are a separate question PAM answers here.
        // SAFETY: same live transaction.
        unsafe { pam_acct_mgmt(handle, PAM_DISALLOW_NULL_AUTHTOK) }
    } else {
        authenticated
    };

    // The name PAM settled on, which a module may have rewritten from the one
    // supplied. Read before `pam_end`, which frees it.
    let settled = if account == PAM_SUCCESS {
        let mut item: *const c_void = ptr::null();
        // SAFETY: same live transaction; PAM owns the returned string.
        let status = unsafe { pam_get_item(handle, PAM_USER, &raw mut item) };
        (status == PAM_SUCCESS && !item.is_null()).then(|| {
            // SAFETY: `PAM_USER` is a NUL-terminated string owned by PAM
            // until `pam_end`.
            unsafe { CStr::from_ptr(item.cast::<c_char>()) }
                .to_string_lossy()
                .into_owned()
        })
    } else {
        None
    };

    // SAFETY: ends the transaction and releases PAM's state.
    unsafe { pam_end(handle, account) };
    drop(secret);

    if authenticated != PAM_SUCCESS {
        return Err(AuthFailure::InvalidCredentials);
    }
    if account != PAM_SUCCESS {
        return Err(AuthFailure::AccountNotPermitted);
    }
    let settled = settled.unwrap_or_else(|| user.to_owned());
    Ok(match resolve_account(&settled) {
        Some(resolved) => Authenticated {
            user: resolved.name,
            uid: Some(resolved.uid),
        },
        None => Authenticated {
            user: settled,
            uid: None,
        },
    })
}

/// Returns PAM's own description of a code, for diagnostics only.
///
/// Never sent to a client: PAM text can name accounts and policy.
#[must_use]
pub fn describe(code: c_int) -> String {
    // SAFETY: a null handle is valid for `pam_strerror`.
    let text = unsafe { pam_strerror(ptr::null_mut(), code) };
    if text.is_null() {
        return format!("PAM code {code}");
    }
    // SAFETY: PAM returns a NUL-terminated static string.
    unsafe { CStr::from_ptr(text) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wrong_password_is_refused() {
        // The account exists on every Mac; the password does not.
        let outcome = authenticate("root", "definitely-not-the-password");
        assert!(
            matches!(
                outcome,
                Err(AuthFailure::InvalidCredentials | AuthFailure::AccountNotPermitted)
            ),
            "a wrong password must be refused, got {outcome:?}"
        );
    }

    #[test]
    fn an_unknown_account_is_refused_the_same_way_as_a_wrong_password() {
        // Distinguishing the two would tell an attacker which usernames exist.
        let unknown = authenticate("no-such-account-cf8a21", "anything");
        let wrong = authenticate("root", "definitely-not-the-password");
        assert!(unknown.is_err());
        assert!(wrong.is_err());
        assert_eq!(
            unknown.unwrap_err().as_str(),
            wrong.unwrap_err().as_str(),
            "an unknown account and a wrong password must look identical"
        );
    }

    #[test]
    fn an_alias_resolves_to_the_short_name_and_uid() {
        let root = resolve_account("root").expect("root exists on every Mac");
        assert_eq!(root.uid, 0);
        assert_eq!(root.name, "root");
        assert!(resolve_account("no-such-account-cf8a21").is_none());
        assert!(resolve_account("nul\0byte").is_none());
    }

    #[test]
    fn an_empty_password_is_refused() {
        // `PAM_DISALLOW_NULL_AUTHTOK`: a blank password is never a valid one.
        assert!(authenticate("root", "").is_err());
    }

    #[test]
    fn embedded_nuls_are_rejected_rather_than_truncated() {
        // Truncating would authenticate a different string than was supplied.
        assert_eq!(
            authenticate("us\0er", "password"),
            Err(AuthFailure::MalformedInput)
        );
        assert_eq!(
            authenticate("someone", "pass\0word"),
            Err(AuthFailure::MalformedInput)
        );
    }

    #[test]
    fn failures_carry_a_stable_code_and_a_message_that_reveals_nothing() {
        for failure in [
            AuthFailure::InvalidCredentials,
            AuthFailure::AccountNotPermitted,
            AuthFailure::MalformedInput,
            AuthFailure::PolicyUnavailable,
            AuthFailure::NotConsoleOwner,
            AuthFailure::TooManyAttempts,
            AuthFailure::HostBusy,
        ] {
            assert!(!failure.as_str().is_empty());
            let message = failure.to_string();
            assert!(!message.is_empty());
            // No message may hint at which half of a credential was wrong.
            assert!(!message.contains("username is"));
            assert!(!message.contains("no such"));
        }
    }
}
