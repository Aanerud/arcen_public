//! `IClassFactory` for the Arcen provider CLSID.
//!
//! LogonUI reaches us through `DllGetClassObject`, which hands back this factory;
//! `CreateInstance` then produces an [`crate::provider::Provider`] as an
//! `ICredentialProvider`. Aggregation is refused, as required for credential
//! providers.

use core::ffi::c_void;

use windows::core::{Error, IUnknown, Interface, GUID};
use windows::Win32::Foundation::{BOOL, CLASS_E_NOAGGREGATION, E_INVALIDARG, E_POINTER};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows::Win32::UI::Shell::ICredentialProvider;

use crate::provider::Provider;

#[windows::core::implement(IClassFactory)]
pub struct ClassFactory;

impl ClassFactory {
    pub fn new() -> Self {
        crate::com::dll_add_ref();
        Self
    }
}

impl Default for ClassFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ClassFactory {
    fn drop(&mut self) {
        crate::com::dll_release();
    }
}

impl IClassFactory_Impl for ClassFactory_Impl {
    fn CreateInstance(
        &self,
        punkouter: Option<&IUnknown>,
        riid: *const GUID,
        ppvobject: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        crate::com::guard("IClassFactory::CreateInstance", || {
            if ppvobject.is_null() {
                return Err(Error::from_hresult(E_POINTER));
            }
            // SAFETY: ppvobject is a non-null writable pointer slot (checked above).
            unsafe { *ppvobject = core::ptr::null_mut() };
            // Credential providers are never aggregated.
            if punkouter.is_some() {
                return Err(Error::from_hresult(CLASS_E_NOAGGREGATION));
            }
            if riid.is_null() {
                return Err(Error::from_hresult(E_INVALIDARG));
            }
            let provider: ICredentialProvider = Provider::new().into();
            // SAFETY: riid/ppvobject are valid; query performs the AddRef on success.
            unsafe { provider.query(riid, ppvobject).ok() }
        })
    }

    fn LockServer(&self, flock: BOOL) -> windows::core::Result<()> {
        crate::com::guard("IClassFactory::LockServer", || {
            crate::com::server_lock(flock.as_bool());
            Ok(())
        })
    }
}
