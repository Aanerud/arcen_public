//! Broker side of the SYSTEM-only Credential Provider control pipe, plus the
//! remote first-login orchestrator.
//!
//! The LocalSystem broker owns one named-pipe server (`\\.\pipe\arcen-…`)
//! protected by an explicit `D:(A;;FA;;;SY)` SDDL — only SYSTEM may open it. It
//! is created once, before any remote client is accepted, and runs for the life
//! of the process, accepting the Credential Provider (which LogonUI loads at the
//! secure desktop) and surviving CP reconnects. Each connection is verified out
//! of band: the peer process image must be `LogonUI.exe` and its token SID must
//! be SYSTEM, or the connection is dropped, fail-closed. The kernel ACL and the
//! peer checks are independent layers; neither alone is trusted.
//!
//! When an authenticated remote account has no existing session, the broker
//! seals its credential to the provider's per-Advise ephemeral key (see
//! [`arcen_cp_ipc`]) and pushes it, then polls WTS for a SID-matching
//! unlocked session before spawning the per-session agent. Only one first-login
//! runs at a time, machine-wide.

use std::sync::Arc;

use zeroize::Zeroizing;

use crate::auth::AuthenticatedAccount;
use crate::first_login::{FirstLoginError, RequestIds};
use crate::windows_session::{SelectedSession, WindowsSessionIdentity};

pub use arcen_cp_ipc::UsageScenario;

/// Configuration for the credential pipe server.
#[derive(Clone, Debug)]
pub struct CpCoordinatorConfig {
    /// The pipe name. Defaults to [`arcen_cp_ipc::CP_PIPE_NAME`].
    pub pipe_name: String,
    /// The process image basename the connecting peer must present. LogonUI
    /// hosts the credential provider, so this is `LogonUI.exe`.
    pub expected_peer_image: String,
    /// Maximum time to wait for profile creation and a bindable WTS session
    /// after LogonUI has consumed the credential.
    pub session_timeout: std::time::Duration,
}

impl Default for CpCoordinatorConfig {
    fn default() -> Self {
        Self {
            pipe_name: arcen_cp_ipc::CP_PIPE_NAME.to_string(),
            expected_peer_image: "LogonUI.exe".to_string(),
            session_timeout: crate::first_login::SESSION_TIMEOUT,
        }
    }
}

/// Case-insensitive comparison of a process image path's file name against an
/// expected basename. Delegates to the single shared implementation in
/// `arcen_cp_ipc` so both pipe endpoints apply the identical rule.
pub use arcen_cp_ipc::image_basename_matches;

/// Machine-wide coordinator for the credential pipe and the single in-flight
/// first-login. Cheap to clone via `Arc`.
pub struct CpCoordinator {
    /// One global pending-login gate (bounded singleton), coordinated alongside
    /// the streaming `BrokerAgentLease`.
    pending: tokio::sync::Semaphore,
    request_ids: RequestIds,
    session_timeout: std::time::Duration,
    #[cfg(windows)]
    server: Arc<windows_impl::Server>,
    #[cfg(not(windows))]
    _config: CpCoordinatorConfig,
}

impl CpCoordinator {
    /// Start the coordinator, launching the pipe server so a Credential Provider
    /// can connect even before the first remote client is accepted.
    pub fn start(config: CpCoordinatorConfig) -> Arc<Self> {
        #[cfg(windows)]
        {
            let session_timeout = config.session_timeout;
            let server = windows_impl::Server::start(config);
            Arc::new(Self {
                pending: tokio::sync::Semaphore::new(1),
                request_ids: RequestIds::new(),
                session_timeout,
                server,
            })
        }
        #[cfg(not(windows))]
        {
            Arc::new(Self {
                pending: tokio::sync::Semaphore::new(1),
                request_ids: RequestIds::new(),
                session_timeout: config.session_timeout,
                _config: config,
            })
        }
    }

