// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Arcen Deck for Windows: a thin Win32/D3D11 shell around
//! [`arcen_deck_core`].
//!
//! Everything portable lives in `arcen-deck-core` and the other shared
//! crates; this crate only does what Windows forces on it, and each module
//! is owned by one work stream:
//!
//! | Module | Owner | Implements |
//! | --- | --- | --- |
//! | [`decode`] | `decoder` | `VideoDecoderBackend` on `ID3D11VideoDecoder` (DXVA) |
//! | [`present`] | `present` | `VideoPresenterSink` on `DirectComposition` + flip-model swapchains, `RefineCompositor` |
//! | [`input`] | `input` | raw input mouse, keyboard grab, IME/TSF, `WM_POINTER` pen, cursors |
//! | [`audio`] | `platform` | WASAPI render and microphone (`DeckServices::start_microphone`) |
//! | [`clipboard`] | `platform` | `PlatformClipboard` on a clipboard-listener window |
//! | [`display`] | `platform` | monitors, Per-Monitor-V2 DPI, colour facts |
//! | [`platform`] | `platform` | `DeckServices`: folders, build identity, network, time zone, WER |
//! | [`media`] | `core` | one connection's media: the decoder, the sinks and the shared worker |
//! | [`show`] | `core` | `arcen-deck show`: connect and show a desktop in a plain window (lab) |
//! | [`logging`] | `platform` | log files and WER crash dumps (the support bundle's facts: `platform::support`) |
//! | [`shell`] | `ui` | the window: egui-winit + egui-wgpu over the video visual |
//!
//! Off Windows the crate compiles to nothing, so the workspace still checks
//! on a Mac; Windows code is type-checked there with
//! `cargo xwin check --target x86_64-pc-windows-msvc -p arcen-deck-windows`.

/// Release packaging scans `arcen-deck.exe` for this marker and refuses to
/// ship it, so a developer build can never become a release.
#[cfg(feature = "dev-tools")]
#[used]
pub static ARCEN_DECK_DEV_TOOLS_FEATURE_MARKER: &[u8] =
    b"ARCEN_DECK_DEV_TOOLS_FEATURE_MARKER_DO_NOT_SHIP";

#[cfg(windows)]
pub mod audio;
#[cfg(windows)]
pub mod clipboard;
#[cfg(windows)]
pub mod decode;
#[cfg(windows)]
pub mod display;
#[cfg(windows)]
pub mod input;
#[cfg(windows)]
pub mod logging;
#[cfg(windows)]
pub mod media;
#[cfg(windows)]
pub mod platform;
#[cfg(windows)]
pub mod present;
#[cfg(windows)]
pub mod shell;
#[cfg(windows)]
pub mod show;

/// The seams this shell implements, from the portable Deck.
#[cfg(windows)]
pub use arcen_deck_core::{
    DeckServices,
    clipboard::PlatformClipboard,
    decode::{VideoDecoderBackend, VideoDecoderFactory},
    present::VideoPresenterSink,
};

/// Lets a command line (`--version`, `show`, `--connect`) write to the
/// console it was started from: the Deck is a windows-subsystem program and
/// gets none of its own. Output that is already redirected (a pipe, a file)
/// keeps its handles.
#[cfg(windows)]
pub fn attach_parent_console() {
    use windows::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    };
    // SAFETY: plain queries of this process's standard handles, and
    // attaching to the parent's console, which fails harmlessly when there
    // is none.
    #[allow(unsafe_code, reason = "Win32 console attach; see the SAFETY note")]
    unsafe {
        let redirected = |which| {
            GetStdHandle(which).is_ok_and(|handle| !handle.is_invalid() && !handle.0.is_null())
        };
        if !redirected(STD_OUTPUT_HANDLE) && !redirected(STD_ERROR_HANDLE) {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

/// The Deck's version, as the installer and `--version` report it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `arcen-deck.exe support-bundle [--out DIR]` (shared command, Windows
/// facts).
#[cfg(windows)]
fn support_bundle_command(arguments: &[String]) -> std::process::ExitCode {
    use arcen_deck_core::DeckServices as _;
    let services = platform::WindowsDeckServices::new();
    let _ = logging::init(&services, arcen_deck_core::logging::startup_profile());
    let code = arcen_deck_core::support_bundle::run_command(
        arguments,
        &services,
        services.displays(),
        platform::support::component(),
    );
    arcen_deck_core::logging::shutdown();
    std::process::ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// Runs the Deck with the process arguments (without the program name).
/// Returns the process exit code.
#[cfg(windows)]
#[must_use]
pub fn run(args: &[std::ffi::OsString]) -> std::process::ExitCode {
    if args.first().and_then(|arg| arg.to_str()) == Some("show") {
        let rest: Vec<String> = args[1..]
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        return show::run(&rest);
    }
    if matches!(
        args.first().and_then(|arg| arg.to_str()),
        Some("--version" | "-V")
    ) {
        print!(
            "{}",
            platform::build::version_text(&platform::build::current())
        );
        return std::process::ExitCode::SUCCESS;
    }
    let rest: Vec<String> = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    if rest.first().map(String::as_str) == Some("support-bundle") {
        return support_bundle_command(&rest[1..]);
    }
    shell::run(&rest)
}
