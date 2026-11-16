// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Paths and strings both component binaries use (`arcen-usb-prepare`
//! includes this file): the protected component directory, wide strings.

use std::path::{Path, PathBuf};

use std::ffi::c_void;
use windows::Win32::Foundation::{HLOCAL, LocalFree};

use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows::Win32::Security::{
    ACL, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION, GetAce,
    GetAclInformation, GetSecurityDescriptorDacl, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
};
use windows::Win32::Storage::FileSystem::CreateDirectoryW;
use windows::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;
use windows::core::{PCWSTR, PWSTR};

/// A NUL-terminated UTF-16 copy of `text`.
pub(crate) fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The text before the first NUL of a UTF-16 buffer.
pub(crate) fn from_wide(buffer: &[u16]) -> String {
    let end = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

/// `C:\Windows`.
pub(crate) fn system_root() -> Option<PathBuf> {
    let mut buffer = [0_u16; 260];
    // SAFETY: writes at most 260 units into `buffer`.
    let length = unsafe { GetSystemWindowsDirectoryW(Some(&mut buffer)) } as usize;
    (length > 0 && length < buffer.len()).then(|| PathBuf::from(from_wide(&buffer)))
}

/// `%ProgramData%\Arcen\USB`: the component's state, writable only by
/// SYSTEM and administrators, readable by users (the Deck reads which
/// tablets are prepared).
fn component_root() -> PathBuf {
    std::env::var_os("ProgramData")
        .map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from)
        .join("Arcen")
        .join("USB")
}

/// Owners the component trusts for its directories and files: SYSTEM,
/// Administrators, `TrustedInstaller`.
const TRUSTED_OWNERS: [&str; 3] = [
    "S-1-5-18",
    "S-1-5-32-544",
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464",
];

/// Access bits that let a holder change an object or what it contains.
const WRITE_LIKE: u32 = 0x0002 // FILE_WRITE_DATA / FILE_ADD_FILE
    | 0x0004 // FILE_APPEND_DATA / FILE_ADD_SUBDIRECTORY
    | 0x0010 // FILE_WRITE_EA
    | 0x0100 // FILE_WRITE_ATTRIBUTES (enough to set a reparse point)
    | 0x0040 // FILE_DELETE_CHILD
    | 0x0001_0000 // DELETE
    | 0x0004_0000 // WRITE_DAC
    | 0x0008_0000 // WRITE_OWNER
    | 0x0200_0000 // MAXIMUM_ALLOWED
    | 0x1000_0000 // GENERIC_ALL
    | 0x4000_0000; // GENERIC_WRITE
const INHERIT_ONLY_ACE: u8 = 0x08;

fn sid_string(sid: PSID) -> Option<String> {
    let mut text = PWSTR::null();
    // SAFETY: converts a valid SID; the string is LocalAlloc'd and freed.
    unsafe { ConvertSidToStringSidW(sid, &raw mut text) }.ok()?;
    // SAFETY: a NUL-terminated string from the call above.
    let value = unsafe { text.to_string() }.ok();
    // SAFETY: frees the string allocated above.
    unsafe { LocalFree(HLOCAL(text.0.cast())) };
    value
}

/// Why an object's security is not trusted: its owner is not
/// SYSTEM/Administrators/`TrustedInstaller`, its DACL is missing, or an ACE
/// that applies to it lets anyone else write, delete or re-permission it.
fn untrusted_security(path: &Path) -> Option<String> {
    let name = wide(&path.to_string_lossy());
    let mut owner = PSID::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: reads owner and DACL into a LocalAlloc'd descriptor freed below.
    let code = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(name.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&raw mut owner),
            None,
            Some(&raw mut dacl),
            None,
            &raw mut descriptor,
        )
    };
    if code.is_err() {
        return Some(format!(
            "security of {} unreadable: {code:?}",
            path.display()
        ));
    }
    let verdict = (|| {
        let owner = sid_string(owner).unwrap_or_default();
        if !TRUSTED_OWNERS.contains(&owner.as_str()) {
            return Some(format!(
                "{} is owned by {owner}, not by SYSTEM or Administrators",
                path.display()
            ));
        }
        if dacl.is_null() {
            return Some(format!(
                "{} has no DACL (everyone has access)",
                path.display()
            ));
        }
        let mut info = ACL_SIZE_INFORMATION::default();
        // SAFETY: reads the ACE count of a valid DACL.
        if unsafe {
            GetAclInformation(
                dacl,
                (&raw mut info).cast(),
                u32::try_from(size_of::<ACL_SIZE_INFORMATION>()).unwrap_or(0),
                AclSizeInformation,
            )
        }
        .is_err()
        {
            return Some(format!("{} DACL unreadable", path.display()));
        }
        for index in 0..info.AceCount {
            let mut ace: *mut c_void = std::ptr::null_mut();
            // SAFETY: an index below the ACE count of a valid DACL.
            if unsafe { GetAce(dacl, index, &raw mut ace) }.is_err() || ace.is_null() {
                return Some(format!("{} ACE {index} unreadable", path.display()));
            }
            let bytes = ace.cast::<u8>();
            // SAFETY: every ACE starts with ACE_HEADER {type u8, flags u8,
            // size u16}; allowed ACEs continue with mask u32 and the SID.
            let (kind, flags) = unsafe { (*bytes, *bytes.add(1)) };
            match kind {
                // ACCESS_DENIED_ACE_TYPE only takes rights away.
                1 => {}
                _ if flags & INHERIT_ONLY_ACE != 0 => {}
                // ACCESS_ALLOWED_ACE_TYPE.
                0 => {
                    // SAFETY: as above; the mask is at offset 4, the SID at 8.
                    let mask = unsafe { bytes.add(4).cast::<u32>().read_unaligned() };
                    let sid = sid_string(PSID(unsafe { bytes.add(8) }.cast())).unwrap_or_default();
                    if mask & WRITE_LIKE != 0 && !TRUSTED_OWNERS.contains(&sid.as_str()) {
                        return Some(format!(
                            "{} lets {sid} change it (access mask {mask:#x})",
                            path.display()
                        ));
                    }
                }
                other => {
                    return Some(format!(
                        "{} carries an ACE of type {other} the component does not accept",
                        path.display()
                    ));
                }
            }
        }
        None
    })();
    // SAFETY: frees the descriptor allocated above.
    unsafe { LocalFree(HLOCAL(descriptor.0)) };
    verdict
}

