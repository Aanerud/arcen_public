//! Windows credential verification for the machine broker.
//!
//! `LogonUserW` proves the supplied account secret, but it does not create an
//! interactive Windows session. The returned SID is retained only long enough
//! to bind the authenticated account to an existing WTS session token.

use std::sync::Arc;

use tokio::sync::OwnedSemaphorePermit;
use zeroize::Zeroizing;

pub struct PreauthGuard {
    _permit: OwnedSemaphorePermit,
}

impl PreauthGuard {
    pub fn new(permit: OwnedSemaphorePermit) -> Arc<Self> {
        Arc::new(Self { _permit: permit })
    }
}

#[derive(Debug)]
pub struct AuthenticatedAccount {
    requested_name: String,
    canonical_name: String,
    #[cfg(windows)]
    sid_storage: Vec<usize>,
}

impl AuthenticatedAccount {
    pub fn requested_name(&self) -> &str {
        &self.requested_name
    }

    /// The SID-resolved Windows account name handed to Winlogon.
    ///
    /// Local accounts use `MACHINE\user`; domain accounts use `DOMAIN\user`.
    pub fn canonical_name(&self) -> &str {
        &self.canonical_name
    }

    #[cfg(windows)]
    pub fn matches_token(&self, token: windows::Win32::Foundation::HANDLE) -> Result<bool, String> {
        use windows::Win32::Security::EqualSid;

        let candidate = token_user_buffer(token)?;
        // SAFETY: both PSIDs point into aligned TOKEN_USER buffers that remain alive for this call.
        Ok(unsafe {
            EqualSid(
                token_user_sid(&self.sid_storage),
                token_user_sid(&candidate),
            )
        }
        .is_ok())
    }

    /// The authenticated account's SID as an `S-1-...` string. Used only to bind
    /// the sealed-credential transcript for the first-login handoff; it is not a
    /// secret, and its integrity on the wire is guaranteed by the AEAD tag.
    #[cfg(windows)]
    pub fn string_sid(&self) -> Result<String, String> {
        sid_string(token_user_sid(&self.sid_storage))
    }

    #[cfg(not(windows))]
    pub fn string_sid(&self) -> Result<String, String> {
        Err("Windows account SID is unavailable on this platform".to_string())
    }
}

#[cfg(windows)]
pub(crate) fn token_string_sid(
    token: windows::Win32::Foundation::HANDLE,
) -> Result<String, String> {
    let storage = token_user_buffer(token)?;
    sid_string(token_user_sid(&storage))
}

#[cfg(windows)]
fn sid_string(sid: windows::Win32::Security::PSID) -> Result<String, String> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;

    let mut out = PWSTR::null();
    // SAFETY: `sid` points into a live TOKEN_USER buffer for this call;
    // `out` receives one LocalAlloc'd NUL-terminated string.
    unsafe { ConvertSidToStringSidW(sid, &mut out) }
        .map_err(|error| format!("ConvertSidToStringSidW: {error}"))?;
    if out.is_null() {
        return Err("ConvertSidToStringSidW returned a null string".to_string());
    }
    // SAFETY: `out` is a NUL-terminated wide string owned by us until LocalFree.
    let result = unsafe { out.to_string() }.map_err(|error| format!("SID string: {error}"));
    // SAFETY: `out` was allocated by ConvertSidToStringSidW and is freed once.
    unsafe {
        let _ = LocalFree(HLOCAL(out.0.cast()));
    }
    result
}

pub async fn authenticate_windows(
    username: String,
    credential: Zeroizing<String>,
    preauth_guard: Arc<PreauthGuard>,
) -> Result<(AuthenticatedAccount, Zeroizing<String>), String> {
    run_blocking_auth(preauth_guard, move || {
        // Validate with a borrow, then hand the still-zeroizing credential back
        // so the broker can reuse it for a first-login push without ever making
        // a second copy of the secret. On failure the credential is scrubbed as
        // this closure's `credential` owner is dropped.
        authenticate_windows_blocking(&username, &credential).map(|account| (account, credential))
    })
    .await
}

async fn run_blocking_auth<R, F>(preauth_guard: Arc<PreauthGuard>, verify: F) -> Result<R, String>
where
    R: Send + 'static,
    F: FnOnce() -> Result<R, String> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let _preauth_guard = preauth_guard;
        verify()
    })
    .await
    .map_err(|error| {
        tracing::error!(
            target: crate::logging::AUTH,
            %error,
            "LogonUser worker failed"
        );
        "Windows authentication worker failed".to_string()
    })?
}

