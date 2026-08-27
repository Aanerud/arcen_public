//! The single manual `ICredentialProviderCredential` tile.

use std::sync::{Arc, Mutex};

use windows::core::{Error, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    BOOL, E_INVALIDARG, E_NOTIMPL, E_POINTER, E_UNEXPECTED, NTSTATUS,
};
use windows::Win32::Graphics::Gdi::HBITMAP;
use windows::Win32::UI::Shell::{
    ICredentialProviderCredential, ICredentialProviderCredentialEvents,
    ICredentialProviderCredential_Impl, CPFIS_FOCUSED, CPFIS_NONE, CPFS_DISPLAY_IN_BOTH,
    CPFS_DISPLAY_IN_SELECTED_TILE, CPGSR_NO_CREDENTIAL_NOT_FINISHED,
    CPGSR_RETURN_CREDENTIAL_FINISHED, CPSI_ERROR, CPSI_NONE, CPSI_SUCCESS,
    CREDENTIAL_PROVIDER_CREDENTIAL_SERIALIZATION, CREDENTIAL_PROVIDER_FIELD_INTERACTIVE_STATE,
    CREDENTIAL_PROVIDER_FIELD_STATE, CREDENTIAL_PROVIDER_GET_SERIALIZATION_RESPONSE,
    CREDENTIAL_PROVIDER_STATUS_ICON,
};

use crate::fields::{
    CredentialFields, FieldError, FieldId, MAX_PASSWORD_UNITS, MAX_USERNAME_UNITS,
};
use crate::provider::ProviderState;
use crate::secret::SecretWide;
use crate::serialization::{logon_result_message, split_account_name};

pub(crate) type SharedFields = Arc<Mutex<CredentialFields>>;
pub(crate) type SharedProviderState = Arc<Mutex<ProviderState>>;

#[windows::core::implement(ICredentialProviderCredential)]
pub struct Credential {
    fields: SharedFields,
    state: SharedProviderState,
    events: Mutex<Option<ICredentialProviderCredentialEvents>>,
}

impl Credential {
    pub fn new(fields: SharedFields, state: SharedProviderState) -> Self {
        crate::com::dll_add_ref();
        Self {
            fields,
            state,
            events: Mutex::new(None),
        }
    }
}

impl Drop for Credential {
    fn drop(&mut self) {
        match self.fields.try_lock() {
            Ok(mut fields) => {
                fields.clear_secret();
                fields.clear_autologon();
            }
            Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                let mut fields = poisoned.into_inner();
                fields.clear_secret();
                fields.clear_autologon();
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                // Another live callback owns the fields and therefore also owns
                // a COM reference. The final SecretWide owner still scrubs on drop.
            }
        }
        if let Ok(events) = self.events.get_mut() {
            let previous = events.take();
            drop(previous);
        }
        crate::com::dll_release();
    }
}

fn unknown_field() -> Error {
    Error::from_hresult(E_INVALIDARG)
}

fn field_error(error: FieldError) -> Error {
    match error {
        FieldError::UnknownField | FieldError::NotWritable | FieldError::TooLong => unknown_field(),
    }
}

/// Read a COM input string without scanning beyond the provider's field cap.
///
/// # Safety
/// `value` must point to a readable NUL-terminated UTF-16 string for the
/// duration of this call, as required by `SetStringValue`.
unsafe fn read_wide_bounded(value: &PCWSTR, max_units: usize) -> windows::core::Result<Vec<u16>> {
    if value.0.is_null() {
        return Err(Error::from_hresult(E_POINTER));
    }
    let mut units = Vec::with_capacity(max_units.min(64));
    for index in 0..=max_units {
        // SAFETY: COM requires `psz` to reference a readable NUL-terminated
        // string. Reading one unit at a time stops at that terminator and never
        // examines more than the provider's explicit cap plus one unit.
        let unit = unsafe { value.0.add(index).read() };
        if unit == 0 {
            return Ok(units);
        }
        if index == max_units {
            return Err(Error::from_hresult(E_INVALIDARG));
        }
        units.push(unit);
    }
    Err(Error::from_hresult(E_INVALIDARG))
}

