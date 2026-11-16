// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! `arcen-usb-prepare.exe` (ADR 0024; `DECISION-MATRIX.md` §20): run elevated
//! once per tablet model ("Prepare this tablet…").
//!
//! Writes a signed `WinUSB` package for one tablet model into the component's
//! protected directory, and nothing else: it does not touch the driver store,
//! the trust stores or the tablet. The service stages the package only for
//! the length of a session.
//!
//! 1. the INF that binds Microsoft's in-box `WinUSB` to that hardware ID
//!    (shared `winusb_package::winusb_inf`);
//! 2. a per-install, per-model RSA-3072 key that exists **only in memory**
//!    (an unnamed CNG key is never persisted) and a self-signed certificate
//!    for code signing alone (not a CA);
//! 3. the catalog over the INF (in-box `wintrust` CDF functions, the same
//!    work `makecat` does), signed with that key (in-box `mssign32`);
//! 4. the key freed; only the public certificate is kept, for the service's
//!    transient trust.
//!
//! ```text
//! arcen-usb-prepare --instance 'USB\VID_056A&PID_0317\…'   prepare that tablet's model
//! arcen-usb-prepare --model 056A:0317                      prepare by IDs
//! arcen-usb-prepare --remove 056A:0317 | --remove-all       forget prepared models
//! arcen-usb-prepare --list
//! ```

#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    win::main()
}

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    eprintln!("arcen-usb-prepare runs on Windows only");
    std::process::ExitCode::FAILURE
}
