// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! `pam_arcen_greeter.so`: the login screen's half of a remote-first
//! sign-in (ADR 0025).
//!
//! Loaded only by the `gdm-ovirtcred` stack Arcen installs. GNOME's login
//! screen starts that stack when the Pier sends it a one-time token over
//! the system bus, and answers this module's one secret prompt with the
//! token. The module finds the token's record by the token's SHA-256 in a
//! root-only directory, removes it (a token is spent by its first use,
//! admitted or not), checks it with the shared rules
//! (`arcen_session::console_session::greeter_token`) and sets `PAM_USER`.
//! Everything else the login needs — account policy, the session, the
//! keyring — is the rest of the stack's, exactly as for a password sign-in.
//!
//! The module knows no password and stores nothing. A process that is not
//! root cannot read the records, so presenting a token through any other
//! PAM client admits nobody.

#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]
#![allow(
    unsafe_code,
    reason = "a PAM module is a C ABI: libpam's handle and conversation, and the \
              descriptor-relative file calls that keep the record lookup race-free"
)]

use std::ffi::{c_int, CString};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use arcen_session::console_session::greeter_token::{
    admit, record_dir_trustworthy, record_file_trustworthy, record_name, GreeterTokenRecord,
    TokenRejection, MAX_RECORD_BYTES,
};

/// Closes a descriptor when dropped.
struct Fd(c_int);

impl Drop for Fd {
    fn drop(&mut self) {
        // SAFETY: the descriptor was returned by `open`/`openat` and is
        // closed exactly once, here.
        unsafe {
            libc::close(self.0);
        }
    }
}

fn fstat(fd: c_int) -> Option<libc::stat> {
    // SAFETY: `stat` is plain old data; `fstat` fills it for a valid
    // descriptor and the result is used only when it succeeded.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        (libc::fstat(fd, std::ptr::addr_of_mut!(st)) == 0).then_some(st)
    }
}

