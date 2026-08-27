//! Turning typed credentials into a Winlogon serialization blob.
//!
//! The authoritative packing path (Windows only) is:
//!
//! 1. resolve the **Negotiate** authentication package number via
//!    `LsaConnectUntrusted` + `LsaLookupAuthenticationPackage`;
//! 2. protect the password with `CredProtectW`, then pack domain, user, and
//!    password into a `KERB_INTERACTIVE_UNLOCK_LOGON` buffer allocated with
//!    `CoTaskMemAlloc`;
//! 3. hand the buffer, its size, and the package number back to the credential
//!    object, scrubbing every transient copy of the secret on the way out.
//!
//! The account-name parsing and the NTSTATUS→message mapping are pure and live
//! here (with tests) so they are correct independent of the Win32 glue.

#[cfg(windows)]
use crate::secret::SecretWide;

// NTSTATUS values used by `logon_status_message`. windows-rs only exposes
// STATUS_SUCCESS in this feature set, so the rest are pinned by value.
const STATUS_SUCCESS: u32 = 0x0000_0000;
const STATUS_WRONG_PASSWORD: u32 = 0xC000_006A;
const STATUS_LOGON_FAILURE: u32 = 0xC000_006D;
const STATUS_ACCOUNT_RESTRICTION: u32 = 0xC000_006E;
const STATUS_PASSWORD_EXPIRED: u32 = 0xC000_0071;
const STATUS_ACCOUNT_DISABLED: u32 = 0xC000_0072;
const STATUS_PASSWORD_MUST_CHANGE: u32 = 0xC000_0224;
const STATUS_ACCOUNT_LOCKED_OUT: u32 = 0xC000_0234;

/// Which textual form the user typed their account in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountForm {
    /// `DOMAIN\user` down-level form.
    DownLevel,
    /// `user@domain` user-principal-name form.
    Upn,
    /// A bare name, treated as a local machine account.
    Bare,
}

/// A parsed account name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountName {
    /// User portion (down-level) or the whole principal (UPN / bare).
    pub user: String,
    /// Domain for the down-level form; `None` for UPN; `Some(".")` for a bare
    /// local account.
    pub domain: Option<String>,
    pub form: AccountForm,
}

/// Why an account name was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountError {
    /// The whole field was empty/whitespace.
    Empty,
    /// A separator was present but one side was empty (e.g. `\user` or `CORP\`).
    EmptySide,
    /// Both `\` and `@` separators were present — ambiguous.
    Ambiguous,
    /// A separator appeared more than once.
    RepeatedSeparator,
    /// The account contained an embedded NUL.
    InvalidCharacter,
}

/// Parse a typed account name into user/domain, matching the broker's own
/// `LogonUserW` splitting rules so the tile and the broker agree.
pub fn split_account_name(input: &str) -> Result<AccountName, AccountError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(AccountError::Empty);
    }
    if trimmed.contains('\0') {
        return Err(AccountError::InvalidCharacter);
    }
    let has_backslash = trimmed.contains('\\');
    let has_at = trimmed.contains('@');
    if has_backslash && has_at {
        return Err(AccountError::Ambiguous);
    }
    if has_backslash {
        if trimmed.matches('\\').count() != 1 {
            return Err(AccountError::RepeatedSeparator);
        }
        let (domain, user) = trimmed.split_once('\\').expect("contains backslash");
        let domain = domain.trim();
        let user = user.trim();
        if domain.is_empty() || user.is_empty() {
            return Err(AccountError::EmptySide);
        }
        return Ok(AccountName {
            user: user.to_string(),
            domain: Some(domain.to_string()),
            form: AccountForm::DownLevel,
        });
    }
    if has_at {
        if trimmed.matches('@').count() != 1 {
            return Err(AccountError::RepeatedSeparator);
        }
        let (user, domain) = trimmed.split_once('@').expect("contains at");
        let user = user.trim();
        let domain = domain.trim();
        if user.is_empty() || domain.is_empty() {
            return Err(AccountError::EmptySide);
        }
        return Ok(AccountName {
            user: format!("{user}@{domain}"),
            domain: None,
            form: AccountForm::Upn,
        });
    }
    Ok(AccountName {
        user: trimmed.to_string(),
        domain: Some(".".to_string()),
        form: AccountForm::Bare,
    })
}

impl AccountName {
    /// Domain and user fields for `KERB_INTERACTIVE_UNLOCK_LOGON`.
    pub fn logon_components(&self) -> (&str, &str) {
        match self.form {
            AccountForm::DownLevel | AccountForm::Bare => {
                (self.domain.as_deref().unwrap_or("."), &self.user)
            }
            AccountForm::Upn => ("", &self.user),
        }
    }
}