    /// Complete an exact-console logon or unlock through a fresh Credential
    /// Provider peer. Only one runs at a time (`Busy` otherwise). The credential
    /// is consumed here, and the result is rebound to the original SID and exact
    /// console session rather than any same-SID remote/stale session.
    #[cfg(windows)]
    pub async fn first_login(
        &self,
        account: &AuthenticatedAccount,
        credential: Zeroizing<String>,
        usage: UsageScenario,
        target_session: u32,
        correlation_id: &str,
    ) -> Result<(WindowsSessionIdentity, SelectedSession), FirstLoginError> {
        use arcen_cp_ipc::CredentialPayload;

        let _permit = self
            .pending
            .try_acquire()
            .map_err(|_| FirstLoginError::Busy)?;

        let account_sid = account.string_sid().map_err(FirstLoginError::Payload)?;
        let payload = CredentialPayload::new(account.canonical_name(), &credential)
            .map_err(|error| FirstLoginError::Payload(error.to_string()))?;
        // The credential now lives only inside the sealed payload; scrub the copy.
        drop(credential);
        let request_id = self.request_ids.next_id();

        tracing::info!(
            target: crate::logging::CPPIPE,
            correlation_id,
            request_id,
            ?usage,
            "remote first-login: waiting for a ready credential provider"
        );

        let observed_console = crate::logon_activation::active_console_session()
            .map_err(FirstLoginError::PushFailed)?;
        if observed_console != target_session {
            return Err(FirstLoginError::PushFailed(
                "the classified console session is no longer active".into(),
            ));
        }
        let initial_generation = self.server.generation_watermark();
        self.server
            .recycle_session_through(target_session, initial_generation);
        let activated_session =
            crate::logon_activation::activate_console().map_err(FirstLoginError::PushFailed)?;
        if activated_session != target_session {
            return Err(FirstLoginError::PushFailed(
                "the active console session changed during logon activation".into(),
            ));
        }
        let post_sas_generation = self.server.generation_watermark();
        self.server
            .recycle_session_through(target_session, post_sas_generation);

        let ready = self
            .server
            .wait_ready(
                crate::first_login::READY_TIMEOUT,
                crate::first_login::acceptable_scenarios(usage),
                target_session,
                post_sas_generation,
            )
            .await
            .ok_or(FirstLoginError::NoCredentialProvider)?;
        if crate::logon_activation::active_console_session().map_err(FirstLoginError::PushFailed)?
            != target_session
        {
            return Err(FirstLoginError::PushFailed(
                "the active console session changed before credential dispatch".into(),
            ));
        }
        // Validate against the scenario LogonUI actually armed for, not the one
        // classified from WTS state. The provider builds its LSA buffer from the
        // same value, so this keeps the check and the credential describing the
        // same operation.
        let observed_usage = ready.usage();
        if observed_usage != usage {
            tracing::info!(
                target: crate::logging::CPPIPE,
                correlation_id,
                request_id,
                expected = ?usage,
                observed = ?observed_usage,
                "credential provider armed for a different scenario than the console state implied"
            );
        }
        crate::windows_session::validate_cp_target(account, observed_usage, target_session)
            .map_err(FirstLoginError::PushFailed)?;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.server
            .dispatch(
                &ready,
                windows_impl::PushJob {
                    account_sid,
                    request_id,
                    payload,
                    reply: reply_tx,
                },
            )
            .map_err(|_| {
                FirstLoginError::PushFailed("credential provider disconnected before push".into())
            })?;
        let armed = reply_rx
            .await
            .map_err(|_| FirstLoginError::PushFailed("no push acknowledgement".into()))?
            .map_err(FirstLoginError::PushFailed)?;
        if !armed {
            return Err(FirstLoginError::PushFailed(
                "credential provider did not arm the pushed credential".into(),
            ));
        }
        tracing::info!(
            target: crate::logging::CPPIPE,
            correlation_id,
            request_id,
            "remote first-login: credential armed; polling for the new session"
        );

        // Re-run SID matching (never trust the CP-reported username) until an
        // unlocked SID-matching session remains continuously stable through the
        // LogonUI -> user-desktop transition or the bounded deadline passes.
        let deadline = tokio::time::Instant::now() + self.session_timeout;
        let stability_clock = tokio::time::Instant::now();
        let mut stability = crate::first_login::SessionStability::default();
        let mut stability_announced = false;
        loop {
            match crate::windows_session::reclassify_expected_console(account, target_session) {
                Ok(Some((identity, selected))) => {
                    let stable = stability
                        .observe_strict(stability_clock.elapsed(), Ok(true))
                        .unwrap_or(false);
                    if !stable {
                        if !stability_announced {
                            tracing::info!(
                                target: crate::logging::CPPIPE,
                                correlation_id,
                                windows_session_id = identity.session_id,
                                stability_ms = crate::first_login::POST_LOGIN_STABILITY.as_millis(),
                                "remote first-login: exact session observed; waiting for desktop transition stability"
                            );
                            stability_announced = true;
                        }
                        drop(selected);
                    } else {
                        tracing::info!(
                            target: crate::logging::CPPIPE,
                            correlation_id,
                            windows_session_id = identity.session_id,
                            "remote first-login: exact session remained stable through desktop transition"
                        );
                        return Ok((identity, selected));
                    }
                }
                Ok(None) => {
                    let _ = stability.observe_strict(stability_clock.elapsed(), Ok(false));
                    stability_announced = false;
                }
                Err(error) => {
                    let _ =
                        stability.observe_strict(stability_clock.elapsed(), Err(error.as_str()));
                    return Err(FirstLoginError::SessionProbe(error));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(FirstLoginError::SessionTimeout);
            }
            tokio::time::sleep(crate::first_login::POLL_INTERVAL).await;
        }
    }

    #[cfg(not(windows))]
    pub async fn first_login(
        &self,
        _account: &AuthenticatedAccount,
        _credential: Zeroizing<String>,
        _usage: UsageScenario,
        _target_session: u32,
        _correlation_id: &str,
    ) -> Result<(WindowsSessionIdentity, SelectedSession), FirstLoginError> {
        Err(FirstLoginError::Unsupported)
    }
}

#[cfg(windows)]
mod windows_impl {
    use std::collections::HashMap;
    use std::os::windows::io::FromRawHandle;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use arcen_cp_ipc::broker::{push_credential, recv_ready};
    use arcen_cp_ipc::transport::StreamFrames;
    use arcen_cp_ipc::{CredentialPayload, NonceTracker, UsageScenario};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{
        CloseHandle, LocalFree, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL, INVALID_HANDLE_VALUE,
    };
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        CreateWellKnownSid, EqualSid, GetTokenInformation, TokenUser, WinLocalSystemSid,
        PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    };
    use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
        PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    use super::{image_basename_matches, CpCoordinatorConfig};

