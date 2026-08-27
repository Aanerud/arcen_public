//! Provider state and the Windows `ICredentialProvider` implementation.

/// Usage scenarios supported by the manual Arcen tile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderUsage {
    Logon,
    UnlockWorkstation,
}

/// Platform-independent provider state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProviderState {
    usage: Option<ProviderUsage>,
}

impl ProviderState {
    pub fn usage(&self) -> Option<ProviderUsage> {
        self.usage
    }

    pub fn set_usage(&mut self, usage: ProviderUsage) {
        self.usage = Some(usage);
    }

    pub fn clear(&mut self) {
        self.usage = None;
    }

    pub fn is_enumerable(&self) -> bool {
        self.usage.is_some()
    }
}

#[cfg(windows)]
mod windows_impl {
    use std::sync::{Arc, Mutex};

    use windows::core::{Error, GUID};
    use windows::Win32::Foundation::{BOOL, E_INVALIDARG, E_NOTIMPL, E_POINTER, E_UNEXPECTED};
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{
        ICredentialProvider, ICredentialProviderCredential, ICredentialProviderEvents,
        ICredentialProvider_Impl, CPFG_CREDENTIAL_PROVIDER_LABEL, CPFG_LOGON_PASSWORD,
        CPFG_LOGON_USERNAME, CPFG_STANDALONE_SUBMIT_BUTTON, CPFT_EDIT_TEXT, CPFT_LARGE_TEXT,
        CPFT_PASSWORD_TEXT, CPFT_SMALL_TEXT, CPFT_SUBMIT_BUTTON, CPUS_LOGON,
        CPUS_UNLOCK_WORKSTATION, CREDENTIAL_PROVIDER_CREDENTIAL_SERIALIZATION,
        CREDENTIAL_PROVIDER_FIELD_DESCRIPTOR, CREDENTIAL_PROVIDER_NO_DEFAULT,
        CREDENTIAL_PROVIDER_USAGE_SCENARIO,
    };

    use super::{ProviderState, ProviderUsage};
    use crate::credential::{Credential, SharedFields};
    use crate::fields::{field_specs, CredentialFields, FieldKind, FIELD_COUNT};

    #[windows::core::implement(ICredentialProvider)]
    pub struct Provider {
        state: Arc<Mutex<ProviderState>>,
        events: Mutex<Option<(ICredentialProviderEvents, usize)>>,
        fields: SharedFields,
        credential: ICredentialProviderCredential,
        /// Background worker that receives a broker-pushed credential over the
        /// SYSTEM-only pipe and arms a one-shot autologon. Started on Advise,
        /// stopped on UnAdvise/teardown.
        pipe: Mutex<crate::pipe::CredentialPipe>,
    }

    impl Provider {
        pub fn new() -> Self {
            let fields = SharedFields::new(Mutex::new(CredentialFields::new()));
            let state = Arc::new(Mutex::new(ProviderState::default()));
            let credential: ICredentialProviderCredential =
                Credential::new(fields.clone(), Arc::clone(&state)).into();
            crate::com::dll_add_ref();
            Self {
                state,
                events: Mutex::new(None),
                fields,
                credential,
                pipe: Mutex::new(crate::pipe::CredentialPipe::new()),
            }
        }

        fn is_enumerable(&self) -> windows::core::Result<bool> {
            self.state
                .lock()
                .map(|state| state.is_enumerable())
                .map_err(|_| Error::from_hresult(E_UNEXPECTED))
        }
    }

    impl Default for Provider {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Drop for Provider {
        fn drop(&mut self) {
            if let Ok(events) = self.events.get_mut() {
                let previous = events.take();
                drop(previous);
            }
            crate::com::dll_release();
        }
    }

    fn usage_from_win32(
        usage: CREDENTIAL_PROVIDER_USAGE_SCENARIO,
    ) -> windows::core::Result<ProviderUsage> {
        if usage == CPUS_LOGON {
            Ok(ProviderUsage::Logon)
        } else if usage == CPUS_UNLOCK_WORKSTATION {
            Ok(ProviderUsage::UnlockWorkstation)
        } else {
            Err(Error::from_hresult(E_NOTIMPL))
        }
    }

