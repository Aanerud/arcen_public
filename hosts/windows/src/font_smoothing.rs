//! Opt-in grey-scale text anti-aliasing for the length of a session.
//!
//! ClearType draws text with per-subpixel colour fringes tuned for the host's
//! own LCD. Captured and shown on a Deck -- scaled, colour-converted and, on
//! Auto/Speed, chroma-subsampled -- those fringes arrive as coloured edges on
//! every glyph. `platform.font_smoothing = "grayscale"` switches the session's
//! smoothing from ClearType to standard (grey-scale) while the agent serves it
//! and puts it back when the agent ends.
//!
//! The change is made with `SPI_SETFONTSMOOTHINGTYPE` and `SPIF_SENDCHANGE`
//! only, never `SPIF_UPDATEINIFILE`: the user's stored preference in
//! `HKCU\Control Panel\Desktop` is never written.
//!
//! An agent can be killed without unwinding (`TerminateJobObject`, job
//! kill-on-close), so restoring in `Drop` is not enough. Before switching, the
//! agent records a marker in a *volatile* per-user registry key (gone at
//! logoff, exactly like the in-memory setting it describes), keyed by the
//! Windows session id. Every agent start -- whatever the current policy --
//! first repairs: a marker for this session, a session rendering grey-scale
//! and a stored preference of ClearType mean Arcen's change outlived the agent
//! that made it, so ClearType comes back and the marker goes. Without a marker
//! the agent never touches a grey-scale session: that is the user's own choice.

use serde::{Deserialize, Serialize};

/// Operator policy, `platform.font_smoothing` in `pier.json`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FontSmoothingPolicy {
    /// Leave the user's text rendering alone (the default).
    #[default]
    Unchanged,
    /// Serve ClearType sessions with grey-scale anti-aliasing instead.
    Grayscale,
}

/// Session text smoothing as `SystemParametersInfo` describes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SmoothingState {
    pub enabled: bool,
    /// `FE_FONTSMOOTHINGSTANDARD` (1) or `FE_FONTSMOOTHINGCLEARTYPE` (2).
    pub kind: u32,
}

pub(crate) const STANDARD: u32 = 1;
pub(crate) const CLEARTYPE: u32 = 2;

/// Reads and applies the session's smoothing, and keeps this session's
/// restore marker.
pub(crate) trait SmoothingBackend {
    /// What the session renders with right now.
    fn current(&self) -> Result<SmoothingState, String>;
    /// The user's stored preference, when it can be read.
    fn stored(&self) -> Result<Option<SmoothingState>, String>;
    /// Sets the session's smoothing type for running applications only.
    fn apply_kind(&self, kind: u32) -> Result<(), String>;
    /// The smoothing type Arcen promised to restore in this session, if any.
    fn marker(&self) -> Result<Option<u32>, String>;
    /// Records that Arcen changed this session and what to restore.
    fn set_marker(&self, restore_kind: u32) -> Result<(), String>;
    /// Removes this session's marker.
    fn clear_marker(&self) -> Result<(), String>;
}

/// Pure decision for the start-of-agent repair: restore only what Arcen
/// itself changed and the user has not changed since.
pub(crate) fn repair_kind(
    marker: Option<u32>,
    current: SmoothingState,
    stored: Option<SmoothingState>,
) -> Option<u32> {
    let restore_kind = marker?;
    let still_ours = current.enabled && current.kind == STANDARD && current.kind != restore_kind;
    let user_wants_it = stored.is_none_or(|stored| stored.enabled && stored.kind == restore_kind);
    (still_ours && user_wants_it).then_some(restore_kind)
}

/// What a lease decided at session start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LeasePlan {
    /// Nothing to do: the policy is `unchanged`, smoothing is off, or the
    /// session already renders grey-scale and the user wants that too.
    Nothing,
    /// Switch to grey-scale now; restore `restore_kind` at the end.
    Apply { restore_kind: u32 },
}