    /// A push request handed from the async `first_login` to the connection
    /// thread that owns the verified CP pipe.
    pub struct PushJob {
        pub account_sid: String,
        pub request_id: u64,
        pub payload: CredentialPayload,
        pub reply: tokio::sync::oneshot::Sender<Result<bool, String>>,
    }

    enum ConnectionJob {
        Push(PushJob),
        Recycle,
    }

    struct ReadyPeer {
        generation: u64,
        usage: UsageScenario,
        pid: u32,
        session_id: u32,
        job_tx: std::sync::mpsc::Sender<ConnectionJob>,
    }

    #[derive(Default)]
    struct ServerState {
        ready: HashMap<u64, ReadyPeer>,
    }

    impl ServerState {
        /// Pick a ready provider for this session, taking `acceptable` in
        /// preference order so the scenario the broker expected still wins when
        /// LogonUI offers it.
        fn select(
            &self,
            acceptable: &[UsageScenario],
            session_id: u32,
            min_generation: u64,
        ) -> Option<ReadyTicket> {
            acceptable.iter().find_map(|wanted| {
                self.ready
                    .values()
                    .filter(|peer| {
                        peer.generation > min_generation
                            && peer.usage == *wanted
                            && peer.session_id == session_id
                    })
                    .max_by_key(|peer| peer.generation)
                    .map(|peer| ReadyTicket {
                        generation: peer.generation,
                        usage: peer.usage,
                        pid: peer.pid,
                        session_id: peer.session_id,
                    })
            })
        }
    }

    #[derive(Clone, Copy)]
    pub struct ReadyTicket {
        generation: u64,
        usage: UsageScenario,
        pid: u32,
        session_id: u32,
    }

