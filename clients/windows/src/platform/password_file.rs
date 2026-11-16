// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Opening a `--password-file` safely, the Windows half of the macOS
//! Deck's `credentials.rs::open_password_file`.
//!
//! The Mac refuses a symbolic link, anything but a regular file, a file
//! another user owns and any mode broader than `0600`. Windows has no mode
//! bits, so the same promise is made from the file's security descriptor,
//! read from the handle that will be read (no check-then-open race):
//!
//! * the path itself is opened (`FILE_FLAG_OPEN_REPARSE_POINT`) and a
//!   reparse point (symbolic link, junction, cloud placeholder) is
//!   refused, as is anything that is not a regular disk file;
//! * the owner must be the current user, or the token's default owner
//!   for new files (an elevated administrator's files are owned by
//!   `Administrators`);
//! * the DACL must exist and allow access to nobody but the current user,
//!   `SYSTEM` and `Administrators` (who can read any file anyway, like
//!   root on the Mac). Inherit-only entries do not apply to the file and
//!   deny entries only narrow access, so both are ignored; an allow entry
//!   of a kind this check cannot read is refused.
//!
//! Reading, the size limit and the content rules stay with the shared
//! credential parser.

#![allow(
    unsafe_code,
    reason = "Win32 is a C API; every unsafe block states its contract"
)]

use std::fs::File;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    CreateWellKnownSid, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
    GetKernelObjectSecurity, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner,
    GetTokenInformation, INHERIT_ONLY_ACE, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_MAX_SID_SIZE, TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER, TokenOwner, TokenUser,
    WELL_KNOWN_SID_TYPE, WinBuiltinAdministratorsSid, WinLocalSystemSid,
};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_TYPE_DISK, GetFileInformationByHandle, GetFileType,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// `GENERIC_READ | READ_CONTROL`: the contents and the security descriptor.
const READ_WITH_SECURITY: u32 = 0x8000_0000 | 0x0002_0000;
/// `SYNCHRONIZE` alone grants nothing about the contents.
const SYNCHRONIZE: u32 = 0x0010_0000;

/// Why a password file was not opened.
#[derive(Debug)]
pub enum PasswordFileRefusal {
    /// The file could not be opened.
    Open(std::io::Error),
    /// A symbolic link, junction or other reparse point.
    ReparsePoint,
    /// A directory, device or pipe.
    NotRegular,
    /// Its security descriptor could not be read (Win32 error code).
    Security(u32),
    /// Another account owns it.
    WrongOwner,
    /// No DACL: everyone has full access.
    NoDacl,
    /// The DACL lets another account in.
    BroadAccess,
    /// The DACL holds an allow entry this check cannot judge.
    UnknownEntry,
}

impl std::fmt::Display for PasswordFileRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open(error) => write!(formatter, "cannot open password file: {error}"),
            Self::ReparsePoint => formatter.write_str(
                "password file must not be a symbolic link, junction or cloud placeholder",
            ),
            Self::NotRegular => formatter.write_str("password file must be a regular file"),
            Self::Security(code) => write!(
                formatter,
                "cannot read the password file's permissions (error {code})"
            ),
            Self::WrongOwner => {
                formatter.write_str("password file must be owned by the current user")
            }
            Self::NoDacl => formatter.write_str(
                "password file has no access list (everyone can read it); \
                 allow only yourself, SYSTEM and Administrators",
            ),
            Self::BroadAccess => formatter.write_str(
                "password file must not be readable by other accounts; \
                 allow only yourself, SYSTEM and Administrators",
            ),
            Self::UnknownEntry => formatter.write_str(
                "password file's access list has an entry Arcen cannot check; \
                 allow only yourself, SYSTEM and Administrators",
            ),
        }
    }
}

impl std::error::Error for PasswordFileRefusal {}

/// One access-list entry, as far as the judgement needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AceFacts {
    /// `ACCESS_ALLOWED_ACE_TYPE` (0) is the only allow kind this reads.
    pub ace_type: u8,
    pub inherit_only: bool,
    /// The entry's account is the current user, `SYSTEM` or
    /// `Administrators`.
    pub trusted: bool,
    pub mask: u32,
}

const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
/// Allow kinds that carry their SID elsewhere (compound, object,
/// callback, callback object).
const OTHER_ALLOW_TYPES: [u8; 4] = [4, 5, 9, 11];

