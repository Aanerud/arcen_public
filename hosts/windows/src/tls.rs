// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Windows Pier TLS material: the Windows file-system checks behind the
//! shared `arcen_transport::host_material` loader, and the broker's lifecycle
//! events.
//!
//! Reading, validation, pinning, reload and expiry are shared with every Pier.
//! What is Windows' own is how a file is opened and judged: every ancestor is
//! opened without following reparse points and held open without
//! `FILE_SHARE_DELETE` until the leaf is open, only local disk paths are
//! accepted, and the private key's DACL must be protected and grant full
//! control to SYSTEM and Administrators only.

use std::path::Path;
#[cfg(windows)]
use std::path::{Component, PathBuf, Prefix};
use std::sync::Arc;

use arcen_transport::host_material::{
    CertificateEvent, HostTlsError, HostTlsMaterial, HostTlsSource, MaterialKind, SecureFileSystem,
    WindowsFileFacts,
};
use arcen_transport::tls::SystemUnixClock;
#[cfg(feature = "wss-compat")]
use tokio_rustls::TlsAcceptor;

/// The process named in certificate lifecycle events.
const COMPONENT: &str = "pier_broker";

/// The material this Pier serves, and the WSS acceptor over it when built.
pub(crate) struct TlsLifecycle {
    material: HostTlsMaterial,
    #[cfg(feature = "wss-compat")]
    acceptor: TlsAcceptor,
}

impl TlsLifecycle {
    pub(crate) fn load(source: HostTlsSource) -> Result<Self, HostTlsError> {
        let material = HostTlsMaterial::load(
            &SystemSecureFileSystem,
            source,
            Arc::new(SystemUnixClock),
            COMPONENT,
        )?;
        #[cfg(feature = "wss-compat")]
        let acceptor = TlsAcceptor::from(Arc::new(material.server_config()?));
        Ok(Self {
            material,
            #[cfg(feature = "wss-compat")]
            acceptor,
        })
    }

    #[cfg(feature = "wss-compat")]
    pub(crate) fn acceptor(&self) -> TlsAcceptor {
        self.acceptor.clone()
    }

    /// The rustls server configuration for the QUIC endpoint; the caller
    /// sets ALPN.
    pub(crate) fn rustls_server_config_for_quic(
        &self,
    ) -> Result<rustls::ServerConfig, HostTlsError> {
        self.material.server_config()
    }

    /// Stable direct-session Host identity derived only from the validated TLS SPKI.
    pub(crate) fn host_identity(&self) -> Result<arcen_identity::HostIdentity, HostTlsError> {
        self.material.host_identity()
    }

    pub(crate) fn emit_startup(&self, emitter: &crate::LifecycleEmitter) {
        emit(emitter, self.material.startup_event());
    }

    pub(crate) fn reload(&self, emitter: &crate::LifecycleEmitter) -> Result<(), HostTlsError> {
        let outcome = self.material.reload(&SystemSecureFileSystem);
        emit(emitter, outcome.events);
        outcome.result
    }

    pub(crate) fn check_and_emit_status(&self, emitter: &crate::LifecycleEmitter) {
        emit(emitter, self.material.status_events());
    }
}

fn emit(emitter: &crate::LifecycleEmitter, events: impl IntoIterator<Item = CertificateEvent>) {
    for event in events {
        crate::emit_lifecycle_event(emitter, event.kind, random_correlation_id(), event.fields);
    }
}

/// Produces a fresh non-secret correlation id for a standalone lifecycle event.
/// On Windows delegates to `eventlog`; on other platforms uses `getrandom` directly.
fn random_correlation_id() -> arcen_telemetry::CorrelationId {
    #[cfg(windows)]
    {
        crate::eventlog::random_correlation_id()
    }
    #[cfg(not(windows))]
    {
        let mut bytes = [0u8; 16];
        let _ = getrandom::getrandom(&mut bytes);
        arcen_telemetry::CorrelationId::from_uuid_v4_bytes(bytes)
    }
}

#[cfg(windows)]
struct SystemSecureFileSystem;

#[cfg(windows)]
impl SecureFileSystem for SystemSecureFileSystem {
    type Handle = std::fs::File;
    type Snapshot = WindowsFileFacts;