    impl ReadyTicket {
        /// The scenario LogonUI actually armed the provider for, which is what
        /// the credential is validated against — not what the broker guessed
        /// from WTS state.
        pub const fn usage(&self) -> UsageScenario {
            self.usage
        }
    }

    pub struct Server {
        state: Mutex<ServerState>,
        shutdown: AtomicBool,
        generation: AtomicU64,
        expected_image: String,
        pipe_name: Vec<u16>,
    }

    /// How long a verified-but-idle connection waits for a push job before it
    /// recycles, so a stale CP connection cannot pin the readiness slot forever.
    ///
    /// INVARIANT: this MUST be shorter than the CP's session TTL
    /// (`credential-provider::pipe::SESSION_EXPIRY_MS`, 5 min). The broker seals a
    /// pushed credential to whatever CP is "ready"; if a ready connection outlives
    /// its session key, `ingest` returns `Expired` and first-login fails with
    /// "credential provider did not arm". Recycling at 4 min forces the CP to
    /// reconnect and re-arm a fresh session before the 5-min key expires.
    const IDLE_CONNECTION_TIMEOUT: Duration = Duration::from_secs(240);

    impl Server {
        pub fn start(config: CpCoordinatorConfig) -> Arc<Self> {
            let server = Arc::new(Self {
                state: Mutex::new(ServerState::default()),
                shutdown: AtomicBool::new(false),
                generation: AtomicU64::new(0),
                expected_image: config.expected_peer_image,
                pipe_name: wide(&config.pipe_name),
            });
            let accept = Arc::clone(&server);
            std::thread::Builder::new()
                .name("arcen-cp-pipe".to_string())
                .spawn(move || accept.accept_loop())
                .expect("spawn credential pipe accept thread");
            server
        }

        pub fn generation_watermark(&self) -> u64 {
            self.generation.load(Ordering::Acquire)
        }

