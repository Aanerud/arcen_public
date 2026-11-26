// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Reading a service's state from `sc.exe query`, and from the service
//! control manager itself while a start is watched.
//!
//! Kept out of `#[cfg(windows)]`, like `acl` and `diagnosis`: whether
//! uninstall waits for the Pier to stop decides whether it deletes a binary
//! that is still running, and whether install reports the Pier running
//! decides its exit status, so both translations stay unit-testable on every
//! host. Only [`native`] calls Windows.

use std::time::Duration;

use arcen_session::install_lifecycle::ServiceStatusReading;

/// A service control manager state, as `sc.exe query` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScmState {
    Stopped,
    StartPending,
    StopPending,
    Running,
    ContinuePending,
    PausePending,
    Paused,
}

impl ScmState {
    /// Whether the service is on its way somewhere and will not accept a stop
    /// request yet (`sc stop` fails with 1052 while it is starting).
    pub fn is_transitional(self) -> bool {
        matches!(
            self,
            Self::StartPending | Self::ContinuePending | Self::PausePending
        )
    }
}

/// The state in `sc.exe query` output, or `None` when there is none, for
/// example because the service does not exist.
///
/// The field labels are localised on some Windows installations, but the
/// state names are not, so this matches the names as whole words rather than
/// the `STATE` label. The flags line (`NOT_STOPPABLE`, ...) never matches.
pub fn parse_state(query: &str) -> Option<ScmState> {
    query
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .find_map(|word| match word {
            "STOPPED" => Some(ScmState::Stopped),
            "START_PENDING" => Some(ScmState::StartPending),
            "STOP_PENDING" => Some(ScmState::StopPending),
            "RUNNING" => Some(ScmState::Running),
            "CONTINUE_PENDING" => Some(ScmState::ContinuePending),
            "PAUSE_PENDING" => Some(ScmState::PausePending),
            "PAUSED" => Some(ScmState::Paused),
            _ => None,
        })
}

/// One `SERVICE_STATUS_PROCESS`, as the fields the installer reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct StatusProcess {
    /// `dwCurrentState`.
    pub current_state: u32,
    /// `dwCheckPoint`.
    pub checkpoint: u32,
    /// `dwWaitHint`, in milliseconds.
    pub wait_hint_ms: u32,
    /// `dwWin32ExitCode`.
    pub exit_code: u32,
    /// `dwServiceSpecificExitCode`.
    pub service_exit_code: u32,
    /// `dwProcessId`.
    pub process_id: u32,
}

/// The shared reading for one `SERVICE_STATUS_PROCESS`.
pub(crate) fn reading_from_status(status: StatusProcess) -> ServiceStatusReading {
    let StatusProcess {
        current_state,
        checkpoint,
        wait_hint_ms,
        exit_code,
        service_exit_code,
        process_id,
    } = status;
    let wait_hint = Duration::from_millis(u64::from(wait_hint_ms));
    match current_state {
        1 => ServiceStatusReading::Stopped {
            exit_code,
            service_exit_code,
        },
        2 => ServiceStatusReading::Starting {
            progress: checkpoint,
            wait_hint,
            process_id,
        },
        3 => ServiceStatusReading::Stopping {
            progress: checkpoint,
            wait_hint,
        },
        4 => ServiceStatusReading::Running { process_id },
        5 => ServiceStatusReading::Other("continue_pending"),
        6 => ServiceStatusReading::Other("pause_pending"),
        7 => ServiceStatusReading::Other("paused"),
        other => ServiceStatusReading::Unreadable(format!("unknown service state {other}")),
    }
}

/// The service control manager calls the installer makes while it starts
/// the service and watches it come up.
#[cfg(windows)]
#[allow(unsafe_code)]
pub(crate) mod native {
    use std::ffi::OsStr;
    use std::io;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt as _;
    use std::ptr::null;

    use arcen_session::install_lifecycle::ServiceStatusReading;
    use windows_sys::Win32::Foundation::ERROR_SERVICE_ALREADY_RUNNING;
    use windows_sys::Win32::System::Services::{
        ChangeServiceConfig2W, CloseServiceHandle, OpenSCManagerW, OpenServiceW,
        QueryServiceConfig2W, QueryServiceStatusEx, SC_HANDLE, SC_MANAGER_CONNECT,
        SC_STATUS_PROCESS_INFO, SERVICE_CHANGE_CONFIG, SERVICE_CONFIG_PRESHUTDOWN_INFO,
        SERVICE_PRESHUTDOWN_INFO, SERVICE_QUERY_CONFIG, SERVICE_QUERY_STATUS, SERVICE_START,
        SERVICE_STATUS_PROCESS, StartServiceW,
    };

