//! Canonical identity and registry facts for the Arcen Credential Provider.
//!
//! These constants are the single source of truth shared by the COM server, the
//! unit tests, and the PowerShell install/uninstall scripts. Keeping the CLSID
//! and every registry path/value here (and asserting their exact shape in tests)
//! means the DLL and the scripts can never silently disagree about where the
//! provider registers.
//!
//! Registration layout (64-bit hive, never WOW6432Node):
//!
//! ```text
//! HKLM\SOFTWARE\Classes\CLSID\{CLSID}\InprocServer32
//!     (Default)      = <full path to arcen_credential_provider.dll>
//!     ThreadingModel = Apartment
//! HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Authentication\Credential Providers\{CLSID}
//!     (Default)      = Arcen Credential Provider
//! ```
//!
//! The standard Windows password provider is left completely untouched, and no
//! `ICredentialProviderFilter` is registered, so this tile is additive only.

/// Stable class id for the provider, in canonical braced upper-case form.
///
/// Pinned in source so LogonUI, the registry, the reserved notification schema,
/// and the install scripts all reference the exact same GUID. Never regenerate
/// this for an installed base without an explicit migration.
pub const CLSID_STRING: &str = "{2FBE34F2-9E7A-42FA-BFBF-44897694BE60}";

/// Friendly name written under the Credential Providers key and shown in tooling.
pub const PROVIDER_FRIENDLY_NAME: &str = "Arcen Credential Provider";

/// File name of the built in-proc COM server DLL.
pub const DLL_FILE_NAME: &str = "arcen_credential_provider.dll";

/// COM apartment model for an in-proc credential provider. Winlogon hosts
/// providers in an STA; this must be exactly `Apartment`.
pub const THREADING_MODEL: &str = "Apartment";

/// `HKLM` sub-path of the CLSID registration key (no hive prefix).
pub const CLSID_SUBKEY: &str = r"SOFTWARE\Classes\CLSID\{2FBE34F2-9E7A-42FA-BFBF-44897694BE60}";

/// `HKLM` sub-path of the `InprocServer32` key that names the DLL.
pub const INPROC_SUBKEY: &str =
    r"SOFTWARE\Classes\CLSID\{2FBE34F2-9E7A-42FA-BFBF-44897694BE60}\InprocServer32";

/// `HKLM` sub-path of the Credential Providers registration key.
pub const CREDENTIAL_PROVIDERS_SUBKEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Authentication\Credential Providers\{2FBE34F2-9E7A-42FA-BFBF-44897694BE60}";

/// Render a Windows `.reg` file that registers the provider for a DLL installed
/// at `dll_path`. Backslashes in the path are escaped per `.reg` syntax.
///
/// This is a test/inspection template, not the production install path.
/// `install.ps1` enforces architecture, signer, and rollback gates. The template
/// intentionally contains only the two additive key trees.
pub fn render_reg_file(dll_path: &str) -> String {
    let escaped = dll_path.replace('\\', r"\\");
    format!(
        "Windows Registry Editor Version 5.00\r\n\r\n\
         [HKEY_LOCAL_MACHINE\\{clsid}]\r\n\
         @=\"{name}\"\r\n\r\n\
         [HKEY_LOCAL_MACHINE\\{inproc}]\r\n\
         @=\"{dll}\"\r\n\
         \"ThreadingModel\"=\"{threading}\"\r\n\r\n\
         [HKEY_LOCAL_MACHINE\\{providers}]\r\n\
         @=\"{name}\"\r\n",
        clsid = CLSID_SUBKEY,
        inproc = INPROC_SUBKEY,
        providers = CREDENTIAL_PROVIDERS_SUBKEY,
        name = PROVIDER_FRIENDLY_NAME,
        dll = escaped,
        threading = THREADING_MODEL,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clsid_is_canonical_and_matches_subkeys() {
        assert!(arcen_cp_ipc::is_canonical_clsid(CLSID_STRING));
        // Every registry sub-path must embed the exact same braced CLSID.
        assert!(CLSID_SUBKEY.ends_with(CLSID_STRING));
        assert!(INPROC_SUBKEY.contains(CLSID_STRING));
        assert!(CREDENTIAL_PROVIDERS_SUBKEY.ends_with(CLSID_STRING));
    }

    #[test]
    fn inproc_key_is_under_the_clsid_key() {
        assert_eq!(INPROC_SUBKEY, format!(r"{CLSID_SUBKEY}\InprocServer32"));
    }

    #[test]
    fn credential_providers_key_is_the_authentication_path() {
        assert!(CREDENTIAL_PROVIDERS_SUBKEY.starts_with(
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\Authentication\Credential Providers\"
        ));
    }

    #[test]
    fn reg_file_registers_exactly_the_two_additive_keys() {
        let reg = render_reg_file(r"C:\Program Files\Arcen\arcen_credential_provider.dll");
        assert!(reg.starts_with("Windows Registry Editor Version 5.00"));
        assert!(reg.contains("\"ThreadingModel\"=\"Apartment\""));
        // Backslashes in the DLL path are escaped for .reg syntax.
        assert!(reg.contains(r"C:\\Program Files\\Arcen\\arcen_credential_provider.dll"));
        // The friendly name is written under both the CLSID and providers keys.
        assert_eq!(reg.matches(PROVIDER_FRIENDLY_NAME).count(), 2);
        // No credential-provider *filter* key is ever emitted.
        assert!(!reg.contains("Credential Provider Filters"));
    }

    #[test]
    fn powershell_scripts_pin_safe_registration_and_install_gates() {
        let common = include_str!("../registration-common.ps1");
        let production = include_str!("../install.ps1");
        let test = include_str!("../install-test.ps1");
        let uninstall = include_str!("../uninstall.ps1");

        assert!(common.contains(CLSID_STRING));
        assert!(common.contains(PROVIDER_FRIENDLY_NAME));
        assert!(common.contains(DLL_FILE_NAME));
        assert!(common.contains("RegistryView]::Registry64"));
        assert!(common.contains("'ThreadingModel', 'Apartment'"));
        assert!(common.contains(r"Arcen\CredentialProvider"));
        assert!(!common.contains("Credential Provider Filters"));
        assert!(production.contains("Get-AuthenticodeSignature"));
        assert!(production.contains("ExpectedSignerThumbprint"));
        assert!(production.contains("x86_64-pc-windows-msvc"));
        assert!(test.contains("IUnderstandThisModifiesWinlogon"));
        assert!(test.contains("-Mode Test"));
        assert!(uninstall.contains("TestInstall"));
    }
}