fn field_state(
    field: FieldId,
) -> (
    CREDENTIAL_PROVIDER_FIELD_STATE,
    CREDENTIAL_PROVIDER_FIELD_INTERACTIVE_STATE,
) {
    match field {
        FieldId::Label => (CPFS_DISPLAY_IN_BOTH, CPFIS_NONE),
        FieldId::Username => (CPFS_DISPLAY_IN_SELECTED_TILE, CPFIS_FOCUSED),
        FieldId::Password | FieldId::Submit | FieldId::Status => {
            (CPFS_DISPLAY_IN_SELECTED_TILE, CPFIS_NONE)
        }
    }
}

/// Initialize every `GetSerialization` output to a no-credential result.
///
/// # Safety
/// `response` and `serialization` must be writable when non-null, and every
/// non-null optional output must be writable for its corresponding type.
unsafe fn initialize_serialization_outputs(
    response: *mut CREDENTIAL_PROVIDER_GET_SERIALIZATION_RESPONSE,
    serialization: *mut CREDENTIAL_PROVIDER_CREDENTIAL_SERIALIZATION,
    status_text: *mut PWSTR,
    status_icon: *mut CREDENTIAL_PROVIDER_STATUS_ICON,
) -> windows::core::Result<()> {
    if response.is_null() || serialization.is_null() {
        return Err(Error::from_hresult(E_POINTER));
    }
    // SAFETY: required output pointers were checked non-null; optional output
    // pointers are written only when supplied by LogonUI.
    unsafe {
        *response = CPGSR_NO_CREDENTIAL_NOT_FINISHED;
        *serialization = CREDENTIAL_PROVIDER_CREDENTIAL_SERIALIZATION::default();
        if !status_text.is_null() {
            *status_text = PWSTR::null();
        }
        if !status_icon.is_null() {
            *status_icon = CPSI_NONE;
        }
    }
    Ok(())
}

/// Populate optional LogonUI status outputs with task-allocated text.
///
/// # Safety
/// Every non-null output pointer must be writable for its corresponding type.
/// On success, LogonUI assumes ownership of the returned `PWSTR`.
unsafe fn set_optional_status(
    status_text: *mut PWSTR,
    status_icon: *mut CREDENTIAL_PROVIDER_STATUS_ICON,
    message: &str,
    icon: CREDENTIAL_PROVIDER_STATUS_ICON,
) -> windows::core::Result<()> {
    let allocated = if status_text.is_null() {
        None
    } else {
        Some(crate::com::alloc_wide(message)?)
    };
    // SAFETY: optional output pointers are written only when non-null. Any
    // allocated string is transferred to LogonUI through `status_text`.
    unsafe {
        if let Some(text) = allocated {
            *status_text = text;
        }
        if !status_icon.is_null() {
            *status_icon = icon;
        }
    }
    Ok(())
}

impl ICredentialProviderCredential_Impl for Credential_Impl {
    fn Advise(
        &self,
        pcpce: Option<&ICredentialProviderCredentialEvents>,
    ) -> windows::core::Result<()> {
        crate::com::guard("ICredentialProviderCredential::Advise", || {
            let events = pcpce.ok_or_else(|| Error::from_hresult(E_INVALIDARG))?;
            let previous = self
                .events
                .lock()
                .map_err(|_| Error::from_hresult(E_UNEXPECTED))?
                .replace(events.clone());
            drop(previous);
            Ok(())
        })
    }

    fn UnAdvise(&self) -> windows::core::Result<()> {
        crate::com::guard("ICredentialProviderCredential::UnAdvise", || {
            // This manual-only stage starts no background worker. Release the
            // callback outside the mutex so a final Release cannot re-enter us
            // while the lock is held.
            let previous = self
                .events
                .lock()
                .map_err(|_| Error::from_hresult(E_UNEXPECTED))?
                .take();
            drop(previous);
            Ok(())
        })
    }

    fn SetSelected(&self) -> windows::core::Result<BOOL> {
        crate::com::guard("ICredentialProviderCredential::SetSelected", || {
            self.fields
                .lock()
                .map_err(|_| Error::from_hresult(E_UNEXPECTED))?
                .set_selected();
            Ok(BOOL(0))
        })
    }