/// Map a Winlogon result NTSTATUS to a safe, user-facing message.
///
/// Wrong-password and unknown-user are intentionally collapsed into one message
/// so the tile never reveals whether an account exists.
pub fn logon_status_message(ntstatus: u32) -> &'static str {
    match ntstatus {
        STATUS_SUCCESS => "Signed in.",
        STATUS_WRONG_PASSWORD | STATUS_LOGON_FAILURE => "The user name or password is incorrect.",
        STATUS_ACCOUNT_LOCKED_OUT => "The account is locked.",
        STATUS_PASSWORD_EXPIRED | STATUS_PASSWORD_MUST_CHANGE => {
            "The password must be changed before signing in."
        }
        STATUS_ACCOUNT_DISABLED => "The account is disabled.",
        STATUS_ACCOUNT_RESTRICTION => "Account restrictions prevent sign-in.",
        _ => "Sign-in failed.",
    }
}

/// Map a primary/substatus pair, preferring a specific substatus when the
/// primary value is the generic logon failure.
pub fn logon_result_message(ntstatus: u32, substatus: u32) -> &'static str {
    if ntstatus == STATUS_LOGON_FAILURE && substatus != STATUS_SUCCESS {
        logon_status_message(substatus)
    } else {
        logon_status_message(ntstatus)
    }
}

/// Windows-only credential packing.
#[cfg(windows)]
pub use windows_impl::{pack_negotiate, PackedCredential};

#[cfg(windows)]
mod windows_impl {
    use super::{AccountName, SecretWide};
    use crate::provider::ProviderUsage;
    use windows::core::{Error, PSTR, PWSTR};
    use windows::Win32::Foundation::{
        BOOL, E_INVALIDARG, E_OUTOFMEMORY, HANDLE, SEC_E_INTERNAL_ERROR, STATUS_SUCCESS,
    };
    use windows::Win32::Security::Authentication::Identity::{
        KerbInteractiveLogon, KerbWorkstationUnlockLogon, LsaConnectUntrusted,
        LsaDeregisterLogonProcess, LsaLookupAuthenticationPackage, KERB_INTERACTIVE_UNLOCK_LOGON,
        LSA_STRING, LSA_UNICODE_STRING,
    };
    use windows::Win32::Security::Credentials::CredProtectW;
    use windows::Win32::System::Com::{CoTaskMemAlloc, CoTaskMemFree};

    const MAX_PACKED_CREDENTIAL_BYTES: u32 = 64 * 1024;
    const MAX_PROTECTED_PASSWORD_UNITS: u32 = MAX_PACKED_CREDENTIAL_BYTES / 2;

    /// A packed credential ready to be attached to a
    /// `CREDENTIAL_PROVIDER_CREDENTIAL_SERIALIZATION`.
    ///
    /// `buffer` is `CoTaskMemAlloc`'d. [`Self::into_raw`] transfers ownership to
    /// LogonUI; otherwise `Drop` scrubs and frees it.
    pub struct PackedCredential {
        auth_package: u32,
        buffer: *mut u8,
        size: u32,
    }

    impl PackedCredential {
        /// Transfer the packed buffer to LogonUI and disarm RAII cleanup.
        pub fn into_raw(mut self) -> (u32, *mut u8, u32) {
            let values = (self.auth_package, self.buffer, self.size);
            self.buffer = core::ptr::null_mut();
            self.size = 0;
            values
        }
    }

    impl Drop for PackedCredential {
        fn drop(&mut self) {
            if !self.buffer.is_null() {
                // SAFETY: while armed, this object exclusively owns a live
                // CoTaskMem allocation of `size` bytes.
                unsafe {
                    let slice = core::slice::from_raw_parts_mut(self.buffer, self.size as usize);
                    crate::secret::scrub_bytes(slice);
                    CoTaskMemFree(Some(self.buffer.cast()));
                }
                self.buffer = core::ptr::null_mut();
                self.size = 0;
            }
        }
    }

    struct LsaConnection(HANDLE);

    impl Drop for LsaConnection {
        fn drop(&mut self) {
            // SAFETY: this guard owns the handle returned by
            // LsaConnectUntrusted and deregisters it exactly once.
            unsafe {
                let _ = LsaDeregisterLogonProcess(self.0);
            }
        }
    }

    fn lookup_negotiate_package() -> windows::core::Result<u32> {
        // A NUL-terminated ANSI "Negotiate" backing the LSA_STRING.
        let mut name = *b"Negotiate\0";
        let lsa_name = LSA_STRING {
            Length: 9,
            MaximumLength: name.len() as u16,
            Buffer: PSTR(name.as_mut_ptr()),
        };
        let mut lsa = HANDLE::default();
        // SAFETY: out-param handle is valid; we deregister it below.
        let status = unsafe { LsaConnectUntrusted(&mut lsa) };
        if status != STATUS_SUCCESS {
            return Err(Error::from_hresult(SEC_E_INTERNAL_ERROR));
        }
        let lsa = LsaConnection(lsa);
        let mut package = 0u32;
        // SAFETY: lsa is a live untrusted connection; name buffer outlives the call.
        let status = unsafe { LsaLookupAuthenticationPackage(lsa.0, &lsa_name, &mut package) };
        if status != STATUS_SUCCESS {
            return Err(Error::from_hresult(SEC_E_INTERNAL_ERROR));
        }
        Ok(package)
    }

