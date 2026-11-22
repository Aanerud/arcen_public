// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Whether Windows asks for reduced motion: **Animation effects** off
//! (Settings › Accessibility › Visual effects), read with
//! `SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION)`. The shell hands it
//! to the shared screens each pass
//! (`arcen_deck_core::screens::theme::set_reduce_motion`), the macOS Deck's
//! `accessibilityDisplayShouldReduceMotion` counterpart.

use windows::Win32::Foundation::BOOL;
use windows::Win32::UI::WindowsAndMessaging::{
    SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
};

/// Whether Windows has animations turned off. An unreadable setting reads
/// as animations on.
#[must_use]
pub fn reduce_motion() -> bool {
    let mut animate = BOOL(1);
    // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL through the
    // pointer, which points at a live local of that type.
    let read = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some(std::ptr::from_mut(&mut animate).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    read.is_ok() && !animate.as_bool()
}