/// Whether an access list with `entries` keeps the file to its owner,
/// `SYSTEM` and `Administrators`.
///
/// # Errors
///
/// [`PasswordFileRefusal::BroadAccess`] or
/// [`PasswordFileRefusal::UnknownEntry`].
pub fn judge_dacl(entries: &[AceFacts]) -> Result<(), PasswordFileRefusal> {
    for entry in entries.iter().filter(|entry| !entry.inherit_only) {
        if OTHER_ALLOW_TYPES.contains(&entry.ace_type) {
            return Err(PasswordFileRefusal::UnknownEntry);
        }
        if entry.ace_type == ACCESS_ALLOWED_ACE_TYPE
            && !entry.trusted
            && entry.mask & !SYNCHRONIZE != 0
        {
            return Err(PasswordFileRefusal::BroadAccess);
        }
    }
    Ok(())
}

/// Opens `path` for reading after the checks in the module documentation.
///
/// # Errors
///
/// Why the file was refused.
pub fn open_password_file(path: &Path) -> Result<File, PasswordFileRefusal> {
    let file = std::fs::OpenOptions::new()
        .access_mode(READ_WITH_SECURITY)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)
        .map_err(PasswordFileRefusal::Open)?;
    let handle = HANDLE(file.as_raw_handle());
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `handle` is the open file; `info` is a plain out structure.
    unsafe { GetFileInformationByHandle(handle, &raw mut info) }
        .map_err(|error| PasswordFileRefusal::Security(win32_code(&error)))?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(PasswordFileRefusal::ReparsePoint);
    }
    // SAFETY: as above.
    let disk = unsafe { GetFileType(handle) } == FILE_TYPE_DISK;
    if !disk || info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
        return Err(PasswordFileRefusal::NotRegular);
    }
    check_security(handle)?;
    Ok(file)
}

fn win32_code(error: &windows::core::Error) -> u32 {
    // An HRESULT from a Win32 error carries the code in its low word.
    #[allow(clippy::cast_sign_loss, reason = "the HRESULT's bit pattern")]
    let code = error.code().0 as u32;
    code & 0xFFFF
}

fn check_security(handle: HANDLE) -> Result<(), PasswordFileRefusal> {
    let requested = (OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION).0;
    let mut needed = 0_u32;
    // SAFETY: a size query; a null descriptor with length 0 is allowed.
    let _ = unsafe {
        GetKernelObjectSecurity(
            handle,
            requested,
            PSECURITY_DESCRIPTOR::default(),
            0,
            &raw mut needed,
        )
    };
    if needed == 0 {
        return Err(PasswordFileRefusal::Security(0));
    }
    // u64 words keep the descriptor suitably aligned.
    let mut buffer = vec![0_u64; (needed as usize).div_ceil(8)];
    let descriptor = PSECURITY_DESCRIPTOR(buffer.as_mut_ptr().cast());
    // SAFETY: `buffer` holds at least `needed` bytes.
    unsafe { GetKernelObjectSecurity(handle, requested, descriptor, needed, &raw mut needed) }
        .map_err(|error| PasswordFileRefusal::Security(win32_code(&error)))?;

    let (user, default_owner) = current_user_sids()?;
    let mut owner = PSID::default();
    let mut defaulted = windows::Win32::Foundation::BOOL::default();
    // SAFETY: `descriptor` is the self-relative descriptor just read; the
    // owner pointer points into `buffer`.
    unsafe { GetSecurityDescriptorOwner(descriptor, &raw mut owner, &raw mut defaulted) }
        .map_err(|error| PasswordFileRefusal::Security(win32_code(&error)))?;
    if owner.is_invalid()
        || !(sid_equals(owner, user.psid()) || sid_equals(owner, default_owner.psid()))
    {
        return Err(PasswordFileRefusal::WrongOwner);
    }

    let mut present = windows::Win32::Foundation::BOOL::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: as above, for the DACL.
    unsafe {
        GetSecurityDescriptorDacl(
            descriptor,
            &raw mut present,
            &raw mut dacl,
            &raw mut defaulted,
        )
    }
    .map_err(|error| PasswordFileRefusal::Security(win32_code(&error)))?;
    if !present.as_bool() || dacl.is_null() {
        return Err(PasswordFileRefusal::NoDacl);
    }
    let system = well_known_sid(WinLocalSystemSid)?;
    let administrators = well_known_sid(WinBuiltinAdministratorsSid)?;
    let entries = dacl_entries(dacl, |sid| {
        sid_equals(sid, user.psid())
            || sid_equals(sid, system.psid())
            || sid_equals(sid, administrators.psid())
    })?;
    judge_dacl(&entries)
}