    fn field_type(kind: FieldKind) -> windows::Win32::UI::Shell::CREDENTIAL_PROVIDER_FIELD_TYPE {
        match kind {
            FieldKind::LargeText => CPFT_LARGE_TEXT,
            FieldKind::EditText => CPFT_EDIT_TEXT,
            FieldKind::PasswordText => CPFT_PASSWORD_TEXT,
            FieldKind::SubmitButton => CPFT_SUBMIT_BUTTON,
            FieldKind::SmallText => CPFT_SMALL_TEXT,
        }
    }

    fn field_guid(kind: FieldKind) -> GUID {
        match kind {
            FieldKind::LargeText => CPFG_CREDENTIAL_PROVIDER_LABEL,
            FieldKind::EditText => CPFG_LOGON_USERNAME,
            FieldKind::PasswordText => CPFG_LOGON_PASSWORD,
            FieldKind::SubmitButton => CPFG_STANDALONE_SUBMIT_BUTTON,
            FieldKind::SmallText => GUID::from_u128(0),
        }
    }

    impl ICredentialProvider_Impl for Provider_Impl {
        fn SetUsageScenario(
            &self,
            cpus: CREDENTIAL_PROVIDER_USAGE_SCENARIO,
            _dwflags: u32,
        ) -> windows::core::Result<()> {
            crate::com::guard("ICredentialProvider::SetUsageScenario", || {
                let usage = match usage_from_win32(cpus) {
                    Ok(usage) => usage,
                    Err(error) => {
                        self.state
                            .lock()
                            .map_err(|_| Error::from_hresult(E_UNEXPECTED))?
                            .clear();
                        self.fields
                            .lock()
                            .map_err(|_| Error::from_hresult(E_UNEXPECTED))?
                            .reset();
                        return Err(error);
                    }
                };
                {
                    let mut fields = self
                        .fields
                        .lock()
                        .map_err(|_| Error::from_hresult(E_UNEXPECTED))?;
                    fields.reset();
                }
                self.state
                    .lock()
                    .map_err(|_| Error::from_hresult(E_UNEXPECTED))?
                    .set_usage(usage);
                crate::log::debug(match usage {
                    ProviderUsage::Logon => "SetUsageScenario: logon",
                    ProviderUsage::UnlockWorkstation => "SetUsageScenario: unlock",
                });
                Ok(())
            })
        }

        fn SetSerialization(
            &self,
            _pcpcs: *const CREDENTIAL_PROVIDER_CREDENTIAL_SERIALIZATION,
        ) -> windows::core::Result<()> {
            crate::com::guard("ICredentialProvider::SetSerialization", || {
                // Remote/pre-serialized credentials are intentionally unsupported.
                Err(Error::from_hresult(E_NOTIMPL))
            })
        }

        fn Advise(
            &self,
            pcpe: Option<&ICredentialProviderEvents>,
            upadvisecontext: usize,
        ) -> windows::core::Result<()> {
            crate::com::guard("ICredentialProvider::Advise", || {
                let events = pcpe.ok_or_else(|| Error::from_hresult(E_INVALIDARG))?;
                let previous = self
                    .events
                    .lock()
                    .map_err(|_| Error::from_hresult(E_UNEXPECTED))?
                    .replace((events.clone(), upadvisecontext));
                drop(previous);
                // Start the broker pipe worker for this Advise lifecycle so a
                // remote first-login can hand us a credential to auto-submit.
                if let Some(usage) = self
                    .state
                    .lock()
                    .map_err(|_| Error::from_hresult(E_UNEXPECTED))?
                    .usage()
                {
                    self.pipe
                        .lock()
                        .map_err(|_| Error::from_hresult(E_UNEXPECTED))?
                        .start(self.fields.clone(), events, upadvisecontext, usage);
                }
                Ok(())
            })
        }

        fn UnAdvise(&self) -> windows::core::Result<()> {
            crate::com::guard("ICredentialProvider::UnAdvise", || {
                // Stop the pipe worker first (this joins it, so it can issue no
                // further CredentialsChanged) before dropping the callback.
                self.pipe
                    .lock()
                    .map_err(|_| Error::from_hresult(E_UNEXPECTED))?
                    .stop();
                let previous = self
                    .events
                    .lock()
                    .map_err(|_| Error::from_hresult(E_UNEXPECTED))?
                    .take();
                drop(previous);
                Ok(())
            })
        }