fn is_reparse_point(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    std::fs::symlink_metadata(path)
        .is_ok_and(|meta| meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

/// Refuses a path the component must not trust: a reparse point (junction,
/// symbolic link, mount point), an object a standard user owns, or one whose
/// DACL lets anyone but SYSTEM, Administrators or `TrustedInstaller` change
/// it — so a user can neither have planted it nor alter it later.
pub(crate) fn require_trusted(path: &Path) -> Result<(), String> {
    if is_reparse_point(path) {
        return Err(format!("{} is a reparse point", path.display()));
    }
    untrusted_security(path).map_or(Ok(()), Err)
}

/// The component's security descriptor: owner Administrators; SYSTEM and
/// Administrators full; Users read and execute; inherited by children;
/// nothing inherited from above.
const COMPONENT_SDDL: &str = "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)";

fn with_component_descriptor<T>(
    act: impl FnOnce(PSECURITY_DESCRIPTOR) -> Result<T, String>,
) -> Result<T, String> {
    let sddl = wide(COMPONENT_SDDL);
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: parses a constant SDDL string into a LocalAlloc'd descriptor.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &raw mut descriptor,
            None,
        )
    }
    .map_err(|error| format!("component security descriptor: {error}"))?;
    let outcome = act(descriptor);
    // SAFETY: frees the descriptor allocated above.
    unsafe { LocalFree(HLOCAL(descriptor.0)) };
    outcome
}

/// Creates a directory with the component's owner and protected DACL in the
/// same call, so it is never observable with anything else.
fn create_protected_dir(path: &Path) -> Result<(), String> {
    with_component_descriptor(|descriptor| {
        let attributes = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        let name = wide(&path.to_string_lossy());
        // SAFETY: creates one directory with a valid security descriptor.
        unsafe { CreateDirectoryW(PCWSTR(name.as_ptr()), Some(&raw const attributes)) }
            .map_err(|error| format!("create {}: {error}", path.display()))
    })
}

/// Re-applies the component's exact DACL to a directory already verified as
/// trusted (an upgrade may have changed the grants).
fn reprotect(path: &Path) -> Result<(), String> {
    with_component_descriptor(|descriptor| {
        let mut present = false.into();
        let mut defaulted = false.into();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        // SAFETY: reads the DACL pointer inside the descriptor.
        unsafe {
            GetSecurityDescriptorDacl(
                descriptor,
                &raw mut present,
                &raw mut dacl,
                &raw mut defaulted,
            )
        }
        .map_err(|error| format!("component DACL: {error}"))?;
        let name = wide(&path.to_string_lossy());
        // SAFETY: applies a protected DACL to our own, already trusted,
        // directory.
        let code = unsafe {
            SetNamedSecurityInfoW(
                PCWSTR(name.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                PSID::default(),
                PSID::default(),
                Some(dacl),
                None,
            )
        };
        if code.is_ok() {
            Ok(())
        } else {
            Err(format!("protect {}: {code:?}", path.display()))
        }
    })
}

/// Creates the component's directories atomically with their security, or
/// verifies the ones that exist. `%ProgramData%` lets any user create
/// folders, so an existing `Arcen`, `Arcen\USB` or `packages` that a user
/// owns or may change, or any reparse point on the way, is refused — never
/// adopted.
pub(crate) fn secure_component_root() -> Result<PathBuf, String> {
    let root = component_root();
    let arcen = root
        .parent()
        .ok_or("component root has no parent")?
        .to_path_buf();
    for dir in [arcen.clone(), root.clone(), root.join("packages")] {
        if dir.exists() || is_reparse_point(&dir) {
            require_trusted(&dir)?;
            if dir != arcen {
                reprotect(&dir)?;
            }
        } else {
            create_protected_dir(&dir)?;
        }
    }
    Ok(root)
}