    fn SetDeselected(&self) -> windows::core::Result<()> {
        crate::com::guard("ICredentialProviderCredential::SetDeselected", || {
            self.fields
                .lock()
                .map_err(|_| Error::from_hresult(E_UNEXPECTED))?
                .set_deselected();
            Ok(())
        })
    }

    fn GetFieldState(
        &self,
        dwfieldid: u32,
        pcpfs: *mut CREDENTIAL_PROVIDER_FIELD_STATE,
        pcpfis: *mut CREDENTIAL_PROVIDER_FIELD_INTERACTIVE_STATE,
    ) -> windows::core::Result<()> {
        crate::com::guard("ICredentialProviderCredential::GetFieldState", || {
            if pcpfs.is_null() || pcpfis.is_null() {
                return Err(Error::from_hresult(E_POINTER));
            }
            let field = FieldId::from_u32(dwfieldid).ok_or_else(unknown_field)?;
            let (state, interactive) = field_state(field);
            // SAFETY: both output pointers were checked non-null.
            unsafe {
                *pcpfs = state;
                *pcpfis = interactive;
            }
            Ok(())
        })
    }

    fn GetStringValue(&self, dwfieldid: u32) -> windows::core::Result<PWSTR> {
        crate::com::guard("ICredentialProviderCredential::GetStringValue", || {
            let field = FieldId::from_u32(dwfieldid).ok_or_else(unknown_field)?;
            let value = self
                .fields
                .lock()
                .map_err(|_| Error::from_hresult(E_UNEXPECTED))?
                .get_string(field)
                .map_err(field_error)?;
            crate::com::alloc_wide(&value)
        })
    }

    fn GetBitmapValue(&self, _dwfieldid: u32) -> windows::core::Result<HBITMAP> {
        crate::com::guard("ICredentialProviderCredential::GetBitmapValue", || {
            Err(Error::from_hresult(E_NOTIMPL))
        })
    }

    fn GetCheckboxValue(
        &self,
        _dwfieldid: u32,
        pbchecked: *mut BOOL,
        ppszlabel: *mut PWSTR,
    ) -> windows::core::Result<()> {
        crate::com::guard("ICredentialProviderCredential::GetCheckboxValue", || {
            // SAFETY: COM guarantees each supplied output pointer is writable;
            // null optional outputs are deliberately ignored.
            unsafe {
                if !pbchecked.is_null() {
                    *pbchecked = BOOL(0);
                }
                if !ppszlabel.is_null() {
                    *ppszlabel = PWSTR::null();
                }
            }
            Err(Error::from_hresult(E_NOTIMPL))
        })
    }

    fn GetSubmitButtonValue(&self, dwfieldid: u32) -> windows::core::Result<u32> {
        crate::com::guard(
            "ICredentialProviderCredential::GetSubmitButtonValue",
            || {
                if FieldId::from_u32(dwfieldid) != Some(FieldId::Submit) {
                    return Err(unknown_field());
                }
                Ok(FieldId::Password.as_u32())
            },
        )
    }

    fn GetComboBoxValueCount(
        &self,
        _dwfieldid: u32,
        pcitems: *mut u32,
        pdwselecteditem: *mut u32,
    ) -> windows::core::Result<()> {
        crate::com::guard(
            "ICredentialProviderCredential::GetComboBoxValueCount",
            || {
                // SAFETY: COM guarantees each supplied output pointer is
                // writable; null optional outputs are deliberately ignored.
                unsafe {
                    if !pcitems.is_null() {
                        *pcitems = 0;
                    }
                    if !pdwselecteditem.is_null() {
                        *pdwselecteditem = 0;
                    }
                }
                Err(Error::from_hresult(E_NOTIMPL))
            },
        )
    }

    fn GetComboBoxValueAt(&self, _dwfieldid: u32, _dwitem: u32) -> windows::core::Result<PWSTR> {
        crate::com::guard("ICredentialProviderCredential::GetComboBoxValueAt", || {
            Err(Error::from_hresult(E_NOTIMPL))
        })
    }