        fn GetFieldDescriptorCount(&self) -> windows::core::Result<u32> {
            crate::com::guard("ICredentialProvider::GetFieldDescriptorCount", || {
                Ok(FIELD_COUNT)
            })
        }

        fn GetFieldDescriptorAt(
            &self,
            dwindex: u32,
        ) -> windows::core::Result<*mut CREDENTIAL_PROVIDER_FIELD_DESCRIPTOR> {
            crate::com::guard("ICredentialProvider::GetFieldDescriptorAt", || {
                let spec = field_specs()
                    .get(dwindex as usize)
                    .copied()
                    .ok_or_else(|| Error::from_hresult(E_INVALIDARG))?;
                let label = crate::com::alloc_wide(spec.label)?;
                let descriptor = CREDENTIAL_PROVIDER_FIELD_DESCRIPTOR {
                    dwFieldID: spec.id.as_u32(),
                    cpft: field_type(spec.kind),
                    pszLabel: label,
                    guidFieldType: field_guid(spec.kind),
                };
                match crate::com::alloc_value(descriptor) {
                    Ok(value) => Ok(value),
                    Err(error) => {
                        // SAFETY: `label` is the live allocation returned above
                        // and ownership has not been transferred.
                        unsafe { CoTaskMemFree(Some(label.0.cast())) };
                        Err(error)
                    }
                }
            })
        }

        fn GetCredentialCount(
            &self,
            pdwcount: *mut u32,
            pdwdefault: *mut u32,
            pbautologonwithdefault: *mut BOOL,
        ) -> windows::core::Result<()> {
            crate::com::guard("ICredentialProvider::GetCredentialCount", || {
                if pdwcount.is_null() || pdwdefault.is_null() || pbautologonwithdefault.is_null() {
                    return Err(Error::from_hresult(E_POINTER));
                }
                let enumerable = self.is_enumerable()?;
                // A broker-pushed credential turns the manual tile into a
                // default that auto-submits exactly once; the report latches the
                // single autologon offer so a failed first-login cannot loop.
                let report = self
                    .fields
                    .lock()
                    .map_err(|_| Error::from_hresult(E_UNEXPECTED))?
                    .autologon_report(enumerable, crate::pipe::now_ms());
                crate::log::debug(&format!(
                    "GetCredentialCount: count={} default={} autologon={}",
                    report.count,
                    report
                        .default_index
                        .map_or_else(|| "none".to_string(), |index| index.to_string()),
                    report.autologon
                ));
                // SAFETY: all three output slots were checked non-null.
                unsafe {
                    *pdwcount = report.count;
                    *pdwdefault = report
                        .default_index
                        .unwrap_or(CREDENTIAL_PROVIDER_NO_DEFAULT);
                    *pbautologonwithdefault = BOOL::from(report.autologon);
                }
                Ok(())
            })
        }

        fn GetCredentialAt(
            &self,
            dwindex: u32,
        ) -> windows::core::Result<ICredentialProviderCredential> {
            crate::com::guard("ICredentialProvider::GetCredentialAt", || {
                if dwindex != 0 {
                    return Err(Error::from_hresult(E_INVALIDARG));
                }
                if !self.is_enumerable()? {
                    return Err(Error::from_hresult(E_UNEXPECTED));
                }
                Ok(self.credential.clone())
            })
        }
    }
}

#[cfg(windows)]
pub use windows_impl::Provider;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_is_not_enumerable_before_a_supported_scenario() {
        let mut state = ProviderState::default();
        assert_eq!(state.usage(), None);
        assert!(!state.is_enumerable());
        state.set_usage(ProviderUsage::Logon);
        assert!(state.is_enumerable());
        state.clear();
        assert!(!state.is_enumerable());
    }

    #[test]
    fn provider_tracks_logon_and_unlock_separately() {
        let mut state = ProviderState::default();
        state.set_usage(ProviderUsage::Logon);
        assert_eq!(state.usage(), Some(ProviderUsage::Logon));
        state.set_usage(ProviderUsage::UnlockWorkstation);
        assert_eq!(state.usage(), Some(ProviderUsage::UnlockWorkstation));
    }
}
