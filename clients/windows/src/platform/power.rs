// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Windows equivalent of the macOS Deck disabling App Nap: a Deck in
//! the background keeps receiving, decoding and playing at full rate, so
//! the process opts out of `EcoQoS` execution-speed throttling and timer
//! coalescing (`SetProcessInformation(ProcessPowerThrottling)`, PARITY
//! SH-07). Display sleep is deliberately not prevented (PARITY Q-14).

use windows::Win32::System::Threading::{
    GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
    PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling, SetProcessInformation,
};

/// Opts this process out of background power throttling. Returns whether
/// Windows accepted it.
#[must_use]
pub fn disable_background_throttling() -> bool {
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
            | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        // Controlled and not set: never throttle.
        StateMask: 0,
    };
    // SAFETY: the pseudo-handle of this process and a live structure of the
    // size passed.
    unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            std::ptr::from_ref(&state).cast(),
            u32::try_from(std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>()).unwrap_or(0),
        )
    }
    .is_ok()
}