    fn SetStringValue(&self, dwfieldid: u32, psz: &PCWSTR) -> windows::core::Result<()> {
        crate::com::guard("ICredentialProviderCredential::SetStringValue", || {
            let field = FieldId::from_u32(dwfieldid).ok_or_else(unknown_field)?;
            match field {
                FieldId::Username => {
                    // SAFETY: SetStringValue's COM contract supplies a readable,
                    // NUL-terminated input string for the duration of this call.
                    let units = unsafe { read_wide_bounded(psz, MAX_USERNAME_UNITS) }?;
                    let value = String::from_utf16(&units)
                        .map_err(|_| Error::from_hresult(E_INVALIDARG))?;
                    self.fields
                        .lock()
                        .map_err(|_| Error::from_hresult(E_UNEXPECTED))?
                        .set_string(field, &value)
                        .map_err(field_error)
                }
                FieldId::Password => {
                    // SAFETY: SetStringValue's COM contract supplies a readable,
                    // NUL-terminated input string for the duration of this call.
                    let units = unsafe { read_wide_bounded(psz, MAX_PASSWORD_UNITS) }?;
                    self.fields
                        .lock()
                        .map_err(|_| Error::from_hresult(E_UNEXPECTED))?
                        .set_password(SecretWide::from_utf16_units(units))
                        .map_err(field_error)
                }
                FieldId::Label | FieldId::Submit | FieldId::Status => {
                    Err(Error::from_hresult(E_INVALIDARG))
                }
            }
        })
    }

    fn SetCheckboxValue(&self, _dwfieldid: u32, _bchecked: BOOL) -> windows::core::Result<()> {
        crate::com::guard("ICredentialProviderCredential::SetCheckboxValue", || {
            Err(Error::from_hresult(E_NOTIMPL))
        })
    }

    fn SetComboBoxSelectedValue(
        &self,
        _dwfieldid: u32,
        _dwselecteditem: u32,
    ) -> windows::core::Result<()> {
        crate::com::guard(
            "ICredentialProviderCredential::SetComboBoxSelectedValue",
            || Err(Error::from_hresult(E_NOTIMPL)),
        )
    }

    fn CommandLinkClicked(&self, _dwfieldid: u32) -> windows::core::Result<()> {
        crate::com::guard("ICredentialProviderCredential::CommandLinkClicked", || {
            Err(Error::from_hresult(E_NOTIMPL))
        })
    }