    /// Pack a password credential for Winlogon/Negotiate using the canonical
    /// `KERB_INTERACTIVE_UNLOCK_LOGON` layout.
    pub fn pack_negotiate(
        account: &AccountName,
        password: &SecretWide,
        usage: ProviderUsage,
    ) -> windows::core::Result<PackedCredential> {
        let auth_package = lookup_negotiate_package()?;
        let (domain, user) = account.logon_components();
        let domain_w: Vec<u16> = domain.encode_utf16().collect();
        let user_w: Vec<u16> = user.encode_utf16().collect();
        let protected_password = protect_password(password)?;
        pack_with_buffers(
            auth_package,
            usage,
            &domain_w,
            &user_w,
            protected_password.as_utf16(),
        )
    }

    fn protect_password(password: &SecretWide) -> windows::core::Result<SecretWide> {
        let mut input = password.to_nul_terminated();
        let mut required = 0u32;
        // SAFETY: input is a live NUL-terminated password buffer; this sizing
        // call intentionally supplies no output storage.
        let _ = unsafe { CredProtectW(BOOL(0), &input, PWSTR::null(), &mut required, None) };
        if required == 0 || required > MAX_PROTECTED_PASSWORD_UNITS {
            crate::secret::scrub_wide(&mut input);
            return Err(Error::from_hresult(E_INVALIDARG));
        }

        let mut output = vec![0u16; required as usize];
        let mut written = required;
        // SAFETY: output has the capacity reported by the sizing call and input
        // remains live for the duration of the call.
        let result = unsafe {
            CredProtectW(
                BOOL(0),
                &input,
                PWSTR(output.as_mut_ptr()),
                &mut written,
                None,
            )
        };
        crate::secret::scrub_wide(&mut input);
        if let Err(error) = result {
            crate::secret::scrub_wide(&mut output);
            return Err(error);
        }
        if written == 0 || written > output.len() as u32 {
            crate::secret::scrub_wide(&mut output);
            return Err(Error::from_hresult(E_INVALIDARG));
        }
        output.truncate(written as usize);
        if output.last() == Some(&0) {
            output.pop();
        }
        Ok(SecretWide::from_utf16_units(output))
    }

    fn pack_with_buffers(
        auth_package: u32,
        usage: ProviderUsage,
        domain_w: &[u16],
        user_w: &[u16],
        password_w: &[u16],
    ) -> windows::core::Result<PackedCredential> {
        let header_size = core::mem::size_of::<KERB_INTERACTIVE_UNLOCK_LOGON>();
        let domain_bytes = component_bytes(domain_w)?;
        let user_bytes = component_bytes(user_w)?;
        let password_bytes = component_bytes(password_w)?;
        let total = header_size
            .checked_add(domain_bytes as usize)
            .and_then(|size| size.checked_add(user_bytes as usize))
            .and_then(|size| size.checked_add(password_bytes as usize))
            .and_then(|size| u32::try_from(size).ok())
            .filter(|&size| size <= MAX_PACKED_CREDENTIAL_BYTES)
            .ok_or_else(|| Error::from_hresult(E_INVALIDARG))?;

        // SAFETY: CoTaskMemAlloc returns a suitably aligned block of `total` bytes.
        let buffer = unsafe { CoTaskMemAlloc(total as usize) } as *mut u8;
        if buffer.is_null() {
            return Err(Error::from_hresult(E_OUTOFMEMORY));
        }
        let domain_offset = header_size;
        let user_offset = domain_offset + domain_bytes as usize;
        let password_offset = user_offset + user_bytes as usize;

        // SAFETY: buffer is aligned and large enough for the header and all
        // checked component ranges. The strings are copied without terminators,
        // exactly as the LSA packed-buffer contract requires. Assigning fields
        // into a zeroed header keeps every ABI padding byte initialized.
        unsafe {
            core::ptr::write_bytes(buffer, 0, total as usize);
            let header = &mut *buffer.cast::<KERB_INTERACTIVE_UNLOCK_LOGON>();
            header.Logon.MessageType = match usage {
                ProviderUsage::Logon => KerbInteractiveLogon,
                ProviderUsage::UnlockWorkstation => KerbWorkstationUnlockLogon,
            };
            header.Logon.LogonDomainName = packed_unicode_string(domain_offset, domain_bytes);
            header.Logon.UserName = packed_unicode_string(user_offset, user_bytes);
            header.Logon.Password = packed_unicode_string(password_offset, password_bytes);
            header.LogonId = Default::default();
            copy_component(buffer, domain_offset, domain_w);
            copy_component(buffer, user_offset, user_w);
            copy_component(buffer, password_offset, password_w);
        }

        Ok(PackedCredential {
            auth_package,
            buffer,
            size: total,
        })
    }