    fn open(&self, path: &Path, kind: MaterialKind) -> Result<Self::Handle, HostTlsError> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
        };

        let absolute = absolute_without_traversal(path, kind)?;
        let components = absolute.components().collect::<Vec<_>>();
        let mut current = PathBuf::new();
        let mut parent_handles = Vec::new();
        for (index, component) in components.iter().enumerate() {
            current.push(component.as_os_str());
            if index + 1 == components.len() || !matches!(component, Component::Normal(_)) {
                continue;
            }
            let directory = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ.0)
                .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
                .open(&current)
                .map_err(|error| map_open_error(error, kind))?;
            let snapshot = windows_snapshot(&directory, kind)?;
            if snapshot.reparse {
                return Err(HostTlsError::ReparsePoint(kind.label()));
            }
            if snapshot.regular || !snapshot.disk {
                return Err(HostTlsError::NotRegular(kind.label()));
            }
            // Keeping every checked directory open without FILE_SHARE_DELETE
            // prevents an attacker from replacing an ancestor before the leaf opens.
            parent_handles.push(directory);
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(absolute)
            .map_err(|error| map_open_error(error, kind))?;
        drop(parent_handles);
        Ok(file)
    }

    fn snapshot(
        &self,
        handle: &Self::Handle,
        kind: MaterialKind,
    ) -> Result<WindowsFileFacts, HostTlsError> {
        windows_snapshot(handle, kind)
    }

    fn admit(
        &self,
        handle: &Self::Handle,
        snapshot: &WindowsFileFacts,
        kind: MaterialKind,
    ) -> Result<(), HostTlsError> {
        snapshot.admit(kind)?;
        if kind == MaterialKind::PrivateKey && !validate_restricted_key_acl(handle)? {
            return Err(HostTlsError::BroadPrivateKeyAcl);
        }
        Ok(())
    }
}

#[cfg(not(windows))]
struct SystemSecureFileSystem;

#[cfg(not(windows))]
impl SecureFileSystem for SystemSecureFileSystem {
    type Handle = std::fs::File;
    type Snapshot = WindowsFileFacts;

    fn open(&self, path: &Path, kind: MaterialKind) -> Result<Self::Handle, HostTlsError> {
        std::fs::File::open(path).map_err(|error| map_open_error(error, kind))
    }

    fn snapshot(
        &self,
        handle: &Self::Handle,
        kind: MaterialKind,
    ) -> Result<WindowsFileFacts, HostTlsError> {
        let metadata = handle
            .metadata()
            .map_err(|_| HostTlsError::Read(kind.label()))?;
        let modified = metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |value| value.as_nanos() as u64);
        Ok(WindowsFileFacts {
            identity: 0,
            length: metadata.len(),
            modified,
            regular: metadata.is_file(),
            reparse: false,
            disk: true,
        })
    }

    fn admit(
        &self,
        _handle: &Self::Handle,
        snapshot: &WindowsFileFacts,
        kind: MaterialKind,
    ) -> Result<(), HostTlsError> {
        snapshot.admit(kind)?;
        if kind == MaterialKind::PrivateKey {
            return Err(HostTlsError::AclQueryFailed);
        }
        Ok(())
    }
}

fn map_open_error(error: std::io::Error, kind: MaterialKind) -> HostTlsError {
    if error.kind() == std::io::ErrorKind::NotFound {
        HostTlsError::Missing(kind.label())
    } else {
        HostTlsError::Open(kind.label())
    }
}

#[cfg(windows)]
fn absolute_without_traversal(path: &Path, kind: MaterialKind) -> Result<PathBuf, HostTlsError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| HostTlsError::Open(kind.label()))?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) if matches!(prefix.kind(), Prefix::Disk(_)) => {
                normalized.push(component.as_os_str());
            }
            Component::Prefix(_) => {
                return Err(HostTlsError::NonDiskDevice(kind.label()));
            }
            Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(HostTlsError::ReparsePoint(kind.label()));
                }
            }
        }
    }
    Ok(normalized)
}