/// Pure decision: grey-scale only replaces ClearType that is on now, and
/// never turns smoothing on for a user who has it off. A grey-scale an
/// earlier agent left behind has already been repaired by then.
pub(crate) fn plan(policy: FontSmoothingPolicy, current: SmoothingState) -> LeasePlan {
    if policy == FontSmoothingPolicy::Unchanged || !current.enabled || current.kind != CLEARTYPE {
        return LeasePlan::Nothing;
    }
    LeasePlan::Apply {
        restore_kind: CLEARTYPE,
    }
}

/// Holds grey-scale smoothing for the session; dropping it restores.
pub(crate) struct FontSmoothingLease<B: SmoothingBackend> {
    backend: B,
    restore_kind: Option<u32>,
}

impl<B: SmoothingBackend> FontSmoothingLease<B> {
    /// Repairs a change left behind by an earlier agent, then applies
    /// `policy` for this session. Failures are logged and leave the session
    /// as it was; text rendering is never worth refusing a session.
    pub(crate) fn acquire(policy: FontSmoothingPolicy, backend: B) -> Self {
        let mut lease = Self {
            backend,
            restore_kind: None,
        };
        lease.repair();
        if policy == FontSmoothingPolicy::Unchanged {
            return lease;
        }
        let current = match lease.backend.current() {
            Ok(current) => current,
            Err(error) => {
                tracing::warn!(target: crate::logging::SESSION, %error, "font smoothing: could not read the session's text smoothing; leaving it unchanged");
                return lease;
            }
        };
        match plan(policy, current) {
            LeasePlan::Nothing => {
                tracing::info!(
                    target: crate::logging::SESSION,
                    ?current,
                    "font smoothing: grey-scale requested; the session is not using ClearType, nothing to change"
                );
            }
            LeasePlan::Apply { restore_kind } => {
                // The marker goes first: a kill between the two calls must
                // still leave the next agent able to repair.
                if let Err(error) = lease.backend.set_marker(restore_kind) {
                    tracing::warn!(target: crate::logging::SESSION, %error, "font smoothing: could not record the restore marker; leaving ClearType");
                    return lease;
                }
                match lease.backend.apply_kind(STANDARD) {
                    Ok(()) => {
                        lease.restore_kind = Some(restore_kind);
                        tracing::info!(
                            target: crate::logging::SESSION,
                            "font smoothing: ClearType replaced by grey-scale for this session"
                        );
                    }
                    Err(error) => {
                        let _ = lease.backend.clear_marker();
                        tracing::warn!(target: crate::logging::SESSION, %error, "font smoothing: could not switch to grey-scale; leaving ClearType");
                    }
                }
            }
        }
        lease
    }

    /// Undoes a grey-scale an earlier, killed agent left in this session.
    fn repair(&self) {
        let marker = match self.backend.marker() {
            Ok(Some(marker)) => marker,
            Ok(None) => return,
            Err(error) => {
                tracing::debug!(target: crate::logging::SESSION, %error, "font smoothing: restore marker unreadable");
                return;
            }
        };
        let current = match self.backend.current() {
            Ok(current) => current,
            Err(error) => {
                tracing::warn!(target: crate::logging::SESSION, %error, "font smoothing: could not read the session to repair it");
                return;
            }
        };
        let stored = self.backend.stored().ok().flatten();
        match repair_kind(Some(marker), current, stored) {
            Some(kind) => match self.backend.apply_kind(kind) {
                Ok(()) => {
                    let _ = self.backend.clear_marker();
                    tracing::info!(
                        target: crate::logging::SESSION,
                        kind,
                        "font smoothing: restored text smoothing an earlier agent left changed"
                    );
                }
                Err(error) => {
                    tracing::warn!(target: crate::logging::SESSION, %error, "font smoothing: repair failed; keeping the marker for the next agent");
                }
            },
            None => {
                // The user changed it since, or it is already back: the
                // promise is void either way.
                let _ = self.backend.clear_marker();
            }
        }
    }
}