    fn component_bytes(units: &[u16]) -> windows::core::Result<u16> {
        units
            .len()
            .checked_mul(core::mem::size_of::<u16>())
            .and_then(|bytes| u16::try_from(bytes).ok())
            .ok_or_else(|| Error::from_hresult(E_INVALIDARG))
    }

    fn packed_unicode_string(offset: usize, bytes: u16) -> LSA_UNICODE_STRING {
        LSA_UNICODE_STRING {
            Length: bytes,
            MaximumLength: bytes,
            Buffer: PWSTR(offset as *mut u16),
        }
    }

    /// # Safety
    /// `buffer` must own a live allocation large enough for `offset` plus every
    /// byte in `units`, and that range must not overlap `units`.
    unsafe fn copy_component(buffer: *mut u8, offset: usize, units: &[u16]) {
        if !units.is_empty() {
            // SAFETY: upheld by the caller's checked layout construction.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    units.as_ptr(),
                    buffer.add(offset).cast::<u16>(),
                    units.len(),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn down_level_splits_domain_and_user() {
        let parsed = split_account_name(r"CORP\alice").unwrap();
        assert_eq!(parsed.form, AccountForm::DownLevel);
        assert_eq!(parsed.user, "alice");
        assert_eq!(parsed.domain.as_deref(), Some("CORP"));
        assert_eq!(parsed.logon_components(), ("CORP", "alice"));
    }

    #[test]
    fn upn_is_kept_whole_with_no_domain() {
        let parsed = split_account_name("alice@corp.example").unwrap();
        assert_eq!(parsed.form, AccountForm::Upn);
        assert_eq!(parsed.user, "alice@corp.example");
        assert_eq!(parsed.domain, None);
        assert_eq!(parsed.logon_components(), ("", "alice@corp.example"));
    }

    #[test]
    fn bare_name_is_local() {
        let parsed = split_account_name("alice").unwrap();
        assert_eq!(parsed.form, AccountForm::Bare);
        assert_eq!(parsed.user, "alice");
        assert_eq!(parsed.domain.as_deref(), Some("."));
        assert_eq!(parsed.logon_components(), (".", "alice"));
    }

    #[test]
    fn whitespace_is_trimmed_and_empty_is_rejected() {
        assert_eq!(split_account_name("   "), Err(AccountError::Empty));
        assert_eq!(split_account_name(""), Err(AccountError::Empty));
        assert_eq!(split_account_name("  bob  ").unwrap().user, "bob");
    }

    #[test]
    fn malformed_separators_are_rejected() {
        assert_eq!(split_account_name(r"\user"), Err(AccountError::EmptySide));
        assert_eq!(split_account_name(r"CORP\"), Err(AccountError::EmptySide));
        assert_eq!(split_account_name("@dom"), Err(AccountError::EmptySide));
        assert_eq!(split_account_name("user@"), Err(AccountError::EmptySide));
        assert_eq!(
            split_account_name(r"CORP\bob@corp"),
            Err(AccountError::Ambiguous)
        );
        assert_eq!(
            split_account_name(r"CORP\bob\extra"),
            Err(AccountError::RepeatedSeparator)
        );
        assert_eq!(
            split_account_name("bob@example@extra"),
            Err(AccountError::RepeatedSeparator)
        );
        assert_eq!(
            split_account_name("bob\0example"),
            Err(AccountError::InvalidCharacter)
        );
        assert_eq!(
            split_account_name(r" CORP \ bob "),
            Ok(AccountName {
                user: "bob".to_string(),
                domain: Some("CORP".to_string()),
                form: AccountForm::DownLevel,
            })
        );
    }

    #[test]
    fn status_messages_are_safe_and_do_not_leak_account_existence() {
        assert_eq!(logon_status_message(0x0000_0000), "Signed in.");
        // Wrong password and unknown user collapse to the same message.
        assert_eq!(
            logon_status_message(0xC000_006A),
            logon_status_message(0xC000_006D)
        );
        assert_eq!(logon_status_message(0xC000_0234), "The account is locked.");
        assert_eq!(logon_status_message(0x1234_5678), "Sign-in failed.");
        assert_eq!(
            logon_result_message(0xC000_006D, 0xC000_0234),
            "The account is locked."
        );
        assert_eq!(
            logon_result_message(0xC000_006A, 0xC000_0234),
            "The user name or password is incorrect."
        );
    }
}