    fn GetSerialization(
        &self,
        pcpgsr: *mut CREDENTIAL_PROVIDER_GET_SERIALIZATION_RESPONSE,
        pcpcs: *mut CREDENTIAL_PROVIDER_CREDENTIAL_SERIALIZATION,
        ppszoptionalstatustext: *mut PWSTR,
        pcpsioptionalstatusicon: *mut CREDENTIAL_PROVIDER_STATUS_ICON,
    ) -> windows::core::Result<()> {
        crate::com::guard("ICredentialProviderCredential::GetSerialization", || {
            // SAFETY: this helper validates required pointers and conditionally
            // initializes optional pointers before any operation can fail.
            unsafe {
                initialize_serialization_outputs(
                    pcpgsr,
                    pcpcs,
                    ppszoptionalstatustext,
                    pcpsioptionalstatusicon,
                )?;
            }

            let usage = self
                .state
                .lock()
                .map_err(|_| Error::from_hresult(E_UNEXPECTED))?
                .usage()
                .ok_or_else(|| Error::from_hresult(E_UNEXPECTED))?;
            let input = {
                let mut fields = self
                    .fields
                    .lock()
                    .map_err(|_| Error::from_hresult(E_UNEXPECTED))?;
                // A broker-pushed credential auto-submits exactly once: consume
                // it in preference to any manual entry. `take_autologon` clears
                // the one-shot so it can never be serialized twice.
                if let Some((username, password)) = fields.take_autologon(crate::pipe::now_ms()) {
                    Some((username, password))
                } else if !fields.can_submit() {
                    fields.clear_secret();
                    fields.set_status("Enter a user name and password.");
                    None
                } else {
                    Some((fields.username().to_owned(), fields.take_password()))
                }
            };

            let Some((username, password)) = input else {
                // SAFETY: optional outputs were initialized above.
                return unsafe {
                    set_optional_status(
                        ppszoptionalstatustext,
                        pcpsioptionalstatusicon,
                        "Enter a user name and password.",
                        CPSI_ERROR,
                    )
                };
            };

            let account = match split_account_name(&username) {
                Ok(account) => account,
                Err(_) => {
                    self.fields
                        .lock()
                        .map_err(|_| Error::from_hresult(E_UNEXPECTED))?
                        .set_status("Enter a valid Windows account name.");
                    // `password` is scrubbed when it leaves this scope.
                    // SAFETY: optional outputs were initialized above.
                    return unsafe {
                        set_optional_status(
                            ppszoptionalstatustext,
                            pcpsioptionalstatusicon,
                            "Enter a valid Windows account name.",
                            CPSI_ERROR,
                        )
                    };
                }
            };

            let packed = match crate::serialization::pack_negotiate(&account, &password, usage) {
                Ok(packed) => packed,
                Err(_) => {
                    crate::log::debug("credential packing failed");
                    self.fields
                        .lock()
                        .map_err(|_| Error::from_hresult(E_UNEXPECTED))?
                        .set_status("Windows could not prepare the credentials.");
                    // SAFETY: optional outputs were initialized above.
                    return unsafe {
                        set_optional_status(
                            ppszoptionalstatustext,
                            pcpsioptionalstatusicon,
                            "Windows could not prepare the credentials.",
                            CPSI_ERROR,
                        )
                    };
                }
            };

            self.fields
                .lock()
                .map_err(|_| Error::from_hresult(E_UNEXPECTED))?
                .set_status("Signing in...");
            let (auth_package, buffer, size) = packed.into_raw();
            // SAFETY: required outputs were checked above. Ownership of `buffer`
            // transfers to LogonUI with this serialization.
            unsafe {
                *pcpcs = CREDENTIAL_PROVIDER_CREDENTIAL_SERIALIZATION {
                    ulAuthenticationPackage: auth_package,
                    clsidCredentialProvider: crate::guid::CLSID_ARCEN,
                    cbSerialization: size,
                    rgbSerialization: buffer,
                };
                *pcpgsr = CPGSR_RETURN_CREDENTIAL_FINISHED;
            }
            crate::log::debug("GetSerialization: returning finished credential");
            Ok(())
        })
    }

    fn ReportResult(
        &self,
        ntsstatus: NTSTATUS,
        ntssubstatus: NTSTATUS,
        ppszoptionalstatustext: *mut PWSTR,
        pcpsioptionalstatusicon: *mut CREDENTIAL_PROVIDER_STATUS_ICON,
    ) -> windows::core::Result<()> {
        crate::com::guard("ICredentialProviderCredential::ReportResult", || {
            // SAFETY: ReportResult's COM contract makes every non-null optional
            // output pointer writable for the duration of this callback.
            unsafe {
                if !ppszoptionalstatustext.is_null() {
                    *ppszoptionalstatustext = PWSTR::null();
                }
                if !pcpsioptionalstatusicon.is_null() {
                    *pcpsioptionalstatusicon = CPSI_NONE;
                }
            }
            let status = ntsstatus.0 as u32;
            let substatus = ntssubstatus.0 as u32;
            crate::log::debug(&format!(
                "ReportResult: status=0x{status:08x} substatus=0x{substatus:08x}"
            ));
            let message = logon_result_message(status, substatus);
            {
                let mut fields = self
                    .fields
                    .lock()
                    .map_err(|_| Error::from_hresult(E_UNEXPECTED))?;
                fields.reset_after_result();
                fields.set_status(message);
            }
            let icon = if status == 0 {
                CPSI_SUCCESS
            } else {
                CPSI_ERROR
            };
            // SAFETY: optional outputs were initialized above.
            unsafe {
                set_optional_status(
                    ppszoptionalstatustext,
                    pcpsioptionalstatusicon,
                    message,
                    icon,
                )
            }
        })
    }
}
