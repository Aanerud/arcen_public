//! Reading a service's state from `sc.exe query`.
//!
//! Kept out of `#[cfg(windows)]`, like `acl` and `diagnosis`: whether
//! uninstall waits for the Pier to stop decides whether it deletes a binary
//! that is still running, so the parsing stays unit-testable on every host.

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
    fn flags_are_not_states() {
        assert_eq!(
            parse_state("(NOT_STOPPABLE, NOT_PAUSABLE, IGNORES_SHUTDOWN)"),
            None
        );
    }
}