#[cfg(windows)]
fn windows_snapshot(
    file: &std::fs::File,
    kind: MaterialKind,
) -> Result<WindowsFileFacts, HostTlsError> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, GetFileType, BY_HANDLE_FILE_INFORMATION,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_TYPE_DISK,
    };

    let handle = HANDLE(file.as_raw_handle());
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `handle` is borrowed from a live `File`; `information` is valid
    // writable storage of the exact structure expected by the API.
    unsafe { GetFileInformationByHandle(handle, &raw mut information) }
        .map_err(|_| HostTlsError::Read(kind.label()))?;
    // SAFETY: `handle` remains live and GetFileType does not take ownership.
    let disk = unsafe { GetFileType(handle) } == FILE_TYPE_DISK;
    let identity = (u128::from(information.dwVolumeSerialNumber) << 64)
        | (u128::from(information.nFileIndexHigh) << 32)
        | u128::from(information.nFileIndexLow);
    let length = (u64::from(information.nFileSizeHigh) << 32) | u64::from(information.nFileSizeLow);
    let modified = (u64::from(information.ftLastWriteTime.dwHighDateTime) << 32)
        | u64::from(information.ftLastWriteTime.dwLowDateTime);
    Ok(WindowsFileFacts {
        identity,
        length,
        modified,
        regular: information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0,
        reparse: information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0,
        disk,
    })
}

