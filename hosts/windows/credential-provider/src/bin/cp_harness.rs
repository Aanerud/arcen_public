#[cfg(not(windows))]
fn main() {
    println!("arcen-cp-harness is available only on Windows");
}

#[cfg(windows)]
fn main() {
    if let Err(error) = windows_harness::run() {
        eprintln!("credential-provider harness failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
mod windows_harness {
    use core::ffi::c_void;
    use std::path::{Path, PathBuf};

    use windows::core::{Interface, GUID, HRESULT, PCSTR, PCWSTR};
    use windows::Win32::Foundation::{FreeLibrary, BOOL, HMODULE, NTSTATUS, S_OK};
    use windows::Win32::Security::Authentication::Identity::{
        KerbInteractiveLogon, KerbWorkstationUnlockLogon, KERB_INTERACTIVE_UNLOCK_LOGON,
        KERB_LOGON_SUBMIT_TYPE, LSA_UNICODE_STRING,
    };
    use windows::Win32::System::Com::{CoTaskMemFree, IClassFactory};
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows::Win32::UI::Shell::{
        ICredentialProvider, ICredentialProviderCredential, CPFIS_FOCUSED,
        CPFS_DISPLAY_IN_SELECTED_TILE, CPFT_EDIT_TEXT, CPFT_LARGE_TEXT, CPFT_PASSWORD_TEXT,
        CPFT_SMALL_TEXT, CPFT_SUBMIT_BUTTON, CPGSR_RETURN_CREDENTIAL_FINISHED, CPSI_NONE,
        CPUS_CHANGE_PASSWORD, CPUS_CREDUI, CPUS_INVALID, CPUS_LOGON, CPUS_PLAP,
        CPUS_UNLOCK_WORKSTATION, CREDENTIAL_PROVIDER_CREDENTIAL_SERIALIZATION,
        CREDENTIAL_PROVIDER_FIELD_TYPE, CREDENTIAL_PROVIDER_GET_SERIALIZATION_RESPONSE,
        CREDENTIAL_PROVIDER_NO_DEFAULT, CREDENTIAL_PROVIDER_STATUS_ICON,
    };

    use arcen_credential_provider::fields::{FieldId, FIELD_COUNT};
    use arcen_credential_provider::guid::CLSID_ARCEN;

    type DllGetClassObject =
        unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;
    type DllCanUnloadNow = unsafe extern "system" fn() -> HRESULT;

    struct Module(Option<HMODULE>);

    impl Module {
        fn load(path: &Path) -> Result<Self, String> {
            use std::os::windows::ffi::OsStrExt;

            let wide: Vec<u16> = path
                .as_os_str()
                .encode_wide()
                .chain(core::iter::once(0))
                .collect();
            // SAFETY: `wide` is a live NUL-terminated absolute path.
            let module = unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) }
                .map_err(|error| format!("LoadLibraryW({}): {error}", path.display()))?;
            Ok(Self(Some(module)))
        }

        fn handle(&self) -> HMODULE {
            self.0.expect("module is open")
        }

        fn close(mut self) -> Result<(), String> {
            let handle = self.0.take().expect("module is open");
            // SAFETY: this guard owns one LoadLibraryW reference.
            unsafe { FreeLibrary(handle) }.map_err(|error| format!("FreeLibrary: {error}"))
        }
    }

    impl Drop for Module {
        fn drop(&mut self) {
            if let Some(handle) = self.0.take() {
                // SAFETY: this guard owns one LoadLibraryW reference.
                let _ = unsafe { FreeLibrary(handle) };
            }
        }
    }

    fn explicit_dll_path() -> Result<PathBuf, String> {
        let mut args = std::env::args_os();
        let program = args.next().unwrap_or_else(|| "arcen-cp-harness".into());
        let Some(path) = args.next() else {
            return Err(format!(
                "usage: {} <absolute-path-to-arcen_credential_provider.dll>",
                Path::new(&program).display()
            ));
        };
        if args.next().is_some() {
            return Err("expected exactly one DLL path".to_string());
        }
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(
                "DLL path must be absolute; registry/search-path loading is forbidden".into(),
            );
        }
        std::fs::canonicalize(&path)
            .map_err(|error| format!("canonicalize {}: {error}", path.display()))
    }

    /// Resolve one export as an exact function-pointer type.
    ///
    /// # Safety
    /// `T` must have the exact ABI and signature of `name` in `module`.
    unsafe fn resolve<T: Copy>(module: HMODULE, name: &'static [u8]) -> Result<T, String> {
        if name.len() < 2 || name.last() != Some(&0) {
            return Err("export name must be non-empty and NUL-terminated".into());
        }
        // SAFETY: `name` is static and NUL-terminated; `module` is live.
        let procedure =
            unsafe { GetProcAddress(module, PCSTR(name.as_ptr())) }.ok_or_else(|| {
                format!(
                    "missing export {}",
                    String::from_utf8_lossy(&name[..name.len() - 1])
                )
            })?;
        if core::mem::size_of::<T>() != core::mem::size_of_val(&procedure) {
            return Err("unexpected function-pointer size".into());
        }
        // SAFETY: the caller chooses `T` to exactly match the named DLL export.
        Ok(unsafe { core::mem::transmute_copy(&procedure) })
    }

    fn class_factory(get_class: DllGetClassObject) -> Result<IClassFactory, String> {
        let mut raw = core::ptr::null_mut();
        // SAFETY: all pointers reference live GUIDs/output storage.
        let result = unsafe { get_class(&CLSID_ARCEN, &IClassFactory::IID, &mut raw) };
        result
            .ok()
            .map_err(|error| format!("DllGetClassObject: {error}"))?;
        if raw.is_null() {
            return Err("DllGetClassObject returned a null factory".into());
        }
        // SAFETY: the successful call returned one owned IClassFactory reference.
        Ok(unsafe { IClassFactory::from_raw(raw) })
    }

    fn new_provider(factory: &IClassFactory) -> Result<ICredentialProvider, String> {
        // SAFETY: aggregation is disabled and the requested interface is exact.
        unsafe {
            factory.CreateInstance::<_, ICredentialProvider>(None::<&windows::core::IUnknown>)
        }
        .map_err(|error| format!("IClassFactory::CreateInstance: {error}"))
    }

    fn descriptor(
        provider: &ICredentialProvider,
        index: u32,
    ) -> Result<(u32, CREDENTIAL_PROVIDER_FIELD_TYPE, String), String> {
        // SAFETY: provider is live; any successful allocation is owned and freed
        // by this function even when `index` is an intentional invalid probe.
        let raw = unsafe { provider.GetFieldDescriptorAt(index) }
            .map_err(|error| format!("GetFieldDescriptorAt({index}): {error}"))?;
        if raw.is_null() {
            return Err(format!("GetFieldDescriptorAt({index}) returned null"));
        }
        // SAFETY: provider returned a live descriptor allocated with CoTaskMem.
        let value = unsafe { *raw };
        let label_result = if value.pszLabel.0.is_null() {
            Err(format!("field {index} returned a null label"))
        } else {
            // SAFETY: provider contract returns a NUL-terminated label.
            unsafe { PCWSTR(value.pszLabel.0).to_string() }
                .map_err(|error| format!("field {index} label: {error}"))
        };
        // SAFETY: both allocations are owned by the harness after this call.
        unsafe {
            if !value.pszLabel.0.is_null() {
                CoTaskMemFree(Some(value.pszLabel.0.cast()));
            }
            CoTaskMemFree(Some(raw.cast()));
        }
        label_result.map(|label| (value.dwFieldID, value.cpft, label))
    }

    fn verify_descriptors(provider: &ICredentialProvider) -> Result<(), String> {
        // SAFETY: provider is live.
        let count = unsafe { provider.GetFieldDescriptorCount() }
            .map_err(|error| format!("GetFieldDescriptorCount: {error}"))?;
        if count != FIELD_COUNT {
            return Err(format!("field count {count}, expected {FIELD_COUNT}"));
        }
        let expected = [
            (FieldId::Label, CPFT_LARGE_TEXT, "Arcen"),
            (FieldId::Username, CPFT_EDIT_TEXT, "User name"),
            (FieldId::Password, CPFT_PASSWORD_TEXT, "Password"),
            (FieldId::Submit, CPFT_SUBMIT_BUTTON, "Sign in with Arcen"),
            (FieldId::Status, CPFT_SMALL_TEXT, "Status"),
        ];
        for (index, (field, kind, label)) in expected.into_iter().enumerate() {
            let actual = descriptor(provider, index as u32)?;
            if actual != (field.as_u32(), kind, label.to_string()) {
                return Err(format!(
                    "field {index} mismatch: id={}, type={:?}, label={:?}",
                    actual.0, actual.1, actual.2
                ));
            }
        }
        if descriptor(provider, FIELD_COUNT).is_ok() {
            return Err("out-of-range field descriptor unexpectedly succeeded".into());
        }
        Ok(())
    }

    fn verify_credential(
        provider: &ICredentialProvider,
    ) -> Result<ICredentialProviderCredential, String> {
        let mut count = 99;
        let mut default = 0;
        let mut autologon = BOOL(1);
        // SAFETY: outputs are live and provider has a supported scenario.
        unsafe { provider.GetCredentialCount(&mut count, &mut default, &mut autologon) }
            .map_err(|error| format!("GetCredentialCount: {error}"))?;
        if count != 1 || default != CREDENTIAL_PROVIDER_NO_DEFAULT || autologon.as_bool() {
            return Err(format!(
                "unexpected credential count tuple ({count}, {default}, {})",
                autologon.as_bool()
            ));
        }
        // SAFETY: index zero is the sole advertised credential.
        let credential = unsafe { provider.GetCredentialAt(0) }
            .map_err(|error| format!("GetCredentialAt(0): {error}"))?;
        // SAFETY: this deliberately probes one out-of-range index.
        if unsafe { provider.GetCredentialAt(1) }.is_ok() {
            return Err("GetCredentialAt(1) unexpectedly succeeded".into());
        }
        // SAFETY: credential is live and output pointers are valid.
        let selected =
            unsafe { credential.SetSelected() }.map_err(|error| format!("SetSelected: {error}"))?;
        if selected.as_bool() {
            return Err("manual tile requested autologon".into());
        }
        let mut field_state = Default::default();
        let mut interactive_state = Default::default();
        // SAFETY: username is a valid field and output pointers are live.
        unsafe {
            credential.GetFieldState(
                FieldId::Username.as_u32(),
                &mut field_state,
                &mut interactive_state,
            )
        }
        .map_err(|error| format!("GetFieldState(username): {error}"))?;
        if field_state != CPFS_DISPLAY_IN_SELECTED_TILE || interactive_state != CPFIS_FOCUSED {
            return Err("username field state/focus mismatch".into());
        }
        // SAFETY: submit is a valid field on the live credential.
        let adjacent = unsafe { credential.GetSubmitButtonValue(FieldId::Submit.as_u32()) }
            .map_err(|error| format!("GetSubmitButtonValue: {error}"))?;
        if adjacent != FieldId::Password.as_u32() {
            return Err("submit button is not adjacent to the password field".into());
        }
        for (field, expected) in [
            (FieldId::Label, "Arcen"),
            (FieldId::Password, ""),
            (FieldId::Status, ""),
        ] {
            // SAFETY: each requested field is a string field on the live credential.
            let value = unsafe { credential.GetStringValue(field.as_u32()) }
                .map_err(|error| format!("GetStringValue({field:?}): {error}"))?;
            if value.0.is_null() {
                return Err(format!("GetStringValue({field:?}) returned null"));
            }
            // SAFETY: the provider returned a NUL-terminated CoTaskMem string.
            let actual = unsafe { PCWSTR(value.0).to_string() }
                .map_err(|error| format!("GetStringValue({field:?}) text: {error}"));
            // SAFETY: the harness owns the returned CoTaskMem allocation.
            unsafe { CoTaskMemFree(Some(value.0.cast())) };
            if actual? != expected {
                return Err(format!("GetStringValue({field:?}) mismatch"));
            }
        }
        // SAFETY: deselection is valid for the selected credential.
        unsafe { credential.SetDeselected() }.map_err(|error| format!("SetDeselected: {error}"))?;
        Ok(credential)
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(core::iter::once(0)).collect()
    }

    fn packed_string(
        bytes: &[u8],
        value: LSA_UNICODE_STRING,
        label: &str,
    ) -> Result<Vec<u16>, String> {
        let offset = value.Buffer.0 as usize;
        let length = value.Length as usize;
        if length != value.MaximumLength as usize || length & 1 != 0 {
            return Err(format!("{label} has an invalid packed length"));
        }
        let end = offset
            .checked_add(length)
            .ok_or_else(|| format!("{label} range overflow"))?;
        if offset < core::mem::size_of::<KERB_INTERACTIVE_UNLOCK_LOGON>() || end > bytes.len() {
            return Err(format!("{label} range is outside the serialization"));
        }
        let mut units = Vec::with_capacity(length / 2);
        for chunk in bytes[offset..end].chunks_exact(2) {
            units.push(u16::from_ne_bytes([chunk[0], chunk[1]]));
        }
        Ok(units)
    }

    fn verify_serialization(
        credential: &ICredentialProviderCredential,
        username: &str,
        expected_domain: &str,
        expected_user: &str,
        expected_message: KERB_LOGON_SUBMIT_TYPE,
    ) -> Result<(), String> {
        let username_w = wide(username);
        let password_w = wide("not-a-real-password");
        // SAFETY: both buffers are live, NUL-terminated strings for the calls.
        unsafe {
            credential.SetStringValue(FieldId::Username.as_u32(), PCWSTR(username_w.as_ptr()))
        }
        .map_err(|error| format!("SetStringValue(username): {error}"))?;
        // SAFETY: password buffer is live and NUL-terminated.
        unsafe {
            credential.SetStringValue(FieldId::Password.as_u32(), PCWSTR(password_w.as_ptr()))
        }
        .map_err(|error| format!("SetStringValue(password): {error}"))?;

        let mut response = CREDENTIAL_PROVIDER_GET_SERIALIZATION_RESPONSE::default();
        let mut serialization = CREDENTIAL_PROVIDER_CREDENTIAL_SERIALIZATION::default();
        let mut status_text = windows::core::PWSTR::null();
        let mut status_icon = CREDENTIAL_PROVIDER_STATUS_ICON::default();
        // SAFETY: every output slot is valid and writable for this live credential.
        unsafe {
            credential.GetSerialization(
                &mut response,
                &mut serialization,
                &mut status_text,
                &mut status_icon,
            )
        }
        .map_err(|error| format!("GetSerialization: {error}"))?;
        if !status_text.is_null() {
            // SAFETY: GetSerialization returned this optional CoTaskMem string.
            unsafe { CoTaskMemFree(Some(status_text.0.cast())) };
        }
        if response != CPGSR_RETURN_CREDENTIAL_FINISHED
            || serialization.clsidCredentialProvider != CLSID_ARCEN
            || serialization.rgbSerialization.is_null()
            || serialization.cbSerialization
                < core::mem::size_of::<KERB_INTERACTIVE_UNLOCK_LOGON>() as u32
        {
            if !serialization.rgbSerialization.is_null() {
                // SAFETY: ownership of the unsuccessful inspection remains here.
                unsafe { CoTaskMemFree(Some(serialization.rgbSerialization.cast())) };
            }
            return Err(format!(
                "GetSerialization tuple: response={:?} auth_package={} clsid={:?} buffer_null={} size={}",
                response,
                serialization.ulAuthenticationPackage,
                serialization.clsidCredentialProvider,
                serialization.rgbSerialization.is_null(),
                serialization.cbSerialization
            ));
        }

        // SAFETY: the provider returned a live allocation of cbSerialization bytes.
        let bytes = unsafe {
            core::slice::from_raw_parts(
                serialization.rgbSerialization,
                serialization.cbSerialization as usize,
            )
        };
        // SAFETY: the allocation is CoTaskMem-aligned and contains at least one header.
        let header = unsafe {
            serialization
                .rgbSerialization
                .cast::<KERB_INTERACTIVE_UNLOCK_LOGON>()
                .read()
        };
        let result = (|| {
            if header.Logon.MessageType != expected_message {
                return Err(format!(
                    "unexpected KERB message type {:?}",
                    header.Logon.MessageType
                ));
            }
            let domain = packed_string(bytes, header.Logon.LogonDomainName, "domain")?;
            let user = packed_string(bytes, header.Logon.UserName, "user")?;
            let password = packed_string(bytes, header.Logon.Password, "password")?;
            if String::from_utf16(&domain).map_err(|error| error.to_string())? != expected_domain {
                return Err("packed domain mismatch".to_string());
            }
            if String::from_utf16(&user).map_err(|error| error.to_string())? != expected_user {
                return Err("packed user mismatch".to_string());
            }
            if password.is_empty() {
                return Err("protected password is empty".to_string());
            }
            Ok(())
        })();

        // The harness owns the returned buffer; scrub the dummy secret before free.
        // SAFETY: the allocation remains live and exclusively owned here.
        unsafe {
            arcen_credential_provider::secret::scrub_bytes(core::slice::from_raw_parts_mut(
                serialization.rgbSerialization,
                serialization.cbSerialization as usize,
            ));
            CoTaskMemFree(Some(serialization.rgbSerialization.cast()));
        }
        // SAFETY: ReportResult outputs are optional and the success status is a
        // deliberate cleanup probe.
        unsafe {
            credential.ReportResult(
                NTSTATUS(0),
                NTSTATUS(0),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        }
        .map_err(|error| format!("ReportResult cleanup: {error}"))?;
        if status_icon != CPSI_NONE {
            return Err("GetSerialization unexpectedly returned a status icon".to_string());
        }
        result
    }

    fn verify_supported_scenario(
        factory: &IClassFactory,
        scenario: windows::Win32::UI::Shell::CREDENTIAL_PROVIDER_USAGE_SCENARIO,
    ) -> Result<(), String> {
        let provider = new_provider(factory)?;
        // SAFETY: scenario is CPUS_LOGON or CPUS_UNLOCK_WORKSTATION.
        unsafe { provider.SetUsageScenario(scenario, 0) }
            .map_err(|error| format!("SetUsageScenario({}): {error}", scenario.0))?;
        // SAFETY: a null serialization pointer is an intentional rejection probe;
        // the provider never dereferences remote serialization.
        if unsafe { provider.SetSerialization(core::ptr::null()) }.is_ok() {
            return Err("remote SetSerialization unexpectedly succeeded".into());
        }
        verify_descriptors(&provider)?;
        let credential = verify_credential(&provider)?;
        if scenario == CPUS_LOGON {
            verify_serialization(
                &credential,
                r"MACHINE\artist",
                "MACHINE",
                "artist",
                KerbInteractiveLogon,
            )?;
        } else {
            verify_serialization(
                &credential,
                r"CORP\artist",
                "CORP",
                "artist",
                KerbWorkstationUnlockLogon,
            )?;
        }
        drop(credential);
        drop(provider);
        Ok(())
    }

    pub fn run() -> Result<(), String> {
        let path = explicit_dll_path()?;
        let module = Module::load(&path)?;
        // SAFETY: signatures match the documented exports in this crate.
        let get_class: DllGetClassObject =
            unsafe { resolve(module.handle(), b"DllGetClassObject\0") }?;
        // SAFETY: signatures match the documented exports in this crate.
        let can_unload: DllCanUnloadNow =
            unsafe { resolve(module.handle(), b"DllCanUnloadNow\0") }?;

        {
            let factory = class_factory(get_class)?;
            verify_supported_scenario(&factory, CPUS_LOGON)?;
            verify_supported_scenario(&factory, CPUS_UNLOCK_WORKSTATION)?;

            for scenario in [CPUS_INVALID, CPUS_CHANGE_PASSWORD, CPUS_CREDUI, CPUS_PLAP] {
                let unsupported = new_provider(&factory)?;
                // SAFETY: each value is a defined but unsupported Win32 scenario.
                if unsafe { unsupported.SetUsageScenario(scenario, 0) }.is_ok() {
                    return Err(format!(
                        "unsupported usage scenario {} unexpectedly succeeded",
                        scenario.0
                    ));
                }
                let mut count = 99;
                let mut default = 0;
                let mut autologon = BOOL(1);
                // SAFETY: all output pointers are live.
                unsafe { unsupported.GetCredentialCount(&mut count, &mut default, &mut autologon) }
                    .map_err(|error| format!("GetCredentialCount after rejection: {error}"))?;
                if count != 0 || default != CREDENTIAL_PROVIDER_NO_DEFAULT || autologon.as_bool() {
                    return Err("rejected scenario remained enumerable".into());
                }
                // SAFETY: rejected scenarios must expose no credential.
                if unsafe { unsupported.GetCredentialAt(0) }.is_ok() {
                    return Err("rejected scenario returned a credential".into());
                }
            }

            // The live factory itself keeps the DLL non-unloadable.
            // SAFETY: resolved no-argument export is live while module is loaded.
            if unsafe { can_unload() } == S_OK {
                return Err("DllCanUnloadNow returned S_OK with a live factory".into());
            }
        }

        // SAFETY: every COM interface obtained from the DLL was dropped above.
        if unsafe { can_unload() } != S_OK {
            return Err("DllCanUnloadNow did not return S_OK after releasing objects".into());
        }
        module.close()?;
        println!(
            "credential-provider harness passed: {} (logon + unlock, no registration)",
            path.display()
        );
        Ok(())
    }
}