impl<B: SmoothingBackend> Drop for FontSmoothingLease<B> {
    fn drop(&mut self) {
        let Some(kind) = self.restore_kind.take() else {
            return;
        };
        match self.backend.apply_kind(kind) {
            Ok(()) => {
                let _ = self.backend.clear_marker();
                tracing::info!(
                    target: crate::logging::SESSION,
                    kind,
                    "font smoothing: restored the user's text smoothing"
                );
            }
            Err(error) => tracing::warn!(
                target: crate::logging::SESSION,
                %error,
                "font smoothing: restore failed; the next agent or logoff restores it"
            ),
        }
    }
}

/// The live session's smoothing through `SystemParametersInfoW`.
#[cfg(windows)]
pub(crate) struct WindowsSmoothing;

/// `HKCU\Software\ArcenVolatile`, created volatile: it holds no state past
/// logoff, matching the in-memory setting its values describe.
#[cfg(windows)]
const MARKER_KEY: windows::core::PCWSTR = windows::core::w!("Software\\ArcenVolatile");

/// `FontSmoothingRestore.<session id>`: one user may hold several Windows
/// sessions, and each repairs only its own.
#[cfg(windows)]
fn marker_value_name() -> Result<Vec<u16>, String> {
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::Threading::GetCurrentProcessId;
    let mut session = 0_u32;
    // SAFETY: the out-pointer is a live local u32.
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) }
        .map_err(|error| format!("ProcessIdToSessionId: {error}"))?;
    Ok(marker_value_name_for(session))
}

/// The marker value name for a Windows session, NUL-terminated.
pub(crate) fn marker_value_name_for(session: u32) -> Vec<u16> {
    format!("FontSmoothingRestore.{session}")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(windows)]
impl SmoothingBackend for WindowsSmoothing {
    fn current(&self) -> Result<SmoothingState, String> {
        use windows::Win32::Foundation::BOOL;
        use windows::Win32::UI::WindowsAndMessaging::{
            SystemParametersInfoW, SPI_GETFONTSMOOTHING, SPI_GETFONTSMOOTHINGTYPE,
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
        };
        let mut enabled = BOOL::default();
        let mut kind = 0_u32;
        // SAFETY: both out-pointers are live locals of the documented types.
        unsafe {
            SystemParametersInfoW(
                SPI_GETFONTSMOOTHING,
                0,
                Some(std::ptr::from_mut(&mut enabled).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
            .map_err(|error| format!("SPI_GETFONTSMOOTHING: {error}"))?;
            SystemParametersInfoW(
                SPI_GETFONTSMOOTHINGTYPE,
                0,
                Some(std::ptr::from_mut(&mut kind).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
            .map_err(|error| format!("SPI_GETFONTSMOOTHINGTYPE: {error}"))?;
        }
        Ok(SmoothingState {
            enabled: enabled.as_bool(),
            kind,
        })
    }

    fn stored(&self) -> Result<Option<SmoothingState>, String> {
        use windows::core::w;
        use windows::Win32::System::Registry::{
            RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
        };
        let mut text = [0_u16; 8];
        let mut text_bytes = u32::try_from(std::mem::size_of_val(&text)).unwrap_or(16);
        // SAFETY: the buffer and its byte length describe a live local array.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                w!("Control Panel\\Desktop"),
                w!("FontSmoothing"),
                RRF_RT_REG_SZ,
                None,
                Some(text.as_mut_ptr().cast()),
                Some(&mut text_bytes),
            )
        };
        if status.is_err() {
            return Ok(None);
        }
        let enabled = text[0] != 0 && text[0] != u16::from(b'0');
        let mut kind = 0_u32;
        let mut kind_bytes = 4_u32;
        // SAFETY: the out-pointer and its byte length describe a live u32.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                w!("Control Panel\\Desktop"),
                w!("FontSmoothingType"),
                RRF_RT_REG_DWORD,
                None,
                Some(std::ptr::from_mut(&mut kind).cast()),
                Some(&mut kind_bytes),
            )
        };
        if status.is_err() {
            return Ok(None);
        }
        Ok(Some(SmoothingState { enabled, kind }))
    }