#[cfg(windows)]
fn validate_restricted_key_acl(file: &std::fs::File) -> Result<bool, HostTlsError> {
    use std::mem::{offset_of, size_of};
    use std::os::windows::io::AsRawHandle;
    use std::ptr::{addr_of, null_mut};
    use windows::Win32::Foundation::{LocalFree, HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows::Win32::Security::{
        CreateWellKnownSid, EqualSid, GetAce, GetSecurityDescriptorControl, IsValidSid,
        WinBuiltinAdministratorsSid, WinLocalSystemSid, ACCESS_ALLOWED_ACE, ACE_HEADER,
        DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        SECURITY_MAX_SID_SIZE, SE_DACL_PROTECTED,
    };
    use windows::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

    struct Descriptor(PSECURITY_DESCRIPTOR);
    impl Drop for Descriptor {
        fn drop(&mut self) {
            if !self.0 .0.is_null() {
                // SAFETY: GetSecurityInfo allocated this descriptor with LocalAlloc.
                let _ = unsafe { LocalFree(HLOCAL(self.0 .0)) };
            }
        }
    }

    let handle = HANDLE(file.as_raw_handle());
    let mut dacl = null_mut();
    let mut owner = PSID::default();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: all requested out-pointers refer to live writable storage and the
    // borrowed file handle remains valid throughout the query.
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&raw mut owner),
            None,
            Some(&raw mut dacl),
            None,
            Some(&raw mut descriptor),
        )
    };
    if status.0 != 0 || descriptor.0.is_null() {
        return Err(HostTlsError::AclQueryFailed);
    }
    let descriptor = Descriptor(descriptor);
    let mut control = 0_u16;
    let mut revision = 0_u32;
    // SAFETY: the descriptor remains live and both output pointers are valid.
    unsafe { GetSecurityDescriptorControl(descriptor.0, &raw mut control, &raw mut revision) }
        .map_err(|_| HostTlsError::AclQueryFailed)?;
    if control & SE_DACL_PROTECTED.0 == 0 || dacl.is_null() {
        return Ok(false);
    }
    // SAFETY: `dacl` aliases the live descriptor.
    let header = unsafe { &*dacl };
    if header.AceCount != 2 || usize::from(header.AclSize) < size_of_val(header) {
        return Ok(false);
    }

    fn known_sid(
        kind: windows::Win32::Security::WELL_KNOWN_SID_TYPE,
    ) -> Result<Vec<u8>, HostTlsError> {
        let mut bytes = vec![0_u8; SECURITY_MAX_SID_SIZE as usize];
        let mut length = bytes.len() as u32;
        // SAFETY: the buffer and size pointer are valid and large enough for a
        // well-known SID; no domain SID is needed for these two identities.
        unsafe { CreateWellKnownSid(kind, None, PSID(bytes.as_mut_ptr().cast()), &raw mut length) }
            .map_err(|_| HostTlsError::AclQueryFailed)?;
        bytes.truncate(length as usize);
        Ok(bytes)
    }

    let system = known_sid(WinLocalSystemSid)?;
    let administrators = known_sid(WinBuiltinAdministratorsSid)?;
    if owner.0.is_null() {
        return Ok(false);
    }
    let system_sid = PSID(system.as_ptr().cast_mut().cast());
    let admin_sid = PSID(administrators.as_ptr().cast_mut().cast());
    // SAFETY: the owner aliases the live descriptor, while both known SID
    // buffers remain live and were produced by CreateWellKnownSid.
    if !unsafe { IsValidSid(owner) }.as_bool()
        || !(unsafe { EqualSid(owner, system_sid) }.is_ok()
            || unsafe { EqualSid(owner, admin_sid) }.is_ok())
    {
        return Ok(false);
    }
    let mut saw_system = false;
    let mut saw_administrators = false;
    for index in 0..2 {
        let mut raw_ace = null_mut();
        // SAFETY: AceCount proves both queried indexes are in range.
        unsafe { GetAce(dacl, index, &raw mut raw_ace) }
            .map_err(|_| HostTlsError::AclQueryFailed)?;
        if raw_ace.is_null() {
            return Ok(false);
        }
        // SAFETY: GetAce returns a pointer to an ACE beginning with ACE_HEADER.
        let ace_header = unsafe { &*raw_ace.cast::<ACE_HEADER>() };
        if ace_header.AceType != 0
            || usize::from(ace_header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>()
        {
            return Ok(false);
        }
        // SAFETY: the checked type and size establish ACCESS_ALLOWED_ACE layout.
        let ace = unsafe { &*raw_ace.cast::<ACCESS_ALLOWED_ACE>() };
        if ace.Mask != FILE_ALL_ACCESS.0 {
            return Ok(false);
        }
        let sid_available = usize::from(ace_header.AceSize)
            .saturating_sub(offset_of!(ACCESS_ALLOWED_ACE, SidStart));
        let sid = PSID(addr_of!(ace.SidStart).cast_mut().cast());
        if sid_available < 8 {
            return Ok(false);
        }
        // SAFETY: the minimum SID header fits inside the checked ACE.
        if !unsafe { IsValidSid(sid) }.as_bool() {
            return Ok(false);
        }
        // SAFETY: all three validated SID buffers remain live for comparison.
        if unsafe { EqualSid(sid, system_sid) }.is_ok() {
            saw_system = true;
        } else if unsafe { EqualSid(sid, admin_sid) }.is_ok() {
            saw_administrators = true;
        } else {
            return Ok(false);
        }
    }
    Ok(saw_system && saw_administrators)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn relative_parent_components_are_lexically_normalized() {
        let current = std::env::current_dir().expect("current directory");
        let expected = current.join("tls").join("host.crt");
        let normalized = absolute_without_traversal(
            Path::new(r"fixtures\..\tls\host.crt"),
            MaterialKind::Certificate,
        )
        .expect("ordinary parent component");
        assert_eq!(normalized, expected);
    }

    #[cfg(windows)]
    #[test]
    fn unc_and_device_namespace_paths_are_rejected() {
        for path in [
            Path::new(r"\\server\share\host.crt"),
            Path::new(r"\\?\C:\tls\host.crt"),
            Path::new(r"\\.\C:\tls\host.crt"),
        ] {
            assert_eq!(
                absolute_without_traversal(path, MaterialKind::Certificate),
                Err(HostTlsError::NonDiskDevice("certificate"))
            );
        }
    }

    /// Off Windows the key's DACL cannot be checked, so a build there refuses
    /// the key rather than serving one nobody inspected.
    #[cfg(not(windows))]
    #[test]
    fn a_key_whose_dacl_cannot_be_read_is_refused() {
        let facts = WindowsFileFacts {
            identity: 0,
            length: 1,
            modified: 0,
            regular: true,
            reparse: false,
            disk: true,
        };
        let file = std::fs::File::open(std::env::current_exe().expect("exe")).expect("file");
        assert_eq!(
            SystemSecureFileSystem.admit(&file, &facts, MaterialKind::PrivateKey),
            Err(HostTlsError::AclQueryFailed)
        );
        assert_eq!(
            SystemSecureFileSystem.admit(&file, &facts, MaterialKind::Certificate),
            Ok(())
        );
    }
}