fn dacl_entries(
    dacl: *mut ACL,
    trusted: impl Fn(PSID) -> bool,
) -> Result<Vec<AceFacts>, PasswordFileRefusal> {
    let mut size = ACL_SIZE_INFORMATION::default();
    // SAFETY: `dacl` points into the descriptor buffer; `size` is a plain
    // out structure of the stated length.
    unsafe {
        GetAclInformation(
            dacl,
            (&raw mut size).cast(),
            u32::try_from(std::mem::size_of::<ACL_SIZE_INFORMATION>()).unwrap_or(0),
            AclSizeInformation,
        )
    }
    .map_err(|error| PasswordFileRefusal::Security(win32_code(&error)))?;
    let mut entries = Vec::new();
    for index in 0..size.AceCount {
        let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `index` is below the ACL's entry count.
        unsafe { GetAce(dacl, index, &raw mut ace) }
            .map_err(|error| PasswordFileRefusal::Security(win32_code(&error)))?;
        // SAFETY: every entry starts with an ACE_HEADER.
        let header = unsafe { *ace.cast::<ACE_HEADER>() };
        let inherit_only = u32::from(header.AceFlags) & INHERIT_ONLY_ACE.0 != 0;
        let (is_trusted, mask) = if header.AceType == ACCESS_ALLOWED_ACE_TYPE {
            let allowed = ace.cast::<ACCESS_ALLOWED_ACE>();
            // SAFETY: an ACCESS_ALLOWED_ACE: the mask, then the SID that
            // starts at `SidStart` and lies within the entry.
            unsafe {
                (
                    trusted(PSID((&raw mut (*allowed).SidStart).cast())),
                    (*allowed).Mask,
                )
            }
        } else {
            (false, 0)
        };
        entries.push(AceFacts {
            ace_type: header.AceType,
            inherit_only,
            trusted: is_trusted,
            mask,
        });
    }
    Ok(entries)
}

fn sid_equals(left: PSID, right: PSID) -> bool {
    // SAFETY: both point at valid SIDs owned by the caller.
    unsafe { EqualSid(left, right) }.is_ok()
}

/// A SID in an owned, aligned buffer.
struct OwnedSid {
    buffer: Vec<u64>,
    offset: usize,
}

impl OwnedSid {
    fn psid(&self) -> PSID {
        PSID(
            self.buffer
                .as_ptr()
                .cast::<u8>()
                .wrapping_add(self.offset)
                .cast_mut()
                .cast(),
        )
    }
}

fn well_known_sid(kind: WELL_KNOWN_SID_TYPE) -> Result<OwnedSid, PasswordFileRefusal> {
    let mut buffer = vec![0_u64; (SECURITY_MAX_SID_SIZE as usize).div_ceil(8)];
    let mut length = SECURITY_MAX_SID_SIZE;
    // SAFETY: `buffer` holds SECURITY_MAX_SID_SIZE bytes.
    unsafe {
        CreateWellKnownSid(
            kind,
            PSID::default(),
            PSID(buffer.as_mut_ptr().cast()),
            &raw mut length,
        )
    }
    .map_err(|error| PasswordFileRefusal::Security(win32_code(&error)))?;
    Ok(OwnedSid { buffer, offset: 0 })
}

/// The process token's user and its default owner for new objects.
fn current_user_sids() -> Result<(OwnedSid, OwnedSid), PasswordFileRefusal> {
    let mut token = HANDLE::default();
    // SAFETY: the pseudo-handle of this process; `token` is closed below.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) }
        .map_err(|error| PasswordFileRefusal::Security(win32_code(&error)))?;
    let result = token_sid(token, TokenUser, |buffer| {
        // SAFETY: the buffer starts with a TOKEN_USER.
        unsafe { (*buffer.cast::<TOKEN_USER>()).User.Sid }
    })
    .and_then(|user| {
        token_sid(token, TokenOwner, |buffer| {
            // SAFETY: the buffer starts with a TOKEN_OWNER.
            unsafe { (*buffer.cast::<TOKEN_OWNER>()).Owner }
        })
        .map(|owner| (user, owner))
    });
    // SAFETY: `token` was opened above.
    let _ = unsafe { CloseHandle(token) };
    result
}