    /// An open service control manager handle, closed on drop.
    #[derive(Debug)]
    struct ScHandle(SC_HANDLE);

    impl Drop for ScHandle {
        fn drop(&mut self) {
            // SAFETY: the handle came from OpenSCManagerW or OpenServiceW and
            // is closed once.
            let _ = unsafe { CloseServiceHandle(self.0) };
        }
    }

    /// A service opened to be started and queried.
    #[derive(Debug)]
    pub(crate) struct Service {
        service: ScHandle,
        _manager: ScHandle,
    }

    impl Service {
        /// Opens `name` for `SERVICE_START` and `SERVICE_QUERY_STATUS`.
        ///
        /// # Errors
        ///
        /// The Windows error when the manager or the service cannot be opened.
        pub(crate) fn open(name: &str) -> Result<Self, String> {
            Self::open_for(name, SERVICE_START | SERVICE_QUERY_STATUS)
        }

        fn open_for(name: &str, access: u32) -> Result<Self, String> {
            // SAFETY: null machine and database names select the local
            // active database.
            let manager = unsafe { OpenSCManagerW(null(), null(), SC_MANAGER_CONNECT) };
            if manager.is_null() {
                return Err(format!(
                    "open the service control manager: {}",
                    io::Error::last_os_error()
                ));
            }
            let manager = ScHandle(manager);
            let wide: Vec<u16> = OsStr::new(name).encode_wide().chain([0]).collect();
            // SAFETY: `wide` is NUL-terminated and outlives the call.
            let service = unsafe { OpenServiceW(manager.0, wide.as_ptr(), access) };
            if service.is_null() {
                return Err(format!(
                    "open service {name}: {}",
                    io::Error::last_os_error()
                ));
            }
            Ok(Self {
                service: ScHandle(service),
                _manager: manager,
            })
        }

        /// Asks the manager to start the service; one already running is
        /// not an error.
        ///
        /// # Errors
        ///
        /// The Windows error the manager refused the start with.
        pub(crate) fn start(&self) -> Result<(), String> {
            // SAFETY: the handle was opened with SERVICE_START; no arguments.
            if unsafe { StartServiceW(self.service.0, 0, null()) } != 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() == i32::try_from(ERROR_SERVICE_ALREADY_RUNNING).ok() {
                Ok(())
            } else {
                Err(error.to_string())
            }
        }

        /// Sets `name`'s preshutdown timeout: how long Windows waits for the
        /// service after its shutdown notice before it moves on.
        ///
        /// # Errors
        ///
        /// The Windows error when the service cannot be opened or changed.
        pub(crate) fn set_preshutdown_timeout(name: &str, timeout_ms: u32) -> Result<(), String> {
            let service = Self::open_for(name, SERVICE_CHANGE_CONFIG)?;
            let info = SERVICE_PRESHUTDOWN_INFO {
                dwPreshutdownTimeout: timeout_ms,
            };
            // SAFETY: the handle was opened with SERVICE_CHANGE_CONFIG and
            // `info` is a live SERVICE_PRESHUTDOWN_INFO for the call.
            let ok = unsafe {
                ChangeServiceConfig2W(
                    service.service.0,
                    SERVICE_CONFIG_PRESHUTDOWN_INFO,
                    (&raw const info).cast(),
                )
            };
            if ok == 0 {
                return Err(format!(
                    "ChangeServiceConfig2W(preshutdown): {}",
                    io::Error::last_os_error()
                ));
            }
            Ok(())
        }

        /// `name`'s preshutdown timeout in milliseconds.
        ///
        /// # Errors
        ///
        /// The Windows error when the service cannot be opened or read.
        pub(crate) fn preshutdown_timeout(name: &str) -> Result<u32, String> {
            let service = Self::open_for(name, SERVICE_QUERY_CONFIG)?;
            // SAFETY: SERVICE_PRESHUTDOWN_INFO is plain old data.
            let mut info: SERVICE_PRESHUTDOWN_INFO = unsafe { std::mem::zeroed() };
            let mut needed = 0_u32;
            // SAFETY: the buffer is a live SERVICE_PRESHUTDOWN_INFO of the
            // size passed, and the handle was opened with
            // SERVICE_QUERY_CONFIG.
            let ok = unsafe {
                QueryServiceConfig2W(
                    service.service.0,
                    SERVICE_CONFIG_PRESHUTDOWN_INFO,
                    (&raw mut info).cast::<u8>(),
                    u32::try_from(size_of::<SERVICE_PRESHUTDOWN_INFO>()).unwrap_or(u32::MAX),
                    &raw mut needed,
                )
            };
            if ok == 0 {
                return Err(format!(
                    "QueryServiceConfig2W(preshutdown): {}",
                    io::Error::last_os_error()
                ));
            }
            Ok(info.dwPreshutdownTimeout)
        }