        pub fn recycle_session_through(&self, session_id: u32, max_generation: u64) {
            let senders = self
                .state
                .lock()
                .map(|state| {
                    state
                        .ready
                        .values()
                        .filter(|peer| {
                            peer.session_id == session_id && peer.generation <= max_generation
                        })
                        .map(|peer| peer.job_tx.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            for sender in senders {
                let _ = sender.send(ConnectionJob::Recycle);
            }
        }

        /// Poll for a freshly verified CP in the exact target session/scenario.
        pub async fn wait_ready(
            &self,
            timeout: Duration,
            acceptable: &[UsageScenario],
            session_id: u32,
            min_generation: u64,
        ) -> Option<ReadyTicket> {
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                if let Some(ticket) = self.current_ready(acceptable, session_id, min_generation) {
                    return Some(ticket);
                }
                if tokio::time::Instant::now() >= deadline {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }

        fn current_ready(
            &self,
            acceptable: &[UsageScenario],
            session_id: u32,
            min_generation: u64,
        ) -> Option<ReadyTicket> {
            let state = self.state.lock().ok()?;
            state.select(acceptable, session_id, min_generation)
        }

        pub fn dispatch(&self, ticket: &ReadyTicket, job: PushJob) -> Result<(), ()> {
            let sender = {
                let state = self.state.lock().map_err(|_| ())?;
                let peer = state.ready.get(&ticket.generation).ok_or(())?;
                if peer.usage != ticket.usage
                    || peer.pid != ticket.pid
                    || peer.session_id != ticket.session_id
                {
                    return Err(());
                }
                peer.job_tx.clone()
            };
            sender.send(ConnectionJob::Push(job)).map_err(|_| ())
        }

        fn accept_loop(self: Arc<Self>) {
            // The replacement instance, created *before* the connected one is
            // handed to its thread.
            //
            // FILE_FLAG_FIRST_PIPE_INSTANCE only protects the create it is
            // passed to. The original loop created an instance, waited for a
            // connection, handed the handle off, and only then created the
            // next one — so between the connection thread closing its handle
            // and the next create, the process could hold zero instances and
            // the name was momentarily free. A process that created the pipe in
            // that window would own the first instance, and therefore the only
            // security descriptor the kernel honours: SECURITY_ATTRIBUTES on
            // non-first instances are ignored, so the broker's own
            // `D:(A;;FA;;;SY)` would be silently discarded and its next
            // CreateNamedPipeW would quietly add an instance to the squatter's
            // pipe. Connection recycling is routine, so the window recurs.
            //
            // Two rules close it. Pre-create the replacement while still
            // holding the current instance, so the name is never unowned; and
            // whenever we nevertheless hold nothing, demand
            // FILE_FLAG_FIRST_PIPE_INSTANCE, which fails closed against a
            // squatter rather than attaching to them. `pending.is_none()` is
            // exactly "we hold no instance", so the flag needs no separate
            // bookkeeping that could drift out of step with reality.
            let mut pending: Option<HANDLE> = None;
            while !self.shutdown.load(Ordering::Acquire) {
                let pipe = match pending.take() {
                    Some(pipe) => pipe,
                    None => match self.create_pipe_instance(true) {
                        Ok(pipe) => pipe,
                        Err(error) => {
                            tracing::error!(
                                target: crate::logging::CPPIPE,
                                %error,
                                "failed to create credential pipe instance; retrying"
                            );
                            std::thread::sleep(Duration::from_secs(1));
                            continue;
                        }
                    },
                };

                // SAFETY: `pipe` is a valid server pipe handle owned here.
                let connected = match unsafe { ConnectNamedPipe(pipe, None) } {
                    Ok(()) => true,
                    Err(error) if error.code() == ERROR_PIPE_CONNECTED.to_hresult() => true,
                    Err(error) => {
                        tracing::warn!(target: crate::logging::CPPIPE, %error, "ConnectNamedPipe failed");
                        false
                    }
                };
                if !connected {
                    // SAFETY: closing the unconnected server handle exactly once.
                    unsafe {
                        let _ = CloseHandle(pipe);
                    }
                    continue;
                }

                // Still holding the connected instance here, so the name cannot
                // become free while this runs.
                match self.create_pipe_instance(false) {
                    Ok(next) => pending = Some(next),
                    Err(error) => tracing::warn!(
                        target: crate::logging::CPPIPE,
                        %error,
                        "could not pre-create the next credential pipe instance; \
                         the next create will demand a first instance"
                    ),
                }

                let server = Arc::clone(&self);
                let pipe_raw = pipe.0 as isize;
                if let Err(error) = std::thread::Builder::new()
                    .name("arcen-cp-conn".to_string())
                    .spawn(move || server.handle_connection(pipe_raw))
                {
                    tracing::error!(target: crate::logging::CPPIPE, %error, "spawn CP connection thread failed");
                    // SAFETY: no handler took ownership; close the handle once.
                    unsafe {
                        let _ = DisconnectNamedPipe(pipe);
                        let _ = CloseHandle(pipe);
                    }
                }
            }

            if let Some(pipe) = pending.take() {
                // SAFETY: the unused pre-created instance is owned here and
                // closed exactly once on shutdown.
                unsafe {
                    let _ = CloseHandle(pipe);
                }
            }
        }

        fn handle_connection(self: Arc<Self>, pipe_raw: isize) {
            // The raw handle value crossed the thread boundary (HANDLE is not
            // Send); reconstruct it here where this thread uniquely owns it.
            let pipe = HANDLE(pipe_raw as *mut _);
            // Verify the peer before reading anything. Fail closed on any doubt.
            let peer = match self.verify_peer(pipe) {
                Ok(peer) => {
                    tracing::info!(
                        target: crate::logging::CPPIPE,
                        peer_pid = peer.pid,
                        windows_session_id = peer.session_id,
                        "credential provider connected and passed peer verification"
                    );
                    peer
                }
                Err(error) => {
                    tracing::warn!(target: crate::logging::CPPIPE, %error, "rejecting CP peer");
                    // SAFETY: the rejected connection is disconnected and closed once.
                    unsafe {
                        let _ = DisconnectNamedPipe(pipe);
                        let _ = CloseHandle(pipe);
                    }
                    return;
                }
            };

            // Ownership of the handle moves into the File wrapper for framed I/O.
            // SAFETY: `pipe` is a live, verified, connected pipe handle owned here
            // and consumed exactly once by from_raw_handle.
            let file = unsafe { std::fs::File::from_raw_handle(pipe.0 as *mut _) };
            let mut frames = StreamFrames::new(file);
            let mut tracker = NonceTracker::new();

            let readiness = match recv_ready(&mut frames, &mut tracker) {
                Ok(readiness) => readiness,
                Err(error) => {
                    tracing::warn!(target: crate::logging::CPPIPE, %error, "CP readiness handshake failed");
                    return; // dropping `frames` closes the pipe
                }
            };
            if readiness.pid != peer.pid {
                tracing::warn!(
                    target: crate::logging::CPPIPE,
                    peer_pid = peer.pid,
                    ready_pid = readiness.pid,
                    "credential provider Ready PID does not match the connected process"
                );
                return;
            }

            let (job_tx, job_rx) = std::sync::mpsc::channel::<ConnectionJob>();
            let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
            if let Ok(mut state) = self.state.lock() {
                state.ready.insert(
                    generation,
                    ReadyPeer {
                        generation,
                        usage: readiness.usage,
                        pid: peer.pid,
                        session_id: peer.session_id,
                        job_tx,
                    },
                );
            }

            // Serve one push (a session is single-use per Advise lifecycle) then
            // recycle. An idle timeout keeps a stale CP from pinning the slot.
            match job_rx.recv_timeout(IDLE_CONNECTION_TIMEOUT) {
                Ok(ConnectionJob::Push(job)) => {
                    let PushJob {
                        account_sid,
                        request_id,
                        payload,
                        reply,
                    } = job;
                    let push_nonce = request_id ^ 0x5a5a_5a5a_5a5a_5a5a;
                    let result = push_credential(
                        &mut frames,
                        &mut tracker,
                        &readiness,
                        &account_sid,
                        request_id,
                        push_nonce.max(1),
                        &payload,
                        true, // peer already verified at connect time
                    )
                    .map_err(|error| error.to_string());
                    // `payload` is dropped here (scrubbed) regardless of outcome.
                    let _ = reply.send(result);
                }
                Ok(ConnectionJob::Recycle) => {
                    tracing::debug!(
                        target: crate::logging::CPPIPE,
                        generation,
                        windows_session_id = peer.session_id,
                        "recycling credential provider connection"
                    );
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    tracing::debug!(target: crate::logging::CPPIPE, "CP connection idle timeout; recycling");
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
            }

            self.deregister(generation);
            // `frames` (and the pipe handle) drop here.
        }

        fn deregister(&self, generation: u64) {
            if let Ok(mut state) = self.state.lock() {
                state.ready.remove(&generation);
            }
        }

        fn create_pipe_instance(&self, first_instance: bool) -> Result<HANDLE, String> {
            let descriptor = SecurityDescriptor::system_only()?;
            let mut open_mode = PIPE_ACCESS_DUPLEX;
            if first_instance {
                // Guard against a squatter pre-creating the pipe name: only the
                // very first instance may exist, or creation fails.
                open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
            }
            let attributes = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0 .0,
                bInheritHandle: false.into(),
            };
            // SAFETY: name is NUL-terminated; the security descriptor lives for
            // the call; byte-mode + reject-remote is intentional.
            let pipe = unsafe {
                CreateNamedPipeW(
                    PCWSTR(self.pipe_name.as_ptr()),
                    open_mode,
                    PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    PIPE_UNLIMITED_INSTANCES,
                    64 * 1024,
                    64 * 1024,
                    0,
                    Some(&attributes),
                )
            };
            if pipe.is_invalid() || pipe == INVALID_HANDLE_VALUE {
                return Err(format!(
                    "CreateNamedPipeW: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(pipe)
        }

        /// Confirm the connected peer is `LogonUI.exe` running as SYSTEM.
        fn verify_peer(&self, pipe: HANDLE) -> Result<PeerIdentity, String> {
            let mut pid = 0u32;
            // SAFETY: pipe is a live connected server handle; pid is a valid out-param.
            unsafe { GetNamedPipeClientProcessId(pipe, &mut pid) }
                .map_err(|error| format!("GetNamedPipeClientProcessId: {error}"))?;

            // SAFETY: pid came from the kernel; limited-information access is enough.
            let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
                .map_err(|error| format!("OpenProcess(client pid {pid}): {error}"))?;
            let process = OwnedHandle(process);

            let image = query_image_name(process.0)?;
            if !image_basename_matches(&image, &self.expected_image) {
                return Err(format!(
                    "peer image {image:?} is not the expected {:?}",
                    self.expected_image
                ));
            }
            if !process_is_system(process.0)? {
                return Err("peer process is not running as SYSTEM".to_string());
            }
            let mut session_id = 0u32;
            // SAFETY: pid came from the kernel and session_id is a valid out-param.
            unsafe { ProcessIdToSessionId(pid, &mut session_id) }
                .map_err(|error| format!("ProcessIdToSessionId({pid}): {error}"))?;
            Ok(PeerIdentity { pid, session_id })
        }
    }

    struct PeerIdentity {
        pid: u32,
        session_id: u32,
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn peer(generation: u64, usage: UsageScenario, pid: u32, session_id: u32) -> ReadyPeer {
            let (job_tx, _job_rx) = std::sync::mpsc::channel();
            ReadyPeer {
                generation,
                usage,
                pid,
                session_id,
                job_tx,
            }
        }

        #[test]
        fn ready_selection_requires_fresh_exact_console_logon() {
            let mut state = ServerState::default();
            state.ready.insert(1, peer(1, UsageScenario::Logon, 100, 1));
            state
                .ready
                .insert(2, peer(2, UsageScenario::UnlockWorkstation, 101, 1));
            state.ready.insert(3, peer(3, UsageScenario::Logon, 102, 7));
            state.ready.insert(4, peer(4, UsageScenario::Logon, 103, 1));

            let selected = state
                .select(&[UsageScenario::Logon], 1, 1)
                .expect("fresh console logon");
            assert_eq!(selected.generation, 4);
            assert_eq!(selected.pid, 103);
            assert!(state.select(&[UsageScenario::Logon], 1, 4).is_none());
            assert!(state
                .select(&[UsageScenario::UnlockWorkstation], 7, 0)
                .is_none());
        }

        #[test]
        fn preference_order_decides_which_armed_scenario_wins() {
            // Both screens are offered for the same console. The classified
            // scenario must be taken when it is there, so ordinary unlocks are
            // completely unaffected by the fallback.
            let mut state = ServerState::default();
            state.ready.insert(1, peer(5, UsageScenario::Logon, 200, 1));
            state
                .ready
                .insert(2, peer(6, UsageScenario::UnlockWorkstation, 201, 1));
            let accepted =
                crate::first_login::acceptable_scenarios(UsageScenario::UnlockWorkstation);
            let selected = state.select(accepted, 1, 0).expect("a provider is ready");
            assert_eq!(selected.usage(), UsageScenario::UnlockWorkstation);
            assert_eq!(selected.pid, 201);

            // Only the logon screen is armed -- the pier-windows-software.example.internal case, which
            // used to time out with a connected, verified provider present.
            let mut only_logon = ServerState::default();
            only_logon
                .ready
                .insert(1, peer(7, UsageScenario::Logon, 202, 1));
            let selected = only_logon
                .select(accepted, 1, 0)
                .expect("logon screen must satisfy a locked console");
            assert_eq!(selected.usage(), UsageScenario::Logon);
            assert_eq!(selected.pid, 202);

            // The converse must still find nothing: a console with no session
            // cannot be satisfied by an unlock screen.
            let mut only_unlock = ServerState::default();
            only_unlock
                .ready
                .insert(1, peer(8, UsageScenario::UnlockWorkstation, 203, 1));
            assert!(only_unlock
                .select(
                    crate::first_login::acceptable_scenarios(UsageScenario::Logon),
                    1,
                    0
                )
                .is_none());
        }
    }

    /// RAII for a plain kernel HANDLE (not a pipe endpoint moved into a File).
    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                // SAFETY: this guard uniquely owns the handle.
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
    }

    /// RAII for a LocalAlloc'd security descriptor.
    struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

    impl SecurityDescriptor {
        fn system_only() -> Result<Self, String> {
            let sddl = wide("D:(A;;FA;;;SY)");
            let mut descriptor = PSECURITY_DESCRIPTOR::default();
            // SAFETY: sddl is NUL-terminated; descriptor is a valid out-param that
            // receives a LocalAlloc'd security descriptor freed on drop.
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    PCWSTR(sddl.as_ptr()),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    None,
                )
            }
            .map_err(|error| {
                format!("ConvertStringSecurityDescriptorToSecurityDescriptorW: {error}")
            })?;
            Ok(Self(descriptor))
        }
    }

    impl Drop for SecurityDescriptor {
        fn drop(&mut self) {
            if !self.0 .0.is_null() {
                // SAFETY: the descriptor was LocalAlloc'd by the conversion API.
                unsafe {
                    let _ = LocalFree(HLOCAL(self.0 .0));
                }
            }
        }
    }

    fn query_image_name(process: HANDLE) -> Result<String, String> {
        let mut buffer = vec![0u16; 1024];
        let mut size = buffer.len() as u32;
        // SAFETY: process is a live handle; buffer/size are valid out-params.
        unsafe {
            QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                windows::core::PWSTR(buffer.as_mut_ptr()),
                &mut size,
            )
        }
        .map_err(|error| format!("QueryFullProcessImageNameW: {error}"))?;
        Ok(String::from_utf16_lossy(&buffer[..size as usize]))
    }

    fn process_is_system(process: HANDLE) -> Result<bool, String> {
        let mut token = HANDLE::default();
        // SAFETY: process is a live handle; token is a valid out-param.
        unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }
            .map_err(|error| format!("OpenProcessToken: {error}"))?;
        let token = OwnedHandle(token);

        let mut bytes = 0u32;
        // SAFETY: sizing call with no output buffer.
        let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut bytes) };
        if bytes < std::mem::size_of::<TOKEN_USER>() as u32 {
            return Err("GetTokenInformation(TokenUser) sizing failed".to_string());
        }
        let mut storage = vec![0u8; bytes as usize];
        // SAFETY: storage is at least the sized byte count and aligned for TOKEN_USER.
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                Some(storage.as_mut_ptr().cast()),
                bytes,
                &mut bytes,
            )
        }
        .map_err(|error| format!("GetTokenInformation(TokenUser): {error}"))?;
        // SAFETY: storage holds a TOKEN_USER whose Sid points within it.
        let user_sid = unsafe { (*(storage.as_ptr().cast::<TOKEN_USER>())).User.Sid };

        let mut system = vec![0u8; 128];
        let mut system_len = system.len() as u32;
        // SAFETY: system buffer/len are valid out-params for the well-known SID.
        unsafe {
            CreateWellKnownSid(
                WinLocalSystemSid,
                None,
                PSID(system.as_mut_ptr().cast()),
                &mut system_len,
            )
        }
        .map_err(|error| format!("CreateWellKnownSid(SYSTEM): {error}"))?;

        // SAFETY: both SIDs are valid and live for the comparison.
        Ok(unsafe { EqualSid(user_sid, PSID(system.as_ptr() as *mut _)) }.is_ok())
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_image_matches_are_case_insensitive_and_basename_only() {
        assert!(image_basename_matches(
            r"C:\Windows\System32\LogonUI.exe",
            "LogonUI.exe"
        ));
        assert!(image_basename_matches(
            r"C:\Windows\System32\logonui.EXE",
            "LogonUI.exe"
        ));
        assert!(!image_basename_matches(
            r"C:\evil\LogonUI.exe.malware.exe",
            "LogonUI.exe"
        ));
        assert!(!image_basename_matches(
            r"C:\Windows\System32\explorer.exe",
            "LogonUI.exe"
        ));
        // A bare name with no separators still compares by basename.
        assert!(image_basename_matches("LogonUI.exe", "LogonUI.exe"));
    }

    #[test]
    fn default_config_targets_logonui_and_the_reserved_pipe() {
        let config = CpCoordinatorConfig::default();
        assert_eq!(config.expected_peer_image, "LogonUI.exe");
        assert_eq!(config.pipe_name, arcen_cp_ipc::CP_PIPE_NAME);
        assert_eq!(config.session_timeout, crate::first_login::SESSION_TIMEOUT);
    }
}