#[cfg(windows)]
fn authenticate_windows_blocking(
    username: &str,
    password: &str,
) -> Result<AuthenticatedAccount, String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        LogonUserW, LOGON32_LOGON_INTERACTIVE, LOGON32_PROVIDER_DEFAULT,
    };

    if username.is_empty() || password.is_empty() {
        return Err("Invalid credentials".to_string());
    }

    let (user, domain): (String, Option<String>) =
        if let Some((domain, user)) = username.split_once('\\') {
            (user.to_string(), Some(domain.to_string()))
        } else if username.contains('@') {
            (username.to_string(), None)
        } else {
            (username.to_string(), Some(".".to_string()))
        };
    let user_w = to_wide(&user);
    let pass_w = Zeroizing::new(to_wide(password));
    let domain_w = domain.as_deref().map(to_wide);
    let domain_p = domain_w
        .as_ref()
        .map_or_else(PCWSTR::null, |value| PCWSTR(value.as_ptr()));

    let mut token = HANDLE::default();
    // SAFETY: strings are NUL-terminated and live through the call; token is a valid out-param.
    let result = unsafe {
        LogonUserW(
            PCWSTR(user_w.as_ptr()),
            domain_p,
            PCWSTR(pass_w.as_ptr()),
            LOGON32_LOGON_INTERACTIVE,
            LOGON32_PROVIDER_DEFAULT,
            &mut token,
        )
    };
    if let Err(error) = result {
        tracing::warn!(
            target: crate::logging::AUTH,
            user = %username,
            code = error.code().0,
            "LogonUser failed"
        );
        return Err("Invalid credentials".to_string());
    }

    let sid_storage = token_user_buffer(token);
    // SAFETY: token is the valid handle returned by LogonUserW and is closed exactly once.
    unsafe {
        let _ = CloseHandle(token);
    }
    let sid_storage = sid_storage.map_err(|error| {
        tracing::error!(
            target: crate::logging::AUTH,
            user = %username,
            %error,
            "cannot read authenticated Windows account SID"
        );
        "Windows account identity could not be verified".to_string()
    })?;
    let canonical_name = lookup_account_name(token_user_sid(&sid_storage)).map_err(|error| {
        tracing::error!(
            target: crate::logging::AUTH,
            user = %username,
            %error,
            "cannot resolve authenticated Windows account name"
        );
        "Windows account identity could not be resolved".to_string()
    })?;
    tracing::info!(target: crate::logging::AUTH, user = %username, "LogonUser OK");
    Ok(AuthenticatedAccount {
        requested_name: username.to_string(),
        canonical_name,
        sid_storage,
    })
}

#[cfg(windows)]
fn token_user_buffer(token: windows::Win32::Foundation::HANDLE) -> Result<Vec<usize>, String> {
    use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_USER};

    let mut bytes = 0u32;
    // SAFETY: documented sizing call with no output buffer and a valid length out-param.
    let sizing = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut bytes) };
    if bytes < std::mem::size_of::<TOKEN_USER>() as u32 {
        return Err(format!(
            "GetTokenInformation(TokenUser) sizing failed: {sizing:?}"
        ));
    }
    let words = (bytes as usize).div_ceil(std::mem::size_of::<usize>());
    let mut storage = vec![0usize; words];
    // SAFETY: storage is aligned and has at least the byte count returned by the sizing call.
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(storage.as_mut_ptr().cast()),
            bytes,
            &mut bytes,
        )
    }
    .map_err(|error| format!("GetTokenInformation(TokenUser): {error}"))?;
    Ok(storage)
}

#[cfg(windows)]
fn token_user_sid(storage: &[usize]) -> windows::Win32::Security::PSID {
    use windows::Win32::Security::TOKEN_USER;

    // SAFETY: token_user_buffer allocated aligned storage initialized as TOKEN_USER.
    unsafe { (*(storage.as_ptr().cast::<TOKEN_USER>())).User.Sid }
}