/// Spends the record of `token` in `dir`, whose directory and file must be
/// `trusted_owner`'s alone (root in the module), and returns the user it
/// admits at `now`. The record is removed before its contents are believed,
/// so every presented token that has a record is spent.
///
/// # Errors
///
/// The [`TokenRejection`] that refuses it.
pub fn consume_record(
    dir: &Path,
    trusted_owner: u32,
    token: &str,
    now: u64,
    resolve_uid: impl Fn(&str) -> Option<u32>,
) -> Result<String, TokenRejection> {
    let name = record_name(token).ok_or(TokenRejection::Unknown)?;
    let name = CString::new(name).map_err(|_| TokenRejection::Unknown)?;
    let dir = CString::new(dir.as_os_str().as_bytes()).map_err(|_| TokenRejection::Unknown)?;
    // SAFETY: a NUL-terminated path; the descriptor is owned by `Fd`.
    let dirfd = unsafe {
        libc::open(
            dir.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if dirfd < 0 {
        return Err(TokenRejection::Unknown);
    }
    let dirfd = Fd(dirfd);
    let dir_stat = fstat(dirfd.0).ok_or(TokenRejection::Unknown)?;
    #[allow(clippy::useless_conversion, reason = "mode_t is u16 on some targets")]
    let mode = u32::from(dir_stat.st_mode);
    #[allow(clippy::useless_conversion, reason = "mode_t is u16 on some targets")]
    let (type_mask, directory, regular) = (
        u32::from(libc::S_IFMT),
        u32::from(libc::S_IFDIR),
        u32::from(libc::S_IFREG),
    );
    if !record_dir_trustworthy(
        mode & type_mask == directory,
        dir_stat.st_uid,
        trusted_owner,
        mode,
    ) {
        return Err(TokenRejection::UntrustedRecord);
    }
    // SAFETY: `dirfd` is an open directory and `name` a NUL-terminated
    // single path component (hexadecimal only).
    let fd = unsafe {
        libc::openat(
            dirfd.0,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(TokenRejection::Unknown);
    }
    let fd = Fd(fd);
    // Read before it is unlinked: afterwards it has no link left to count.
    let file_stat = fstat(fd.0);
    // SAFETY: as above; spending the record is what makes it single-use.
    let removed = unsafe { libc::unlinkat(dirfd.0, name.as_ptr(), 0) } == 0;
    let file_stat = file_stat.ok_or(TokenRejection::Unknown)?;
    #[allow(clippy::useless_conversion, reason = "mode_t is u16 on some targets")]
    let mode = u32::from(file_stat.st_mode);
    #[allow(
        clippy::useless_conversion,
        reason = "nlink_t and off_t differ by target"
    )]
    let trusted = record_file_trustworthy(
        mode & type_mask == regular,
        file_stat.st_uid,
        trusted_owner,
        mode,
        u64::from(file_stat.st_nlink),
        u64::try_from(file_stat.st_size).unwrap_or(u64::MAX),
    );
    if !removed {
        return Err(TokenRejection::Unknown);
    }
    if !trusted {
        return Err(TokenRejection::UntrustedRecord);
    }
    let mut buffer = [0_u8; MAX_RECORD_BYTES + 1];
    // SAFETY: reads at most the buffer's length into the buffer.
    let read = unsafe { libc::read(fd.0, buffer.as_mut_ptr().cast(), buffer.len()) };
    let read = usize::try_from(read).map_err(|_| TokenRejection::Unknown)?;
    let record = GreeterTokenRecord::parse(&buffer[..read]).ok_or(TokenRejection::Unknown)?;
    admit(&record, now, resolve_uid(&record.username))?;
    Ok(record.username)
}

/// The libpam side: the conversation, `PAM_USER`, the account lookup and
/// the exported entry points. Linux only; the record logic above is
/// portable so its tests run anywhere.
#[cfg(target_os = "linux")]
pub mod pam {
    use std::ffi::{c_char, c_int, c_void, CString};
    use std::path::Path;

    use arcen_session::console_session::greeter_token::RECORD_DIR;

    use super::consume_record;

    const PAM_SUCCESS: c_int = 0;
    const PAM_AUTH_ERR: c_int = 7;
    const PAM_USER: c_int = 2;
    const PAM_CONV: c_int = 5;
    const PAM_PROMPT_ECHO_OFF: c_int = 1;
    const LOG_NOTICE: c_int = 5;
    const MAX_TOKEN_BYTES: usize = 256;

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

    type ConversationFn = unsafe extern "C" fn(
        c_int,
        *mut *const PamMessage,
        *mut *mut PamResponse,
        *mut c_void,
    ) -> c_int;

    #[repr(C)]
    struct PamConv {
        conv: Option<ConversationFn>,
        appdata_ptr: *mut c_void,
    }

    #[cfg(target_os = "linux")]
    #[link(name = "pam")]
    extern "C" {
        fn pam_get_item(pamh: *const c_void, item_type: c_int, item: *mut *const c_void) -> c_int;
        fn pam_set_item(pamh: *mut c_void, item_type: c_int, item: *const c_void) -> c_int;
        fn pam_syslog(pamh: *const c_void, priority: c_int, fmt: *const c_char, ...);
    }

    /// The uid the system resolves `name` to (NSS: local files or a directory).
    fn resolve_uid(name: &str) -> Option<u32> {
        let name = CString::new(name).ok()?;
        let mut buffer = vec![0 as c_char; 16 * 1024];
        // SAFETY: `passwd` is plain old data filled by getpwnam_r, which writes
        // its strings into `buffer`; only `pw_uid` is read, and only when an
        // entry was found.
        unsafe {
            let mut entry: libc::passwd = std::mem::zeroed();
            let mut found: *mut libc::passwd = std::ptr::null_mut();
            let status = libc::getpwnam_r(
                name.as_ptr(),
                std::ptr::addr_of_mut!(entry),
                buffer.as_mut_ptr(),
                buffer.len(),
                std::ptr::addr_of_mut!(found),
            );
            (status == 0 && !found.is_null()).then_some(entry.pw_uid)
        }
    }

    fn now_epoch() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs())
    }

    /// Overwrites and frees a C string the application allocated.
    ///
    /// # Safety
    ///
    /// `text` is null or a NUL-terminated string from `malloc`.
    unsafe fn scrub_and_free(text: *mut c_char) {
        if text.is_null() {
            return;
        }
        let length = libc::strlen(text);
        for index in 0..length {
            std::ptr::write_volatile(text.add(index), 0);
        }
        libc::free(text.cast());
    }

    /// Asks the application's conversation for one secret answer.
    ///
    /// # Safety
    ///
    /// `pamh` is the live PAM handle this module was called with.
    unsafe fn ask_token(pamh: *mut c_void) -> Option<String> {
        let mut item: *const c_void = std::ptr::null();
        if pam_get_item(pamh, PAM_CONV, std::ptr::addr_of_mut!(item)) != PAM_SUCCESS
            || item.is_null()
        {
            return None;
        }
        let conversation = &*item.cast::<PamConv>();
        let conv = conversation.conv?;
        let prompt = c"Token:";
        let message = PamMessage {
            msg_style: PAM_PROMPT_ECHO_OFF,
            msg: prompt.as_ptr(),
        };
        let mut messages = [std::ptr::addr_of!(message)];
        let mut response: *mut PamResponse = std::ptr::null_mut();
        let status = conv(
            1,
            messages.as_mut_ptr(),
            std::ptr::addr_of_mut!(response),
            conversation.appdata_ptr,
        );
        if response.is_null() {
            return None;
        }
        let answer = (*response).resp;
        let token = (status == PAM_SUCCESS && !answer.is_null()).then(|| {
            let length = libc::strnlen(answer, MAX_TOKEN_BYTES + 1);
            let bytes = std::slice::from_raw_parts(answer.cast::<u8>(), length);
            String::from_utf8_lossy(bytes).into_owned()
        });
        scrub_and_free(answer);
        libc::free(response.cast());
        token.filter(|token| token.len() <= MAX_TOKEN_BYTES)
    }

    /// Logs one line through the application's syslog identity.
    ///
    /// # Safety
    ///
    /// `pamh` is the live PAM handle this module was called with.
    unsafe fn log(pamh: *const c_void, line: &str) {
        if let Ok(line) = CString::new(line) {
            pam_syslog(pamh, LOG_NOTICE, c"%s".as_ptr(), line.as_ptr());
        }
    }

    /// The user already named on the handle, if any (a login screen that knows
    /// whom it verifies); a token for anyone else admits nobody.
    ///
    /// # Safety
    ///
    /// `pamh` is the live PAM handle this module was called with.
    unsafe fn named_user(pamh: *const c_void) -> Option<String> {
        let mut item: *const c_void = std::ptr::null();
        if pam_get_item(pamh, PAM_USER, std::ptr::addr_of_mut!(item)) != PAM_SUCCESS
            || item.is_null()
        {
            return None;
        }
        Some(
            std::ffi::CStr::from_ptr(item.cast())
                .to_string_lossy()
                .into_owned(),
        )
    }

    unsafe fn authenticate(pamh: *mut c_void) -> c_int {
        let Some(mut token) = ask_token(pamh) else {
            log(pamh, "pam_arcen_greeter: no token in the conversation");
            return PAM_AUTH_ERR;
        };
        let outcome = consume_record(Path::new(RECORD_DIR), 0, &token, now_epoch(), resolve_uid);
        // The token is spent either way; do not keep it in memory.
        token.clear();
        token.shrink_to_fit();
        let user = match outcome {
            Ok(user) => user,
            Err(rejection) => {
                log(
                    pamh,
                    &format!("pam_arcen_greeter: token refused ({})", rejection.token()),
                );
                return PAM_AUTH_ERR;
            }
        };
        if named_user(pamh).is_some_and(|named| named != user) {
            log(
                pamh,
                "pam_arcen_greeter: token refused (another user is being verified)",
            );
            return PAM_AUTH_ERR;
        }
        let Ok(user_c) = CString::new(user.as_str()) else {
            return PAM_AUTH_ERR;
        };
        if pam_set_item(pamh, PAM_USER, user_c.as_ptr().cast()) != PAM_SUCCESS {
            return PAM_AUTH_ERR;
        }
        log(
            pamh,
            &format!("pam_arcen_greeter: one-time Arcen sign-in accepted for {user}"),
        );
        PAM_SUCCESS
    }

    /// PAM `auth`: a one-time Arcen token becomes the user.
    ///
    /// # Safety
    ///
    /// Called by libpam with its live handle.
    #[no_mangle]
    pub unsafe extern "C" fn pam_sm_authenticate(
        pamh: *mut c_void,
        _flags: c_int,
        _argc: c_int,
        _argv: *const *const c_char,
    ) -> c_int {
        std::panic::catch_unwind(|| authenticate(pamh)).unwrap_or(PAM_AUTH_ERR)
    }

    /// PAM `auth` credentials: nothing to establish.
    #[no_mangle]
    pub extern "C" fn pam_sm_setcred(
        _pamh: *mut c_void,
        _flags: c_int,
        _argc: c_int,
        _argv: *const *const c_char,
    ) -> c_int {
        PAM_SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_session::console_session::greeter_token::{token_text, LIFETIME_SECS};
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};

    const NOW: u64 = 1_800_000_000;

    fn owner() -> u32 {
        // SAFETY: getuid cannot fail.
        unsafe { libc::getuid() }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("arcen-pam-greeter-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        dir
    }

    fn plant(dir: &Path, token: &str, record: &GreeterTokenRecord, mode: u32) {
        let path = dir.join(record_name(token).unwrap());
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(path)
            .unwrap();
        std::io::Write::write_all(&mut file, record.encode().as_bytes()).unwrap();
    }

    #[test]
    fn a_token_admits_its_user_once() {
        let dir = scratch("once");
        let token = token_text(&[7; 32]);
        let record = GreeterTokenRecord::issue("tim", 1001, NOW).unwrap();
        plant(&dir, &token, &record, 0o600);
        let resolve = |name: &str| (name == "tim").then_some(1001);
        assert_eq!(
            consume_record(&dir, owner(), &token, NOW + 1, resolve),
            Ok("tim".to_owned())
        );
        assert_eq!(
            consume_record(&dir, owner(), &token, NOW + 1, resolve),
            Err(TokenRejection::Unknown),
            "spent"
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_refused_token_is_still_spent() {
        let dir = scratch("spent");
        let token = token_text(&[8; 32]);
        let record = GreeterTokenRecord::issue("tim", 1001, NOW).unwrap();
        plant(&dir, &token, &record, 0o600);
        assert_eq!(
            consume_record(&dir, owner(), &token, NOW + LIFETIME_SECS + 1, |_| Some(
                1001
            )),
            Err(TokenRejection::Expired)
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let loose = token_text(&[9; 32]);
        plant(&dir, &loose, &record, 0o644);
        assert_eq!(
            consume_record(&dir, owner(), &loose, NOW, |_| Some(1001)),
            Err(TokenRejection::UntrustedRecord)
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_shared_directory_a_symlink_or_another_owner_admits_nobody() {
        let dir = scratch("dir");
        let token = token_text(&[10; 32]);
        let record = GreeterTokenRecord::issue("tim", 1001, NOW).unwrap();
        plant(&dir, &token, &record, 0o600);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            consume_record(&dir, owner(), &token, NOW, |_| Some(1001)),
            Err(TokenRejection::UntrustedRecord)
        );
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            consume_record(&dir, owner().wrapping_add(1), &token, NOW, |_| Some(1001)),
            Err(TokenRejection::UntrustedRecord)
        );
        let link = dir.with_extension("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert_eq!(
            consume_record(&link, owner(), &token, NOW, |_| Some(1001)),
            Err(TokenRejection::Unknown)
        );
        std::fs::remove_file(link).unwrap();
        assert_eq!(
            consume_record(&dir, owner(), "../../etc/shadow", NOW, |_| Some(1001)),
            Err(TokenRejection::Unknown)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_account_that_changed_is_refused() {
        let dir = scratch("account");
        let token = token_text(&[11; 32]);
        let record = GreeterTokenRecord::issue("tim", 1001, NOW).unwrap();
        plant(&dir, &token, &record, 0o600);
        assert_eq!(
            consume_record(&dir, owner(), &token, NOW, |_| Some(1002)),
            Err(TokenRejection::AccountChanged)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
