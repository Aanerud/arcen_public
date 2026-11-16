// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! `arcen-deck.exe`.
//!
//! A windows-subsystem program, so opening the Deck opens no console
//! window; its command-line output reaches the console it was started
//! from (see `arcen_deck_windows::attach_parent_console`).

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if !args.is_empty() {
        arcen_deck_windows::attach_parent_console();
    }
    arcen_deck_windows::run(&args)
}

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    eprintln!(
        "arcen-deck-windows {} runs on Windows only",
        arcen_deck_windows::VERSION
    );
    std::process::ExitCode::FAILURE
}