    fn marker(&self) -> Result<Option<u32>, String> {
        use windows::core::PCWSTR;
        use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
        let name = marker_value_name()?;
        let mut kind = 0_u32;
        let mut bytes = 4_u32;
        // SAFETY: the names are NUL-terminated wide strings that outlive the
        // call; the out-pointer and its byte length describe a live u32.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                MARKER_KEY,
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_DWORD,
                None,
                Some(std::ptr::from_mut(&mut kind).cast()),
                Some(&mut bytes),
            )
        };
        Ok(status.is_ok().then_some(kind))
    }

    fn set_marker(&self, restore_kind: u32) -> Result<(), String> {
        use windows::core::PCWSTR;
        use windows::Win32::System::Registry::{
            RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
            REG_DWORD, REG_OPTION_VOLATILE,
        };
        let name = marker_value_name()?;
        let mut key = HKEY::default();
        // SAFETY: the subkey is a NUL-terminated wide string and `key` is a
        // live out-parameter. REG_OPTION_VOLATILE keeps the key in memory
        // only: it vanishes when the user's hive unloads at logoff.
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                MARKER_KEY,
                0,
                PCWSTR::null(),
                REG_OPTION_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut key,
                None,
            )
        };
        status
            .ok()
            .map_err(|error| format!("create volatile marker key: {error}"))?;
        // SAFETY: `key` is open with KEY_SET_VALUE and the data slice is live.
        let status = unsafe {
            RegSetValueExW(
                key,
                PCWSTR(name.as_ptr()),
                0,
                REG_DWORD,
                Some(&restore_kind.to_le_bytes()),
            )
        };
        // SAFETY: `key` was opened above and is closed exactly once.
        unsafe {
            let _ = RegCloseKey(key);
        }
        status
            .ok()
            .map_err(|error| format!("write marker value: {error}"))
    }

    fn clear_marker(&self) -> Result<(), String> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
        use windows::Win32::System::Registry::{RegDeleteKeyValueW, HKEY_CURRENT_USER};
        let name = marker_value_name()?;
        // SAFETY: both names are NUL-terminated wide strings that outlive the call.
        let status =
            unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, MARKER_KEY, PCWSTR(name.as_ptr())) };
        if status.is_ok() || status == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(format!("delete marker value: {status:?}"))
        }
    }

    fn apply_kind(&self, kind: u32) -> Result<(), String> {
        use windows::Win32::UI::WindowsAndMessaging::{
            SystemParametersInfoW, SPIF_SENDCHANGE, SPI_SETFONTSMOOTHINGTYPE,
        };
        // SAFETY: SPI_SETFONTSMOOTHINGTYPE takes the type in pvParam by value.
        // SPIF_SENDCHANGE without SPIF_UPDATEINIFILE: running applications
        // re-render, and the user's stored preference is untouched.
        unsafe {
            SystemParametersInfoW(
                SPI_SETFONTSMOOTHINGTYPE,
                0,
                Some(kind as usize as *mut core::ffi::c_void),
                SPIF_SENDCHANGE,
            )
        }
        .map_err(|error| format!("SPI_SETFONTSMOOTHINGTYPE({kind}): {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// One Windows session: in-memory smoothing, the stored preference, the
    /// volatile marker, and every `apply_kind` call in order.
    struct Session {
        current: RefCell<SmoothingState>,
        stored: RefCell<Option<SmoothingState>>,
        marker: RefCell<Option<u32>>,
        applied: RefCell<Vec<u32>>,
        fail_apply: bool,
        fail_marker: bool,
    }

    impl Session {
        fn new(current: SmoothingState, stored: Option<SmoothingState>) -> Self {
            Self {
                current: RefCell::new(current),
                stored: RefCell::new(stored),
                marker: RefCell::new(None),
                applied: RefCell::new(Vec::new()),
                fail_apply: false,
                fail_marker: false,
            }
        }

        fn kind(&self) -> u32 {
            self.current.borrow().kind
        }
    }

    impl SmoothingBackend for &Session {
        fn current(&self) -> Result<SmoothingState, String> {
            Ok(*self.current.borrow())
        }
        fn stored(&self) -> Result<Option<SmoothingState>, String> {
            Ok(*self.stored.borrow())
        }
        fn apply_kind(&self, kind: u32) -> Result<(), String> {
            if self.fail_apply {
                return Err("denied".into());
            }
            self.current.borrow_mut().kind = kind;
            self.applied.borrow_mut().push(kind);
            Ok(())
        }
        fn marker(&self) -> Result<Option<u32>, String> {
            Ok(*self.marker.borrow())
        }
        fn set_marker(&self, restore_kind: u32) -> Result<(), String> {
            if self.fail_marker {
                return Err("denied".into());
            }
            *self.marker.borrow_mut() = Some(restore_kind);
            Ok(())
        }
        fn clear_marker(&self) -> Result<(), String> {
            *self.marker.borrow_mut() = None;
            Ok(())
        }
    }

    const CLEAR: SmoothingState = SmoothingState {
        enabled: true,
        kind: CLEARTYPE,
    };
    const GREY: SmoothingState = SmoothingState {
        enabled: true,
        kind: STANDARD,
    };
    const OFF: SmoothingState = SmoothingState {
        enabled: false,
        kind: CLEARTYPE,
    };

    /// An agent killed without unwinding: `TerminateJobObject` never runs Drop.
    fn kill(lease: FontSmoothingLease<&Session>) {
        std::mem::forget(lease);
    }

    #[test]
    fn the_policy_parses_from_pier_json_and_defaults_to_unchanged() {
        assert_eq!(
            FontSmoothingPolicy::default(),
            FontSmoothingPolicy::Unchanged
        );
        let parsed: FontSmoothingPolicy = serde_json::from_str("\"grayscale\"").unwrap();
        assert_eq!(parsed, FontSmoothingPolicy::Grayscale);
        assert!(serde_json::from_str::<FontSmoothingPolicy>("\"cleartype\"").is_err());
    }

    #[test]
    fn grayscale_replaces_cleartype_for_the_session_and_restores_it() {
        let session = Session::new(CLEAR, Some(CLEAR));
        let lease = FontSmoothingLease::acquire(FontSmoothingPolicy::Grayscale, &session);
        assert_eq!(session.kind(), STANDARD);
        assert_eq!(
            *session.marker.borrow(),
            Some(CLEARTYPE),
            "marker held while changed"
        );
        drop(lease);
        assert_eq!(session.kind(), CLEARTYPE);
        assert_eq!(*session.marker.borrow(), None);
        assert_eq!(*session.applied.borrow(), vec![STANDARD, CLEARTYPE]);
    }

    #[test]
    fn unchanged_touches_nothing() {
        let session = Session::new(CLEAR, Some(CLEAR));
        drop(FontSmoothingLease::acquire(
            FontSmoothingPolicy::Unchanged,
            &session,
        ));
        assert!(session.applied.borrow().is_empty());
        assert_eq!(*session.marker.borrow(), None);
    }

    #[test]
    fn smoothing_that_is_off_or_already_grey_is_left_alone() {
        for (current, stored) in [
            (OFF, Some(OFF)),
            (GREY, Some(GREY)),
            (GREY, None),
            (GREY, Some(CLEAR)),
        ] {
            let session = Session::new(current, stored);
            drop(FontSmoothingLease::acquire(
                FontSmoothingPolicy::Grayscale,
                &session,
            ));
            assert!(
                session.applied.borrow().is_empty(),
                "{current:?} {stored:?}"
            );
            assert_eq!(*session.marker.borrow(), None);
        }
    }

    #[test]
    fn a_killed_agent_is_repaired_by_the_next_agent_with_the_same_policy() {
        let session = Session::new(CLEAR, Some(CLEAR));
        kill(FontSmoothingLease::acquire(
            FontSmoothingPolicy::Grayscale,
            &session,
        ));
        assert_eq!(session.kind(), STANDARD, "the kill left grey-scale behind");
        assert_eq!(*session.marker.borrow(), Some(CLEARTYPE));

        let lease = FontSmoothingLease::acquire(FontSmoothingPolicy::Grayscale, &session);
        assert_eq!(session.kind(), STANDARD);
        drop(lease);
        assert_eq!(session.kind(), CLEARTYPE);
        assert_eq!(*session.marker.borrow(), None);
        assert_eq!(
            *session.applied.borrow(),
            vec![STANDARD, CLEARTYPE, STANDARD, CLEARTYPE],
            "repair, then a fresh lease"
        );
    }

    #[test]
    fn a_killed_agent_is_repaired_even_after_the_policy_flips_to_unchanged() {
        let session = Session::new(CLEAR, Some(CLEAR));
        kill(FontSmoothingLease::acquire(
            FontSmoothingPolicy::Grayscale,
            &session,
        ));
        drop(FontSmoothingLease::acquire(
            FontSmoothingPolicy::Unchanged,
            &session,
        ));
        assert_eq!(
            session.kind(),
            CLEARTYPE,
            "the console user is not left on grey-scale"
        );
        assert_eq!(*session.marker.borrow(), None);
        assert_eq!(*session.applied.borrow(), vec![STANDARD, CLEARTYPE]);
    }

    #[test]
    fn a_change_the_user_made_since_is_kept_and_the_marker_dropped() {
        let session = Session::new(CLEAR, Some(CLEAR));
        kill(FontSmoothingLease::acquire(
            FontSmoothingPolicy::Grayscale,
            &session,
        ));
        // The user then chose standard smoothing in Control Panel.
        *session.stored.borrow_mut() = Some(GREY);
        drop(FontSmoothingLease::acquire(
            FontSmoothingPolicy::Unchanged,
            &session,
        ));
        assert_eq!(session.kind(), STANDARD);
        assert_eq!(*session.marker.borrow(), None);
        assert_eq!(*session.applied.borrow(), vec![STANDARD]);
    }

    #[test]
    fn without_a_marker_a_grey_session_is_never_touched() {
        // Grey in memory, ClearType stored, but Arcen did not do it.
        let session = Session::new(GREY, Some(CLEAR));
        drop(FontSmoothingLease::acquire(
            FontSmoothingPolicy::Unchanged,
            &session,
        ));
        assert!(session.applied.borrow().is_empty());
    }

    #[test]
    fn no_switch_happens_without_a_marker_or_after_a_failed_switch() {
        let mut session = Session::new(CLEAR, Some(CLEAR));
        session.fail_marker = true;
        drop(FontSmoothingLease::acquire(
            FontSmoothingPolicy::Grayscale,
            &session,
        ));
        assert!(session.applied.borrow().is_empty(), "no marker, no change");

        let mut session = Session::new(CLEAR, Some(CLEAR));
        session.fail_apply = true;
        drop(FontSmoothingLease::acquire(
            FontSmoothingPolicy::Grayscale,
            &session,
        ));
        assert_eq!(
            *session.marker.borrow(),
            None,
            "a failed switch leaves no promise"
        );
    }

    #[test]
    fn repair_restores_only_what_arcen_changed() {
        assert_eq!(
            repair_kind(Some(CLEARTYPE), GREY, Some(CLEAR)),
            Some(CLEARTYPE)
        );
        assert_eq!(repair_kind(Some(CLEARTYPE), GREY, None), Some(CLEARTYPE));
        assert_eq!(repair_kind(None, GREY, Some(CLEAR)), None);
        assert_eq!(
            repair_kind(Some(CLEARTYPE), CLEAR, Some(CLEAR)),
            None,
            "already back"
        );
        assert_eq!(
            repair_kind(Some(CLEARTYPE), OFF, Some(CLEAR)),
            None,
            "user turned it off"
        );
        assert_eq!(
            repair_kind(Some(CLEARTYPE), GREY, Some(GREY)),
            None,
            "user chose grey"
        );
    }

    #[test]
    fn marker_names_are_per_session() {
        let name = String::from_utf16(&marker_value_name_for(3)).unwrap();
        assert_eq!(name, "FontSmoothingRestore.3\0");
        assert_ne!(marker_value_name_for(3), marker_value_name_for(4));
    }
}