#[cfg(windows)]
fn lookup_account_name(sid: windows::Win32::Security::PSID) -> Result<String, String> {
    use windows::core::PWSTR;
    use windows::Win32::Security::{LookupAccountSidW, SidTypeUser, SID_NAME_USE};

    let mut name_len = 0u32;
    let mut domain_len = 0u32;
    let mut use_type = SID_NAME_USE::default();
    // SAFETY: documented sizing call; the SID points into live aligned storage.
    let _ = unsafe {
        LookupAccountSidW(
            None,
            sid,
            PWSTR::null(),
            &mut name_len,
            PWSTR::null(),
            &mut domain_len,
            &mut use_type,
        )
    };
    if name_len == 0 || domain_len == 0 {
        return Err("LookupAccountSidW sizing returned empty account components".to_string());
    }

    let mut name = vec![0u16; name_len as usize];
    let mut domain = vec![0u16; domain_len as usize];
    // SAFETY: both buffers have the capacities returned by the sizing call and
    // the SID remains live for the duration of this call.
    unsafe {
        LookupAccountSidW(
            None,
            sid,
            PWSTR(name.as_mut_ptr()),
            &mut name_len,
            PWSTR(domain.as_mut_ptr()),
            &mut domain_len,
            &mut use_type,
        )
    }
    .map_err(|error| format!("LookupAccountSidW: {error}"))?;
    if use_type != SidTypeUser {
        return Err(format!(
            "LookupAccountSidW resolved a non-user SID type: {use_type:?}"
        ));
    }

    name.truncate(name_len as usize);
    domain.truncate(domain_len as usize);
    while name.last() == Some(&0) {
        name.pop();
    }
    while domain.last() == Some(&0) {
        domain.pop();
    }
    let name = String::from_utf16(&name)
        .map_err(|error| format!("account name is not UTF-16: {error}"))?;
    let domain = String::from_utf16(&domain)
        .map_err(|error| format!("account domain is not UTF-16: {error}"))?;
    qualify_account_name(&domain, &name)
}

fn qualify_account_name(domain: &str, user: &str) -> Result<String, String> {
    if domain.is_empty() || user.is_empty() {
        return Err("Windows account domain and user must both be non-empty".to_string());
    }
    if domain.contains(['\0', '\\']) || user.contains(['\0', '\\']) {
        return Err("Windows account components contain an invalid character".to_string());
    }
    Ok(format!(r"{domain}\{user}"))
}

#[cfg(windows)]
fn to_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(not(windows))]
fn authenticate_windows_blocking(
    username: &str,
    _password: &str,
) -> Result<AuthenticatedAccount, String> {
    tracing::warn!(
        target: crate::logging::AUTH,
        user = %username,
        "authenticate_windows stub on non-Windows build - denying"
    );
    Err("Windows authentication is unavailable on this platform".to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn canonical_account_name_requires_domain_and_user() {
        assert_eq!(
            super::qualify_account_name("EXAMPLEHOST", "operator").unwrap(),
            r"EXAMPLEHOST\operator"
        );
        assert_eq!(
            super::qualify_account_name("CORP", "artist").unwrap(),
            r"CORP\artist"
        );
        assert!(super::qualify_account_name("", "artist").is_err());
        assert!(super::qualify_account_name("CORP", "").is_err());
        assert!(super::qualify_account_name("BAD\\DOMAIN", "artist").is_err());
    }

    #[tokio::test]
    async fn portable_logon_worker_denies_without_blocking_runtime() {
        #[cfg(not(windows))]
        {
            use zeroize::Zeroizing;
            let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
            let guard = super::PreauthGuard::new(slots.clone().try_acquire_owned().expect("slot"));
            assert!(super::authenticate_windows(
                "user".into(),
                Zeroizing::new("password".to_string()),
                guard
            )
            .await
            .is_err());
            assert!(slots.try_acquire_owned().is_ok());
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_auth_keeps_slot_until_blocking_worker_finishes() {
        let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let guard = super::PreauthGuard::new(slots.clone().try_acquire_owned().expect("slot"));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let auth = tokio::spawn(super::run_blocking_auth::<(), _>(guard, move || {
            let _ = started_tx.send(());
            let _ = release_rx.recv();
            Err("cancelled test".to_string())
        }));

        started_rx.await.expect("worker started");
        auth.abort();
        let _ = auth.await;
        assert!(slots.clone().try_acquire_owned().is_err());

        release_tx.send(()).expect("release worker");
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if slots.clone().try_acquire_owned().is_ok() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("worker releases preauth slot");
    }
}