/// A SID-bearing token information class, kept in its own buffer (the SID
/// points into it).
fn token_sid(
    token: HANDLE,
    class: windows::Win32::Security::TOKEN_INFORMATION_CLASS,
    sid_of: impl Fn(*const u64) -> PSID,
) -> Result<OwnedSid, PasswordFileRefusal> {
    let mut needed = 0_u32;
    // SAFETY: a size query.
    let _ = unsafe { GetTokenInformation(token, class, None, 0, &raw mut needed) };
    if needed == 0 {
        return Err(PasswordFileRefusal::Security(0));
    }
    let mut buffer = vec![0_u64; (needed as usize).div_ceil(8)];
    // SAFETY: `buffer` holds `needed` bytes.
    unsafe {
        GetTokenInformation(
            token,
            class,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &raw mut needed,
        )
    }
    .map_err(|error| PasswordFileRefusal::Security(win32_code(&error)))?;
    let sid = sid_of(buffer.as_ptr());
    let offset = (sid.0 as usize).wrapping_sub(buffer.as_ptr() as usize);
    if offset >= buffer.len() * 8 {
        return Err(PasswordFileRefusal::Security(0));
    }
    Ok(OwnedSid { buffer, offset })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    const READ: u32 = 0x0012_0089;

    fn allow(trusted: bool, mask: u32) -> AceFacts {
        AceFacts {
            ace_type: ACCESS_ALLOWED_ACE_TYPE,
            inherit_only: false,
            trusted,
            mask,
        }
    }

    #[test]
    fn only_the_user_system_and_administrators_may_be_allowed_in() {
        assert!(judge_dacl(&[allow(true, 0x001F_01FF), allow(true, READ)]).is_ok());
        assert!(matches!(
            judge_dacl(&[allow(true, READ), allow(false, READ)]),
            Err(PasswordFileRefusal::BroadAccess)
        ));
    }

    #[test]
    fn deny_inherit_only_and_synchronize_entries_grant_nothing() {
        let deny = AceFacts {
            ace_type: 1,
            ..allow(false, READ)
        };
        let inherit_only = AceFacts {
            inherit_only: true,
            ..allow(false, READ)
        };
        assert!(judge_dacl(&[deny, inherit_only, allow(false, SYNCHRONIZE)]).is_ok());
    }

    #[test]
    fn an_allow_entry_this_check_cannot_read_is_refused() {
        let object = AceFacts {
            ace_type: 5,
            ..allow(true, READ)
        };
        assert!(matches!(
            judge_dacl(&[object]),
            Err(PasswordFileRefusal::UnknownEntry)
        ));
    }

    struct Fixture(std::path::PathBuf);

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn fixture(name: &str) -> Fixture {
        fixture_in(
            &crate::platform::folders::native_log_dir().expect("local app data"),
            name,
        )
    }

    fn fixture_in(parent: &Path, name: &str) -> Fixture {
        let directory = parent.join("password-file-tests");
        std::fs::create_dir_all(&directory).expect("fixture directory");
        let path = directory.join(format!("{name}-{}", std::process::id()));
        std::fs::write(&path, b"dummy-password\n").expect("fixture");
        Fixture(path)
    }

    #[test]
    fn a_file_in_the_users_own_profile_opens() {
        // %LOCALAPPDATA% inherits SYSTEM, Administrators and the user only.
        let file = fixture("private");
        let opened = open_password_file(&file.0);
        assert!(opened.is_ok(), "{opened:?}");
    }

    #[test]
    fn a_file_every_user_can_read_is_refused() {
        // %ProgramData% hands its files to BUILTIN\Users for reading.
        let file = fixture_in(
            &std::path::PathBuf::from(std::env::var_os("ProgramData").expect("ProgramData"))
                .join("Arcen-test"),
            "shared",
        );
        let opened = open_password_file(&file.0);
        assert!(
            matches!(opened, Err(PasswordFileRefusal::BroadAccess)),
            "{opened:?}"
        );
    }

    #[test]
    fn a_directory_is_not_a_password_file() {
        let file = fixture("directory-parent");
        let directory = file.0.parent().expect("parent").to_path_buf();
        assert!(open_password_file(&directory).is_err());
    }
}