        /// The service's current status, or why it could not be read.
        pub(crate) fn query(&self) -> ServiceStatusReading {
            // SAFETY: SERVICE_STATUS_PROCESS is plain old data.
            let mut status: SERVICE_STATUS_PROCESS = unsafe { std::mem::zeroed() };
            let mut needed = 0_u32;
            // SAFETY: the buffer is a live SERVICE_STATUS_PROCESS of the size
            // passed, and the handle was opened with SERVICE_QUERY_STATUS.
            let ok = unsafe {
                QueryServiceStatusEx(
                    self.service.0,
                    SC_STATUS_PROCESS_INFO,
                    (&raw mut status).cast::<u8>(),
                    u32::try_from(size_of::<SERVICE_STATUS_PROCESS>()).unwrap_or(u32::MAX),
                    &raw mut needed,
                )
            };
            if ok == 0 {
                return ServiceStatusReading::Unreadable(format!(
                    "QueryServiceStatusEx: {}",
                    io::Error::last_os_error()
                ));
            }
            super::reading_from_status(super::StatusProcess {
                current_state: status.dwCurrentState,
                checkpoint: status.dwCheckPoint,
                wait_hint_ms: status.dwWaitHint,
                exit_code: status.dwWin32ExitCode,
                service_exit_code: status.dwServiceSpecificExitCode,
                process_id: status.dwProcessId,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const START_PENDING: &str = "\r\nSERVICE_NAME: ArcenPier \r\n        TYPE               : 10  WIN32_OWN_PROCESS  \r\n        STATE              : 2  START_PENDING \r\n                                (NOT_STOPPABLE, NOT_PAUSABLE, IGNORES_SHUTDOWN)\r\n        WIN32_EXIT_CODE    : 0  (0x0)\r\n";

    #[test]
    fn a_starting_service_is_not_mistaken_for_a_stopped_one() {
        // The old check looked only for RUNNING, so a service still starting
        // read as stopped and uninstall deleted a binary that was in use.
        assert_eq!(parse_state(START_PENDING), Some(ScmState::StartPending));
        assert!(ScmState::StartPending.is_transitional());
    }

    #[test]
    fn the_state_names_are_read_whatever_the_labels_say() {
        let norwegian = "TJENESTENAVN: ArcenPier\r\n        TILSTAND           : 4  RUNNING\r\n                                (STOPPABLE, NOT_PAUSABLE, ACCEPTS_SHUTDOWN)\r\n";
        assert_eq!(parse_state(norwegian), Some(ScmState::Running));
        assert_eq!(
            parse_state("        STATE              : 3  STOP_PENDING \r\n"),
            Some(ScmState::StopPending)
        );
        assert_eq!(
            parse_state("        STATE              : 1  STOPPED \r\n"),
            Some(ScmState::Stopped)
        );
    }

    #[test]
    fn no_state_means_no_service() {
        assert_eq!(
            parse_state(
                "[SC] EnumQueryServicesStatus:OpenService FAILED 1060:\r\n\r\nThe specified service does not exist as an installed service.\r\n"
            ),
            None
        );
        assert_eq!(parse_state(""), None);
    }

    #[test]
    fn the_managers_status_becomes_the_shared_reading() {
        let status = |current_state, checkpoint, wait_hint_ms| StatusProcess {
            current_state,
            checkpoint,
            wait_hint_ms,
            process_id: 812,
            ..StatusProcess::default()
        };
        assert_eq!(
            reading_from_status(status(2, 3, 10_000)),
            ServiceStatusReading::Starting {
                progress: 3,
                wait_hint: Duration::from_secs(10),
                process_id: 812,
            }
        );
        assert_eq!(
            reading_from_status(status(4, 0, 0)),
            ServiceStatusReading::Running { process_id: 812 }
        );
        // ERROR_SERVICE_SPECIFIC_ERROR with the Pier's own code.
        assert_eq!(
            reading_from_status(StatusProcess {
                current_state: 1,
                exit_code: 1066,
                service_exit_code: 6,
                ..StatusProcess::default()
            }),
            ServiceStatusReading::Stopped {
                exit_code: 1066,
                service_exit_code: 6,
            }
        );
        assert_eq!(
            reading_from_status(status(7, 0, 0)),
            ServiceStatusReading::Other("paused")
        );
        assert!(matches!(
            reading_from_status(status(42, 0, 0)),
            ServiceStatusReading::Unreadable(_)
        ));
    }

    #[test]
    fn flags_are_not_states() {
        assert_eq!(
            parse_state("(NOT_STOPPABLE, NOT_PAUSABLE, IGNORES_SHUTDOWN)"),
            None
        );
    }
}
