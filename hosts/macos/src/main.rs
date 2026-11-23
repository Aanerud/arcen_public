// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

#![deny(clippy::disallowed_methods)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

#[allow(clippy::too_many_lines)]
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("arcen-pier-macos {}", arcen_pier_macos::VERSION);
        println!("{}", arcen_pier_macos::SOURCE_OFFER);
        println!("Source: {}", arcen_pier_macos::SOURCE_URL);
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "Usage: arcen-pier-macos [--config PATH] [daemon|agent|serve|install-service|uninstall-service|validate-config|inventory|permissions|diagnostics|probe-media|probe-input|probe-keyboard|probe-keymap|probe-clipboard|probe-cursor|probe-desk-privacy|probe-modes|probe-scroll|probe-audio|register-audio-consent|core-audio-ready|request-permissions|new-host-cert|support-bundle]"
        );
        return ExitCode::SUCCESS;
    }

    let (command, config_path, support_args) = match parse_args(&args[1..]) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    if command == "inventory" {
        return match arcen_pier_macos::displays::probe() {
            Ok(displays) => {
                let report = arcen_pier_macos::displays::InventoryReport::from_displays(displays);
                match serde_json::to_string_pretty(&report) {
                    Ok(json) => {
                        println!("{json}");
                        ExitCode::SUCCESS
                    }
                    Err(error) => {
                        eprintln!("display inventory serialization failed: {error}");
                        ExitCode::FAILURE
                    }
                }
            }
            Err(error) => {
                eprintln!("display inventory failed: {error:?}");
                ExitCode::FAILURE
            }
        };
    }
    if command == "permissions" {
        let report = arcen_pier_macos::permissions::PermissionReport::from_snapshot(
            arcen_pier_macos::permissions::probe(),
        );
        return match serde_json::to_string_pretty(&report) {
            Ok(json) => {
                println!("{json}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("permission report serialization failed: {error}");
                ExitCode::FAILURE
            }
        };
    }
    if command == "install-service" || command == "uninstall-service" {
        return run_service(command, &support_args);
    }
    if command == "activate" {
        return run_activate(&support_args);
    }
    if command == "serve" {
        return run_serve(&support_args, &config_path);
    }
    if command == "daemon" {
        return run_daemon(&support_args, &config_path);
    }
    // The virtual HID helper a desktop agent starts. Owns one device and
    // reads reports; nothing else.
    if command == "hid-injector" {
        return match support_args.first().map(String::as_str) {
            None => arcen_pier_macos::input::virtual_keyboard::run_injector(),
            // The same child, owning a bridged tablet's devices (ADR 0022).
            #[cfg(feature = "native-tablet")]
            Some("--bridged-usb") if support_args.len() == 1 => {
                arcen_pier_macos::input::bridged_hid::run_bridged_injector()
            }
            #[cfg(not(feature = "native-tablet"))]
            Some("--bridged-usb") => {
                eprintln!(
                    "hid-injector: {}",
                    arcen_pier_macos::bridged_tablet::NOT_BUILT
                );
                ExitCode::FAILURE
            }
            Some(other) => {
                eprintln!("hid-injector: unknown argument: {other}");
                ExitCode::FAILURE
            }
        };
    }
    // The process that locks the session if the agent serving a Deck dies
    // (`lock_guard`). Started by the agent; nothing else.
    if command == arcen_pier_macos::lock_guard::SUBCOMMAND {
        return arcen_pier_macos::lock_guard::run(&support_args);
    }
    // What the HID system sees of a bridged tablet (ADR 0022). Lab builds
    // only: it sends the Wacom mode switch (a feature report) to the device,
    // so a shipped Pier must not contain it.
    #[cfg(feature = "lab-bridged-hid-probe")]
    if command == "probe-bridged-hid" {
        return arcen_pier_macos::bridged_hid_probe::run(&support_args);
    }
    // The session's keyboard-layout watcher (`host_keymap`): Text Input
    // Sources want a main thread, and the agent's belongs to its runtime.
    if command == "keymap-watch" {
        return arcen_pier_macos::input::layout::run_keymap_watch();
    }
    // This Mac's current layout as a Deck would be told it.
    if command == "probe-keymap" {
        return match arcen_pier_macos::input::layout::read_layout()
            .and_then(|notice| serde_json::to_string_pretty(&notice).ok())
        {
            Some(json) => {
                println!("{json}");
                ExitCode::SUCCESS
            }
            None => {
                eprintln!("probe-keymap: no keyboard layout could be read");
                ExitCode::FAILURE
            }
        };
    }
    if command == "virtual-display-child" {
        return match arcen_pier_macos::virtual_display::run_child_stdio() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("virtual-display-child: {error}");
                ExitCode::FAILURE
            }
        };
    }
    // The installer's launchd definitions, printed from the same code the
    // tests check, so the package cannot ship a hand-edited copy that drifts.
    if command == "launchd-plist" {
        return match support_args.first().map(String::as_str) {
            Some("daemon") => {
                print!(
                    "{}",
                    arcen_pier_macos::service::daemon_plist(
                        std::path::Path::new(arcen_pier_macos::service::PIER_PROGRAM),
                        std::path::Path::new(arcen_pier_macos::service::TLS_DIRECTORY),
                    )
                );
                ExitCode::SUCCESS
            }
            Some("agent") => {
                print!(
                    "{}",
                    arcen_pier_macos::service::agent_plist(std::path::Path::new(
                        arcen_pier_macos::service::AGENT_PROGRAM
                    ))
                );
                ExitCode::SUCCESS
            }
            _ => {
                eprintln!("launchd-plist needs daemon or agent");
                ExitCode::FAILURE
            }
        };
    }
    if command == "agent" {
        return run_agent(&support_args, &config_path);
    }
    if command == "new-host-cert" {
        return run_host_cert(&support_args);
    }
    if command == "probe-keyboard" {
        return match arcen_pier_macos::input::probe_keyboard() {
            Ok(report) => {
                match serde_json::to_string_pretty(&report) {
                    Ok(json) => println!("{json}"),
                    Err(error) => {
                        eprintln!("probe serialization failed: {error}");
                        return ExitCode::FAILURE;
                    }
                }
                if report.usable {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }
            }
            Err(error) => {
                eprintln!("probe-keyboard failed: {error}");
                ExitCode::FAILURE
            }
        };
    }
    // Asks for the permissions rather than reporting them. This is what puts
    // the helper in Privacy & Security at all: TCC lists a subject only after
    // it has requested access, so a host that merely checks is invisible
    // there and an operator has nothing to switch on.
    if command == "request-permissions" {
        let snapshot = arcen_pier_macos::permissions::request();
        match serde_json::to_string_pretty(&snapshot) {
            Ok(json) => println!("{json}"),
            Err(error) => {
                eprintln!("serialization failed: {error}");
                return ExitCode::FAILURE;
            }
        }
        // A refusal is a decision, not a failure of this command.
        return ExitCode::SUCCESS;
    }
    if command == "core-audio-ready" {
        // For the package scripts, after they restart Core Audio: succeeds
        // once Core Audio serves again with the Arcen microphone driver
        // loaded. A fresh process for every question, so a Core Audio
        // connection opened mid-restart is never asked twice.
        return if arcen_pier_macos::microphone_input::hal_device_published() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    if command == "register-audio-consent" {
        // macOS lists a subject under Privacy & Security only once it has
        // asked, and for system audio a Core Audio process tap is what asks.
        // There is no preflight call. Until something asks, the grant cannot
        // be given at all — which is why audio on an otherwise healthy host
        // stayed unavailable with no prompt to approve and nothing in the
        // pane to switch on.
        //
        // Deliberately its own command rather than something `serve` does.
        // Creating a tap holds the output device briefly, and doing that on
        // every launch would contend with the first session's own tap. Run it
        // once, from the account that will serve.
        return match arcen_pier_macos::audio::register_capture_consent() {
            arcen_pier_macos::audio::CaptureConsent::Registered => {
                println!(
                    "Asked for system audio capture. \"Arcen Agent Helper\" is now listed \
                     under System Settings > Privacy & Security > Screen & System Audio \
                     Recording; switch it on there, then run probe-audio to confirm samples \
                     arrive."
                );
                ExitCode::SUCCESS
            }
            arcen_pier_macos::audio::CaptureConsent::Unavailable => {
                eprintln!(
                    "No audio tap could be created, so there is nothing to grant. This host \
                     has no capturable output device, or the request was refused outright."
                );
                ExitCode::FAILURE
            }
        };
    }
    if command == "probe-audio" {
        // Defaults to the policy the host actually ships: muted. An operator
        // checking whether audio works must be checking the configuration
        // they will run, not a laxer one.
        let playback = if support_args.iter().any(|arg| arg == "--audible") {
            arcen_session::pier_config::LocalPlayback::Audible
        } else {
            arcen_session::pier_config::LocalPlayback::Muted
        };
        let report = arcen_pier_macos::audio::probe(playback);
        match serde_json::to_string_pretty(&report) {
            Ok(json) => println!("{json}"),
            Err(error) => {
                eprintln!("probe serialization failed: {error}");
                return ExitCode::FAILURE;
            }
        }
        return if report.usable {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    // Whether this signature may publish a virtual input device: the
    // entitlement is granted to a signing identity, not compiled in, so the
    // only honest answer is to try.
    if command == "probe-virtual-hid" {
        let support = arcen_pier_macos::virtual_hid::probe();
        println!(
            "virtual_hid={} uid={}",
            if support.is_available() {
                "available"
            } else {
                "refused"
            },
            arcen_pier_macos::service::current_uid()
        );
        if let Some(reason) = support.refusal() {
            println!("{reason}");
        }
        return if support.is_available() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    // What a Deck's Tablet support pen delivers to applications here, posted
    // through the session input path and read back from AppKit. Hovers only
    // unless --strokes. Lab builds only: it injects input under the agent's
    // Accessibility approval, so a shipped Pier must not contain it.
    #[cfg(feature = "lab-pen-probe")]
    if command == "probe-pen" {
        let options = match arcen_pier_macos::pen_probe::parse_options(&support_args) {
            Ok(options) => options,
            Err(message) => {
                eprintln!("{message}");
                return ExitCode::FAILURE;
            }
        };
        let evidence = arcen_pier_macos::pen_probe::probe(options);
        match serde_json::to_string_pretty(&evidence) {
            Ok(json) => println!("{json}"),
            Err(error) => {
                eprintln!("probe serialization failed: {error}");
                return ExitCode::FAILURE;
            }
        }
        println!(
            "pen={}",
            if evidence.observed() {
                "observed"
            } else {
                "unobserved"
            }
        );
        return if evidence.observed() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    // What a generic virtual pen digitizer delivers to applications here,
    // read back from the events macOS posts. Hovers only; never presses.
    if command == "probe-pen-hid" {
        let evidence = arcen_pier_macos::virtual_hid::probe_pen();
        match serde_json::to_string_pretty(&evidence) {
            Ok(json) => println!("{json}"),
            Err(error) => {
                eprintln!("probe serialization failed: {error}");
                return ExitCode::FAILURE;
            }
        }
        println!(
            "pen_hid={} tablet_surface={}",
            if evidence.observed() {
                "observed"
            } else {
                "unobserved"
            },
            if evidence.carries_tablet_surface() {
                "carried"
            } else {
                "empty (Basic Tablet stays on CGEvent)"
            }
        );
        return if evidence.observed() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    // Types through the virtual HID keyboard exactly as a session does — the
    // entitled injector child, started by this process — so the whole path
    // is proven on a real desktop: "arcen hid ok", then Command-S.
    if command == "probe-virtual-keyboard" {
        return probe_virtual_keyboard();
    }
    if command == "probe-virtual-display" {
        let before = arcen_pier_macos::displays::probe().map_or(0, |displays| displays.len());
        match arcen_pier_macos::virtual_display::VirtualDisplay::create(2560, 1440, 60.0) {
            Ok(display) => {
                // Give the window server a moment to publish it before asking.
                std::thread::sleep(std::time::Duration::from_millis(1500));
                let after = arcen_pier_macos::displays::probe().unwrap_or_default();
                println!(
                    "created {}x{} id={}",
                    display.size().0,
                    display.size().1,
                    display.display_id()
                );
                println!("displays before={before} after={}", after.len());
                for entry in &after {
                    println!(
                        "  display {} {}x{}",
                        entry.display_id, entry.pixel_width, entry.pixel_height
                    );
                }
                println!("modes on the new display:");
                for (width, height) in
                    arcen_pier_macos::displays::available_modes(display.display_id())
                {
                    println!("  {width}x{height}");
                }
                return ExitCode::SUCCESS;
            }
            Err(error) => {
                eprintln!("virtual display unavailable: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
    if command == "probe-scroll" {
        let Ok(displays) = arcen_pier_macos::displays::probe() else {
            eprintln!("probe-scroll: no display to scroll on");
            return ExitCode::FAILURE;
        };
        let Some(display) = displays.first() else {
            eprintln!("probe-scroll: no display to scroll on");
            return ExitCode::FAILURE;
        };
        let bounds = arcen_pier_macos::input::DesktopBounds::new(
            display.origin_x,
            display.origin_y,
            pixels_to_f64(display.pixel_width),
            pixels_to_f64(display.pixel_height),
        );
        return match arcen_pier_macos::input::probe_scroll(bounds, -5) {
            Ok(report) => {
                match serde_json::to_string_pretty(&report) {
                    Ok(json) => println!("{json}"),
                    Err(error) => eprintln!("probe-scroll: {error}"),
                }
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("probe-scroll failed: {error}");
                ExitCode::FAILURE
            }
        };
    }
    if command == "probe-modes" {
        let displays = match arcen_pier_macos::displays::probe() {
            Ok(displays) => displays,
            Err(error) => {
                eprintln!("probe-modes: {error:?}");
                return ExitCode::FAILURE;
            }
        };
        for display in &displays {
            println!(
                "display {} currently {}x{}",
                display.display_id, display.pixel_width, display.pixel_height
            );
            for (width, height) in arcen_pier_macos::displays::available_modes(display.display_id) {
                println!("  {width}x{height}");
            }
        }
        return ExitCode::SUCCESS;
    }
    // Desk privacy's facts on this Mac (ADR 0025 §5): which displays would go
    // dark, who the window server says posted the input it sees, and with
    // `--blank SECS [--capture]` the displays darkened for that long (and
    // the main display captured before and while dark). Run it from the Aqua
    // session at the Mac: it darkens the screens it is asked to.
    if command == "probe-desk-privacy" {
        return probe_desk_privacy(&support_args);
    }
    // Whether this host can follow the desktop's pointer (ADR 0029). It moves
    // the pointer in a lap and puts it back, unless `--passive`; `--pump`
    // runs the main run loop between samples; `--seconds N` (default 6).
    if command == "probe-cursor" {
        let options = match cursor_probe_options(&support_args) {
            Ok(options) => options,
            Err(message) => {
                eprintln!("probe-cursor: {message}");
                return ExitCode::from(2);
            }
        };
        let report = arcen_pier_macos::cursor_probe::probe(options);
        match serde_json::to_string_pretty(&report) {
            Ok(json) => println!("{json}"),
            Err(error) => eprintln!("probe-cursor: {error}"),
        }
        return if report.image_readable {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    if command == "probe-clipboard" {
        return match arcen_pier_macos::clipboard::probe() {
            Ok(report) => {
                match serde_json::to_string_pretty(&report) {
                    Ok(json) => println!("{json}"),
                    Err(error) => {
                        eprintln!("probe serialization failed: {error}");
                        return ExitCode::FAILURE;
                    }
                }
                if report.usable {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }
            }
            Err(error) => {
                eprintln!("probe-clipboard failed: {error}");
                ExitCode::FAILURE
            }
        };
    }
    if command == "probe-input" {
        let displays = match arcen_pier_macos::displays::probe() {
            Ok(displays) => displays,
            Err(error) => {
                eprintln!("display inventory failed: {error:?}");
                return ExitCode::FAILURE;
            }
        };
        let Some(display) = displays.first() else {
            eprintln!("probe-input failed: no display to map coordinates onto");
            return ExitCode::FAILURE;
        };
        let bounds = arcen_pier_macos::input::DesktopBounds::new(
            display.origin_x,
            display.origin_y,
            pixels_to_f64(display.pixel_width),
            pixels_to_f64(display.pixel_height),
        );
        return match arcen_pier_macos::input::probe(bounds) {
            Ok(report) => {
                match serde_json::to_string_pretty(&report) {
                    Ok(json) => println!("{json}"),
                    Err(error) => {
                        eprintln!("probe serialization failed: {error}");
                        return ExitCode::FAILURE;
                    }
                }
                if report.usable {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }
            }
            Err(error) => {
                eprintln!("probe-input failed: {error}");
                ExitCode::FAILURE
            }
        };
    }
    if command == "probe-media" {
        let options = match arcen_pier_macos::media_probe::parse_options(&support_args) {
            Ok(options) => options,
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::FAILURE;
            }
        };
        return match arcen_pier_macos::media_probe::run(&options) {
            Ok(report) => {
                match serde_json::to_string_pretty(&report) {
                    Ok(json) => println!("{json}"),
                    Err(error) => {
                        eprintln!("probe serialization failed: {error}");
                        return ExitCode::FAILURE;
                    }
                }
                // A probe that ran but proved nothing usable must not report
                // success, or it becomes evidence for a capability we do not
                // actually have.
                if report.usable {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }
            }
            Err(error) => {
                eprintln!("probe-media failed: {error}");
                ExitCode::FAILURE
            }
        };
    }
    let (config, startup) = match arcen_pier_macos::load_config(&config_path) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("configuration failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    if command == "validate-config" {
        println!("Pier configuration is valid.");
        return ExitCode::SUCCESS;
    }
    if command == "diagnostics" {
        return match arcen_pier_macos::DiagnosticsReport::collect(&config, &startup) {
            Ok(report) => match serde_json::to_string_pretty(&report) {
                Ok(json) => {
                    println!("{json}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("diagnostics serialization failed: {error}");
                    ExitCode::FAILURE
                }
            },
            Err(error) => {
                eprintln!("diagnostics failed: {error}");
                ExitCode::FAILURE
            }
        };
    }
    if command == "support-bundle" {
        return run_support_bundle(&support_args, &config, &startup);
    }

    let observability = match arcen_pier_macos::initialize_diagnostics(startup.profile) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("diagnostic setup failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    let source = match startup.profile_source {
        arcen_session::pier_config::LoggingProfileSource::Level => "config_level",
        arcen_session::pier_config::LoggingProfileSource::LegacyVerbosity => {
            "config_legacy_verbosity"
        }
        arcen_session::pier_config::LoggingProfileSource::ProductionDefault => "production_default",
    };
    let sid = match arcen_telemetry::CorrelationId::new("macos-pier-startup") {
        Ok(sid) => sid,
        Err(error) => {
            eprintln!("startup correlation setup failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    let context = arcen_observability::LifecycleContext {
        sid,
        user: None,
        host: None,
        peer_addr: None,
        health_state: None,
    };
    if let Err(error) =
        observability
            .handle()
            .emit_effective_profile(startup.profile, source, context)
    {
        eprintln!("startup profile record failed: {error}");
        return ExitCode::FAILURE;
    }
    match arcen_pier_macos::run(&config, &startup) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("macOS Pier stopped: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Starts the installed service and agents and proves they run, for the
/// package's `postinstall`. The exit status is the shared installer
/// transaction's verdict.
fn run_activate(arguments: &[String]) -> ExitCode {
    let mut agent_uids = Vec::new();
    let mut port = arcen_pier_macos::net::DEFAULT_PORT;
    let mut index = 0;
    while index < arguments.len() {
        let value = arguments.get(index + 1);
        match (arguments[index].as_str(), value) {
            ("--agent-uid", Some(value)) => match value.parse::<u32>() {
                Ok(uid) => agent_uids.push(uid),
                Err(_) => {
                    eprintln!("activate: --agent-uid needs a number, not {value:?}");
                    return ExitCode::FAILURE;
                }
            },
            ("--port", Some(value)) => match value.parse::<u16>() {
                Ok(parsed) => port = parsed,
                Err(_) => {
                    eprintln!("activate: --port needs a port number, not {value:?}");
                    return ExitCode::FAILURE;
                }
            },
            (other, _) => {
                eprintln!("activate: unknown or incomplete argument {other}");
                return ExitCode::FAILURE;
            }
        }
        index += 2;
    }
    if !arcen_pier_macos::service::is_root() {
        eprintln!("activate: must run as root");
        return ExitCode::FAILURE;
    }
    let plan = arcen_pier_macos::activation::ActivationPlan::installed(agent_uids, port);
    match arcen_pier_macos::activation::activate(
        &mut arcen_pier_macos::activation::SystemLaunchd,
        &plan,
    ) {
        Ok(report) => {
            for line in report {
                println!("Arcen Pier: {line}");
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("Arcen Pier: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Runs `serve`: provisions material if needed, binds QUIC, and handshakes.
///
/// This is the command that makes the Mac reachable by a Deck.
/// Installs or removes the launchd service.
fn run_service(command: &str, arguments: &[String]) -> ExitCode {
    let mut program = std::env::current_exe()
        .unwrap_or_else(|_| std::path::PathBuf::from("/usr/local/bin/arcen-pier-macos"));
    let mut tls = std::path::PathBuf::from(arcen_pier_macos::host_cert::DEFAULT_DIRECTORY);
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--program" => {
                index += 1;
                let Some(path) = arguments.get(index) else {
                    eprintln!("--program requires a path");
                    return ExitCode::FAILURE;
                };
                program = std::path::PathBuf::from(path);
            }
            "--tls-directory" => {
                index += 1;
                let Some(path) = arguments.get(index) else {
                    eprintln!("--tls-directory requires a path");
                    return ExitCode::FAILURE;
                };
                tls = std::path::PathBuf::from(path);
            }
            other => {
                eprintln!("unknown {command} argument: {other}");
                return ExitCode::FAILURE;
            }
        }
        index += 1;
    }

    let outcome = if command == "install-service" {
        arcen_pier_macos::service::install(&program, &tls)
    } else {
        arcen_pier_macos::service::uninstall()
    };
    match outcome {
        Ok(()) => {
            if command == "install-service" {
                println!(
                    "installed {} running {}",
                    arcen_pier_macos::service::DAEMON_LABEL,
                    program.display()
                );
            } else {
                println!("removed {}", arcen_pier_macos::service::DAEMON_LABEL);
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{command}: {error}");
            ExitCode::FAILURE
        }
    }
}

#[allow(clippy::too_many_lines)]
/// Loads the QUIC server configuration from provisioned material.
///
/// # Errors
///
/// Returns a message naming the command that fixes it, because a missing
/// certificate is the most common first-run failure and an operator should not
/// have to guess.
fn load_server_config(
    directory: &std::path::Path,
) -> Result<
    (
        quinn::ServerConfig,
        std::sync::Arc<arcen_pier_macos::tls_material::TlsLifecycle>,
    ),
    String,
> {
    let paths = arcen_pier_macos::host_cert::MaterialPaths::in_directory(directory);
    arcen_pier_macos::net::served_material(&paths.certificate, &paths.key).map_err(|error| {
        format!(
            "serve: {error}\nRun `arcen-pier-macos new-host-cert --directory {}` first.",
            directory.display()
        )
    })
}

/// What `serve` was asked to do.
struct ServeOptions {
    directory: std::path::PathBuf,
    port: Option<u16>,
    once: bool,
    stream_frames: Option<u64>,
    local_playback: Option<arcen_session::pier_config::LocalPlayback>,
    /// How long to wait for the authenticated account to reach the console.
    first_login_timeout: std::time::Duration,
}

/// Parses `serve` arguments.
///
/// # Errors
///
/// Returns the message to show the operator when an argument is unusable.
fn parse_serve_options(arguments: &[String]) -> Result<ServeOptions, String> {
    let mut options = ServeOptions {
        directory: std::path::PathBuf::from(arcen_pier_macos::host_cert::DEFAULT_DIRECTORY),
        port: None,
        once: false,
        stream_frames: None,
        local_playback: None,
        first_login_timeout: arcen_pier_macos::console::DEFAULT_FIRST_LOGIN_TIMEOUT,
    };

    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--tls-directory" => {
                index += 1;
                let path = arguments
                    .get(index)
                    .ok_or_else(|| "--tls-directory requires a path".to_owned())?;
                options.directory = std::path::PathBuf::from(path);
            }
            "--port" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| "--port requires a number".to_owned())?;
                options.port = Some(
                    value
                        .parse()
                        .map_err(|_| format!("--port expects a number, got '{value}'"))?,
                );
            }
            // Accept one Deck and exit, so an end-to-end test can run without
            // a supervisor.
            "--once" => options.once = true,
            // Leave the host's own speakers working. Explicit because the
            // failure it permits — sound from an unattended machine — is not
            // something to enable by accident.
            "--audible" => {
                options.local_playback = Some(arcen_session::pier_config::LocalPlayback::Audible);
            }
            // Matches the Windows host's first_login_timeout_secs, so one
            // runbook covers both.
            "--first-login-timeout-secs" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| "--first-login-timeout-secs requires a number".to_owned())?;
                let secs: u64 = value.parse().map_err(|_| {
                    format!("--first-login-timeout-secs expects a number, got '{value}'")
                })?;
                options.first_login_timeout = std::time::Duration::from_secs(secs);
            }
            "--frames" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| "--frames requires a count".to_owned())?;
                options.stream_frames = Some(
                    value
                        .parse()
                        .map_err(|_| format!("--frames expects a number, got '{value}'"))?,
                );
            }
            other => return Err(format!("unknown serve argument: {other}")),
        }
        index += 1;
    }
    Ok(options)
}

/// Streams one authenticated desktop until the client leaves.
///
/// # Errors
///
/// Returns the reason streaming stopped, when it stopped for a reason worth
/// reporting rather than the client simply disconnecting.
async fn serve_desktop(
    socket: &mut arcen_pier_macos::net::PierSocket,
    handshake: &arcen_pier_macos::session::Handshake,
    stream_frames: Option<u64>,
    telemetry: arcen_pier_macos::observability::HostTelemetry,
    session_id: arcen_telemetry::CorrelationId,
    microphone: Option<
        &mut arcen_pier_macos::microphone_input::MicrophoneIngress<
            arcen_pier_macos::microphone_input::NativeMicrophoneDevice,
        >,
    >,
    audio: Option<&mut arcen_pier_macos::audio::AudioCaptureSession>,
    audio_channel: Option<tokio::net::UnixStream>,
    sent_frames: arcen_telemetry::SentFrames,
) -> Result<u64, Box<arcen_pier_macos::stream::StreamEnded>> {
    if let Some(multi_monitor) = handshake.multi_monitor.as_ref() {
        return serve_multi_desktop(
            socket,
            handshake,
            multi_monitor,
            stream_frames,
            telemetry,
            session_id,
            audio,
            sent_frames,
        )
        .await;
    }
    // Separate providers per contract, chosen by the plan resolved at the
    // handshake. Not a flag on one capture path: Grading and the 8-bit path
    // have different surface formats and different copy costs, and the fast
    // path stays fast by not carrying the other one.
    // What the Deck asked to receive, which is not always this host's own
    // screen size. Input bounds below stay on the host geometry, because
    // pointer coordinates arrive normalised and are mapped onto the desktop
    // the pointer actually moves across.
    let width = handshake.capture_width as usize;
    let height = handshake.capture_height as usize;
    let display_id = handshake.display_id;
    // Host cursor means the compositor draws the pointer into the frame.
    // Local means the Deck draws it, with the desktop cursor's images when
    // this session sends them (ADR 0029), so the capture leaves it out.
    // `cursor_mode` is the settled authority: a Deck that asked for the
    // automatic cursor gets Local when this agent could read the cursor at
    // sign-in, and Host when it could not.
    let shows_cursor = handshake.cursor_mode == arcen_protocol::messages::CursorMode::Host;
    let capture = match handshake.plan.tier {
        arcen_media::session_plan::VideoTier::Standard => {
            arcen_pier_macos::capture::CaptureConfig::sdr(display_id, width, height, handshake.fps)
                .showing_cursor(shows_cursor)
        }
        arcen_media::session_plan::VideoTier::Grading => {
            arcen_pier_macos::capture::CaptureConfig::grading(
                display_id,
                width,
                height,
                handshake.fps,
            )
            .showing_cursor(shows_cursor)
            .stating_sdr_transfer(handshake.plan.transfer)
        }
        // Only resolved when the display being captured reported headroom
        // above SDR white; see the handshake's HDR desktop proof.
        arcen_media::session_plan::VideoTier::HighDynamicRange => {
            arcen_pier_macos::capture::CaptureConfig::hdr(display_id, width, height, handshake.fps)
                .showing_cursor(shows_cursor)
        }
    };
    if let Some(reason) = handshake.plan.degraded {
        eprintln!(
            "serving {:?} instead of what was requested: {reason:?}",
            handshake.plan.tier
        );
    }
    let streamed = arcen_pier_macos::stream::stream(
        socket,
        arcen_pier_macos::stream::StreamSession {
            capture,
            codec: handshake.codec,
            motion_priority: handshake.motion_priority,
            requested_pipeline: handshake.requested_pipeline,
            served_pipeline: handshake.active_pipeline.clone(),
            // The login window has no pasteboard: AppKit returns none, and
            // asking for one panicked the clipboard thread.
            clipboard: handshake.clipboard.filter(|_| !handshake.login_window),
            cursor_mode: handshake.cursor_mode,
            input_mode_results: handshake.input_mode_results.clone(),
            audio_channel,
            audio_encoding: arcen_pier_macos::stream::AudioEncoding::for_stream(handshake.audio),
            frame_budget: stream_frames,
            // The desktop being captured, which is not the host's own when a
            // display was arranged for this session. Pointer coordinates
            // arrive normalised against the picture and are mapped onto this;
            // using the host's own size while serving a larger arranged
            // display put every click three quarters of the way to where it
            // was aimed.
            //
            // The display's own rectangle in the space events are posted in,
            // points rather than pixels and at its real origin. The capture
            // size stands in only when the display cannot be read.
            input_bounds: arcen_pier_macos::displays::point_bounds(display_id).map_or_else(
                || {
                    arcen_pier_macos::input::DesktopBounds::new(
                        0.0,
                        0.0,
                        pixels_to_f64(width),
                        pixels_to_f64(height),
                    )
                },
                |(x, y, w, h)| arcen_pier_macos::input::DesktopBounds::new(x, y, w, h),
            ),
            telemetry,
            session_id,
            microphone,
            audio,
            path_signal_connection: socket.get_ref().path_signal_connection(),
            refine: handshake.refine,
            host_keymap: handshake.host_keymap,
            sent_frames,
        },
    )
    .await;
    // On every ending, not only the clean one: a stream that capture or the
    // client ended has done exactly the work someone will want to read.
    let stats = match &streamed {
        Ok(stats) => stats,
        Err(ended) => &ended.stats,
    };
    println!(
        "streamed {} frames ({} keyframes, {:.1} fps, {:.2} ms mean, {:.2} ms worst, \
         {} bytes); input applied {}, out of order {}",
        stats.frames_sent,
        stats.keyframes,
        stats.sent_fps,
        stats.mean_frame_ms,
        stats.max_frame_ms,
        stats.bytes_sent,
        stats.input.applied,
        stats.input.out_of_order
    );
    streamed.map(|stats| stats.frames_sent)
}

#[allow(clippy::too_many_arguments)]
async fn serve_multi_desktop(
    socket: &mut arcen_pier_macos::net::PierSocket,
    handshake: &arcen_pier_macos::session::Handshake,
    multi_monitor: &arcen_pier_macos::multi_monitor::MacOsMultiMonitorPlan,
    stream_frames: Option<u64>,
    telemetry: arcen_pier_macos::observability::HostTelemetry,
    session_id: arcen_telemetry::CorrelationId,
    audio: Option<&mut arcen_pier_macos::audio::AudioCaptureSession>,
    sent_frames: arcen_telemetry::SentFrames,
) -> Result<u64, Box<arcen_pier_macos::stream::StreamEnded>> {
    let epoch =
        arcen_media::MediaStreamEpoch::new(multi_monitor.generation.get()).map_err(|error| {
            Box::new(arcen_pier_macos::stream::StreamEnded {
                stats: arcen_pier_macos::stream::StreamStats::default(),
                error: arcen_pier_macos::stream::StreamError::Encode(error.to_string()),
            })
        })?;
    let monitors = multi_monitor
        .monitors
        .iter()
        .map(|monitor| arcen_pier_macos::stream::RegionStreamPlan {
            capture: multi_capture_config(handshake.plan, monitor.fps, monitor).showing_cursor(
                handshake.cursor_mode == arcen_protocol::messages::CursorMode::Host,
            ),
            monitor_id: monitor.session_monitor_id,
            topology_generation: multi_monitor.generation,
            stream_epoch: epoch,
        })
        .collect();
    let streamed = arcen_pier_macos::stream::stream_multi(
        socket,
        arcen_pier_macos::stream::MultiStreamSession {
            monitors,
            codec: handshake.codec,
            motion_priority: handshake.motion_priority,
            requested_pipeline: handshake.requested_pipeline,
            served_pipeline: handshake.active_pipeline.clone(),
            // The login window has no pasteboard: AppKit returns none, and
            // asking for one panicked the clipboard thread.
            clipboard: handshake.clipboard.filter(|_| !handshake.login_window),
            cursor_mode: handshake.cursor_mode,
            input_mode_results: handshake.input_mode_results.clone(),
            audio_encoding: arcen_pier_macos::stream::AudioEncoding::for_stream(handshake.audio),
            frame_budget: stream_frames,
            input: arcen_pier_macos::input_session::InputMode::Region(
                arcen_pier_macos::input_session::RegionInputSession::new(
                    multi_monitor.applied_regions.clone(),
                    multi_monitor
                        .monitors
                        .iter()
                        .map(|monitor| {
                            (
                                monitor.session_monitor_id,
                                arcen_pier_macos::input::DesktopBounds::new(
                                    monitor.display.origin_x,
                                    monitor.display.origin_y,
                                    monitor.display.pixel_width as f64,
                                    monitor.display.pixel_height as f64,
                                ),
                            )
                        })
                        .collect(),
                ),
            ),
            telemetry,
            session_id,
            audio,
            path_signal_connection: socket.get_ref().path_signal_connection(),
            host_keymap: handshake.host_keymap,
            sent_frames,
        },
    )
    .await;
    let stats = match &streamed {
        Ok(stats) => stats,
        Err(ended) => &ended.stats,
    };
    println!(
        "streamed {} region frames across {} monitors ({:.1} fps, {} bytes)",
        stats.frames_sent,
        multi_monitor.monitors.len(),
        stats.sent_fps,
        stats.bytes_sent
    );
    streamed.map(|stats| stats.frames_sent)
}

fn multi_capture_config(
    plan: arcen_media::session_plan::ResolvedVideoPlan,
    fps: u32,
    monitor: &arcen_pier_macos::multi_monitor::MacOsMonitorPlan,
) -> arcen_pier_macos::capture::CaptureConfig {
    match plan.tier {
        arcen_media::session_plan::VideoTier::Standard => {
            arcen_pier_macos::capture::CaptureConfig::sdr(
                monitor.display.display_id,
                monitor.display.pixel_width,
                monitor.display.pixel_height,
                fps,
            )
        }
        arcen_media::session_plan::VideoTier::Grading => {
            arcen_pier_macos::capture::CaptureConfig::grading(
                monitor.display.display_id,
                monitor.display.pixel_width,
                monitor.display.pixel_height,
                fps,
            )
            .stating_sdr_transfer(plan.transfer)
        }
        arcen_media::session_plan::VideoTier::HighDynamicRange => {
            arcen_pier_macos::capture::CaptureConfig::hdr(
                monitor.display.display_id,
                monitor.display.pixel_width,
                monitor.display.pixel_height,
                fps,
            )
        }
    }
}

/// Decides whether a session may be served, and takes the mute lease if so.
///
/// Two gates, both fail-closed.
///
/// The first is ownership: this Pier can only serve the desktop its own window
/// server session owns. Authenticating a different PAM-permitted account and
/// then handing over the console user's screen, input and pasteboard would be
/// a straightforward privilege escalation, so a mismatch is refused until real
/// session activation exists.
///
/// The second is privacy: local playback is silenced for the whole session
/// before anything is served, because someone beside this Mac hearing the
/// remote user is a failure whether or not audio is being transmitted. A mute
/// that cannot be established refuses the session rather than proceeding
/// quietly.
///
/// # Errors
///
/// Returns the reason the session must not proceed.
async fn admit_session(
    authenticated: &str,
    first_login_timeout: std::time::Duration,
) -> Result<(), String> {
    // The console owner is read from the system, not from this process's
    // environment: a service never runs as the person at the screen. The wait
    // exists because switching users at the machine is an ordinary thing to
    // do, and refusing instantly would make Fast User Switching unusable. It
    // is the same procedure the Windows host follows for a first interactive
    // login.
    let authenticated = authenticated.to_owned();
    let owner = arcen_pier_macos::blocking::run_exclusive(
        "arcen-macos-console-owner",
        &CONSOLE_OWNER_SLOT,
        console_owner_failure,
        move || {
            arcen_pier_macos::console::wait_for_console_owner(&authenticated, first_login_timeout)
                .map_err(|error| error.to_string())
        },
    )
    .await??;
    tracing::info!(
        target: "arcen::session",
        user = %owner,
        "serving the console session owned by the authenticated account"
    );

    Ok(())
}

fn console_owner_failure(failure: arcen_pier_macos::blocking::ExclusiveFailure) -> String {
    match failure {
        arcen_pier_macos::blocking::ExclusiveFailure::Busy => {
            "a previous console-owner check is still running".to_owned()
        }
        arcen_pier_macos::blocking::ExclusiveFailure::Spawn(error) => {
            format!("start console-owner worker: {error}")
        }
        arcen_pier_macos::blocking::ExclusiveFailure::Disconnected => {
            "console-owner worker did not complete".to_owned()
        }
    }
}

/// The logging profile the configuration asks for.
///
/// Read rather than pinned. `serve` used a constant `Info`, which meant the
/// one command that streams ignored `logging.level` entirely: setting it to
/// `debug` changed nothing, no debug record was ever written, and every
/// measurement taken to diagnose this host was taken at a verbosity nobody had
/// chosen.
///
/// A host that cannot read its configuration still serves, at the profile a
/// production host would want.
fn configured_profile(config_path: &std::path::Path) -> arcen_telemetry::OperationalProfile {
    arcen_pier_macos::load_config(config_path)
        .map_or(arcen_telemetry::OperationalProfile::Info, |(_, startup)| {
            startup.profile
        })
}

/// What every process that serves a desktop reads before it starts.
struct SessionHost {
    /// Kept alive for the life of the process; dropping it stops the log.
    _observability: Option<arcen_observability::InstalledObservability>,
    telemetry: arcen_pier_macos::observability::HostTelemetry,
    file_config: arcen_pier_macos::PierFileConfig,
}

impl SessionHost {
    /// The policy a desktop-serving process advertises and enforces.
    ///
    /// Separate from loading, because building it probes the encoder, and the
    /// network service — which never encodes — should not start VideoToolbox.
    fn session_policy(&self) -> Result<arcen_pier_macos::session::SessionPolicy, ExitCode> {
        arcen_pier_macos::session::SessionPolicy::from_config(&self.file_config).map_err(|error| {
            eprintln!("configuration failed: {error}");
            ExitCode::FAILURE
        })
    }
}

/// Opens the structured log and reads the configuration.
///
/// # Errors
///
/// Returns the exit code to use when the configuration cannot be used.
fn load_session_host(config_path: &std::path::Path) -> Result<SessionHost, ExitCode> {
    // `serve` is dispatched before the startup path that installs the
    // canonical runtime, so without this the one command that actually
    // streams would emit no structured records at all — exactly the command
    // whose frame rate and latency someone needs to read afterwards.
    //
    // A host that cannot open its log directory must still serve desktops:
    // losing telemetry is a degradation, and refusing to run because of it
    // would turn a logging fault into an outage. The failure is printed once
    // and the session continues with a disabled emitter.
    let observability =
        match arcen_pier_macos::initialize_diagnostics(configured_profile(config_path)) {
            Ok(runtime) => Some(runtime),
            Err(error) => {
                eprintln!("structured logging unavailable, continuing without it: {error}");
                None
            }
        };
    let telemetry = observability.as_ref().map_or_else(
        arcen_pier_macos::observability::HostTelemetry::disabled,
        |runtime| {
            arcen_pier_macos::observability::HostTelemetry::new(
                runtime.handle().clone(),
                Some(hostname()),
            )
        },
    );

    // Absent, explicitly named, and malformed are three different things, and
    // Linux already settled which is which in `hosts/linux/src/config.rs`:
    // absent at the default path is a default installation and uses defaults;
    // absent at a path the operator *named* is an error, because a typo must
    // not silently hand back defaults; malformed is always an error.
    //
    // These had been collapsed into one failure, and it turned every fresh
    // install into a crash loop — the installer wrote no `pier.json`, so the
    // agent refused to start, launchd restarted it on its throttle, and the
    // operator saw a connection time out against a host that had never once
    // listened.
    let file_config = match arcen_pier_macos::load_config(config_path) {
        Ok((config, _)) => config,
        Err(error)
            if arcen_pier_macos::is_missing_config(&error)
                && config_path == std::path::Path::new(arcen_pier_macos::DEFAULT_CONFIG_PATH) =>
        {
            eprintln!(
                "no configuration at {}; serving built-in defaults",
                config_path.display()
            );
            arcen_pier_macos::default_config()
        }
        Err(error) => {
            eprintln!("configuration failed: {error}");
            return Err(ExitCode::FAILURE);
        }
    };
    if file_config.video.qp_map.is_some() {
        tracing::info!(
            target: arcen_telemetry::names::target::SESSION,
            "video.qp_map is accepted but not supported by the macOS Pier VideoToolbox encoder"
        );
    }
    let (desk_private, desk_notes) = arcen_pier_macos::desk_privacy_startup_notes(&file_config);
    // Off, or a desk it cannot make private, at error level: every log
    // profile records it.
    for (index, note) in desk_notes.into_iter().enumerate() {
        if index == 0 && desk_private {
            tracing::info!(target: arcen_telemetry::names::target::SESSION, "{note}");
        } else {
            tracing::error!(target: arcen_telemetry::names::target::SESSION, "{note}");
        }
    }
    Ok(SessionHost {
        _observability: observability,
        telemetry,
        file_config,
    })
}

/// The address and port the configuration asks to listen on.
fn listen_address(
    file_config: &arcen_pier_macos::PierFileConfig,
    port: Option<u16>,
) -> (String, u16) {
    let port = port
        .or(file_config.listen.quic_port)
        .or(file_config.listen.port)
        .unwrap_or(arcen_pier_macos::net::DEFAULT_PORT);
    let host = file_config
        .listen
        .host
        .as_deref()
        .unwrap_or("0.0.0.0")
        .to_owned();
    (host, port)
}

fn run_serve(arguments: &[String], config_path: &std::path::Path) -> ExitCode {
    let options = match parse_serve_options(arguments) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };
    let ServeOptions {
        directory,
        port,
        once,
        stream_frames,
        local_playback,
        first_login_timeout,
    } = options;

    let host_setup = match load_session_host(config_path) {
        Ok(setup) => setup,
        Err(code) => return code,
    };
    let session_policy = match host_setup.session_policy() {
        Ok(policy) => policy,
        Err(code) => return code,
    };
    let local_playback = local_playback.unwrap_or(host_setup.file_config.audio.local_playback);
    let (host, port) = listen_address(&host_setup.file_config, port);
    // One process serves everything here, so the child is this executable.
    if let Ok(program) = std::env::current_exe() {
        arcen_pier_macos::bridged_tablet::set_injector_program(program);
    }

    let (config, tls) = match load_server_config(&directory) {
        Ok(loaded) => loaded,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };

    // Before the main thread is given to the runtime, and only from here:
    // NSApplication must be created on the main thread, and the cursor
    // accessors return NULL until it exists. Prohibited activation means no
    // Dock icon and no menu bar, so a background agent stays one.
    if !arcen_pier_macos::cursor_probe::connect_to_window_server() {
        tracing::warn!(
            target: arcen_telemetry::names::target::HID,
            "no window-server connection; cursor shapes will not be reported"
        );
    }

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("serve: start runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(serve_loop(
        config,
        tls,
        host,
        port,
        once,
        stream_frames,
        local_playback,
        first_login_timeout,
        session_policy,
        host_setup.telemetry,
        arcen_pier_macos::timezone_redirection_enabled(&host_setup.file_config),
    ))
}

/// How long the service holds an admitted Deck while the console session's
/// agent appears.
///
/// Long enough to cover an agent starting after a login or restarting after a
/// crash; short enough that a Deck asking for a desktop nobody is logged in to
/// is told so while the person is still looking at it.
const AGENT_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Runs the network service: the listener, the TLS key and admission, and no
/// desktop.
///
/// This is what launchd starts at boot as the service account. It survives
/// logout and user switching because it belongs to the machine rather than to
/// whoever is at the screen, and it hands each admitted Deck to the agent of
/// the session on the console.
fn run_daemon(arguments: &[String], config_path: &std::path::Path) -> ExitCode {
    let options = match parse_serve_options(arguments) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };
    // The service parses untrusted network input and holds the host key. It
    // has no reason to be able to rewrite the system.
    if arcen_pier_macos::service::is_root() && std::env::var_os("ARCEN_DAEMON_ALLOW_ROOT").is_none()
    {
        eprintln!(
            "daemon: refusing to run as root; launchd starts it as {}",
            arcen_pier_macos::service::SERVICE_ACCOUNT
        );
        return ExitCode::FAILURE;
    }
    let host_setup = match load_session_host(config_path) {
        Ok(setup) => setup,
        Err(code) => return code,
    };
    let (host, port) = listen_address(&host_setup.file_config, options.port);
    let (config, tls) = match load_server_config(&options.directory) {
        Ok(loaded) => loaded,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("daemon: start runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let reconnect_window_secs = host_setup
        .file_config
        .auth
        .reconnect_window_secs
        .unwrap_or(arcen_session::direct_reconnect::DEFAULT_RECONNECT_WINDOW_SECONDS);
    // Exact still image: the grant offers the refine lane only when this
    // host may refine at all.
    let refine_allowed = arcen_pier_macos::refine::COMPILED
        && arcen_session::pier_config::LosslessRefinementSetting::resolve(
            host_setup.file_config.video.lossless_refinement,
        );
    runtime.block_on(daemon_loop(
        config,
        tls,
        host,
        port,
        host_setup.telemetry,
        reconnect_window_secs,
        refine_allowed,
    ))
}

async fn daemon_loop(
    config: quinn::ServerConfig,
    tls: std::sync::Arc<arcen_pier_macos::tls_material::TlsLifecycle>,
    host: String,
    port: u16,
    telemetry: arcen_pier_macos::observability::HostTelemetry,
    reconnect_window_secs: u32,
    refine_allowed: bool,
) -> ExitCode {
    let address = match resolve_bind_addr(&host, port).await {
        Ok(address) => address,
        Err(error) => {
            eprintln!("daemon: {error}");
            return ExitCode::FAILURE;
        }
    };
    let listener = match arcen_pier_macos::net::Listener::bind(address, config) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("daemon: {error}");
            return ExitCode::FAILURE;
        }
    };
    let socket_path = std::path::Path::new(arcen_pier_macos::relay::AGENT_SOCKET);
    let agent_listener = match arcen_pier_macos::relay::AgentRegistry::bind(socket_path) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("daemon: {error}");
            return ExitCode::FAILURE;
        }
    };
    let own_uid = arcen_pier_macos::service::current_uid();
    // Pinned to the installed helper when this is the installed service; a
    // service run by hand from a build tree has no bundle to pin to.
    let expected_agent = std::env::current_exe()
        .ok()
        .and_then(|path| arcen_pier_macos::relay::expected_agent_program(&path));
    if expected_agent.is_none() {
        eprintln!("daemon: not running from a bundle; any local agent may register");
    }
    let _ = reconnect_window_secs;
    let registry = arcen_pier_macos::relay::AgentRegistry::new(
        (own_uid != 0).then_some(own_uid),
        expected_agent,
    );
    tokio::spawn(std::sync::Arc::clone(&registry).run(agent_listener));
    match arcen_pier_macos::microphone_input::MicrophoneHub::start() {
        Ok(hub) => {
            let _hub = Box::leak(Box::new(hub));
            eprintln!("daemon: microphone Mach service ready");
        }
        Err(error) => eprintln!("daemon: microphone Mach service unavailable: {error:?}"),
    }

    if let Some(fields) = arcen_telemetry::lifecycle_fields::service_start(
        "arcen-pier-macos",
        arcen_pier_macos::VERSION,
        std::process::id(),
    ) {
        telemetry.emit(
            arcen_telemetry::LifecycleEventKind::ServiceStart,
            &arcen_pier_macos::observability::SessionScope::service(
                arcen_telemetry::CorrelationId::from_uuid_v4_bytes(
                    arcen_pier_macos::observability::random_correlation_bytes(),
                ),
            ),
            fields,
            arcen_telemetry::names::target::HEALTH,
            "service start",
        );
    }
    watch_tls(tls, &telemetry);
    println!(
        "{}",
        arcen_pier_macos::startup_build_line(&arcen_pier_macos::build_identity())
    );
    match listener.local_addr() {
        Ok(bound) => println!(
            "listening on {bound}/udp; agents on {}",
            socket_path.display()
        ),
        Err(error) => {
            eprintln!("daemon: {error}");
            return ExitCode::FAILURE;
        }
    }

    serve_decks(&listener, &registry, refine_allowed).await
}

/// Reports the served certificate and starts its lifecycle: `SIGHUP`
/// reloads it, and expiring and expired are reported as they happen, the
/// same events the Linux and Windows Piers emit.
fn watch_tls(
    tls: std::sync::Arc<arcen_pier_macos::tls_material::TlsLifecycle>,
    telemetry: &arcen_pier_macos::observability::HostTelemetry,
) {
    arcen_pier_macos::tls_material::emit(telemetry, tls.startup_events());
    let sink_telemetry = telemetry.clone();
    if let Err(error) = arcen_pier_macos::tls_material::spawn_watch(tls, move |events| {
        arcen_pier_macos::tls_material::emit(&sink_telemetry, events);
    }) {
        eprintln!("tls: no SIGHUP reload ({error}); restart the service to load new material");
    }
}

/// Admits Decks side by side and relays each to its desktop agent; returns
/// only when the endpoint closes.
async fn serve_decks(
    listener: &arcen_pier_macos::net::Listener,
    registry: &std::sync::Arc<arcen_pier_macos::relay::AgentRegistry>,
    refine_allowed: bool,
) -> ExitCode {
    // Handshakes run side by side, each holding one of the shared
    // pre-sign-in places from its first packet until its control lane is
    // open, so a peer that never finishes delays nobody else.
    let handshakes =
        std::sync::Arc::new(arcen_session::session_admission::PreauthAdmission::default());
    let relays = std::sync::Arc::new(arcen_session::session_admission::PreauthAdmission::new(
        RELAY_LIMIT,
        arcen_session::session_admission::PREAUTH_PER_SOURCE,
    ));
    loop {
        let incoming = match listener.next_incoming().await {
            Ok(incoming) => incoming,
            Err(error) => {
                eprintln!("daemon: {error}");
                return ExitCode::FAILURE;
            }
        };
        let peer = incoming.remote_address();
        let handshake =
            match handshakes.try_enter_unproved(peer.ip(), incoming.remote_address_validated()) {
                Ok(place) => place,
                Err(arcen_session::session_admission::PreauthRefusal::AddressUnproved) => {
                    // A stateless Retry: a forged source cannot answer it, so
                    // it cannot hold another machine's places.
                    if let Err(error) = incoming.retry() {
                        error.into_incoming().refuse();
                    }
                    continue;
                }
                Err(refusal) => {
                    tracing::info!(
                        target: "arcen::net",
                        %peer,
                        reason = refusal.as_str(),
                        "QUIC connection refused before TLS: {refusal}"
                    );
                    incoming.refuse();
                    continue;
                }
            };
        let relays = std::sync::Arc::clone(&relays);
        let registry = std::sync::Arc::clone(registry);
        tokio::spawn(async move {
            let admitted = arcen_pier_macos::net::Listener::admit(
                incoming,
                arcen_pier_macos::net::admission_deadline(),
            )
            .await;
            drop(handshake);
            let Some((mut deck, peer)) = admitted else {
                return;
            };
            // The one-session rule is the agent's, applied after the password.
            // The service only bounds how many Decks it relays at once, overall
            // and per address, so a silent peer holds one sign-in slot rather
            // than the whole host.
            match relays.try_enter(peer.ip()) {
                Ok(held) => {
                    relay_one(deck, peer, &registry, refine_allowed).await;
                    drop(held);
                }
                Err(refusal) => {
                    eprintln!("refused {peer}: {refusal}");
                    let _ = arcen_pier_macos::net::refuse_lanes(
                        &mut deck,
                        "too many connections to this Mac right now; try again shortly",
                    )
                    .await;
                }
            }
        });
    }
}

/// Relays at once: one per agent sign-in slot. The service holds a Deck's
/// place for its whole relay, at most
/// [`arcen_session::session_admission::PREAUTH_PER_SOURCE`] from one source,
/// because the sign-in and the session both run in the console user's agent.
const RELAY_LIMIT: usize = AGENT_SIGN_IN_SLOTS;

/// Hands one admitted Deck to the console session's agent and relays it.
async fn relay_one(
    mut deck: arcen_transport::quic::SessionLaneSocket,
    peer: std::net::SocketAddr,
    registry: &arcen_pier_macos::relay::AgentRegistry,
    refine_allowed: bool,
) {
    let attached = registry
        .attach(
            arcen_pier_macos::console::console_holder,
            &peer.to_string(),
            AGENT_WAIT,
        )
        .await;
    match attached {
        Ok(attached) => {
            let arcen_pier_macos::relay::AttachedAgent {
                stream: agent,
                agent: parked,
                session,
                audio_channel,
            } = attached;
            tracing::info!(
                target: "arcen::relay",
                %peer,
                session,
                uid = parked.uid,
                kind = ?parked.kind,
                "relaying a Deck to its desktop agent"
            );
            // One lane per role; the lane setup already keeps the send
            // window interactive, so the agent's writes feel the path.
            let relayed = arcen_pier_macos::relay::relay_lanes_gated(
                deck,
                agent,
                session,
                Some(audio_channel),
                true,
                refine_allowed,
                Some(arcen_pier_macos::relay::SignInGate::new(
                    &arcen_pier_macos::relay::SERVICE_AUTH_THROTTLE,
                    Some(peer.ip()),
                )),
            )
            .await;
            registry.end_session(session);
            match relayed {
                Ok(stats) => tracing::info!(
                    target: "arcen::relay",
                    %peer,
                    to_agent = stats.to_agent,
                    to_deck = stats.to_deck,
                    side_channel_audio = stats.side_channel_audio,
                    video_dropped = stats.lanes.video_dropped,
                    audio_dropped = stats.lanes.audio_dropped,
                    keyframe_requests = stats.lanes.keyframe_requests,
                    bulk_dropped = stats.lanes.bulk_dropped,
                    clipboard_failed = stats.lanes.clipboard_failed,
                    "relay ended"
                ),
                Err(error) => tracing::info!(
                    target: "arcen::relay",
                    %peer,
                    %error,
                    "relay ended by the transport"
                ),
            }
        }
        Err(reason) => {
            eprintln!("refused {peer}: {reason}");
            let _ = arcen_pier_macos::net::refuse_lanes(&mut deck, &reason).await;
        }
    }
}

/// Runs a desktop agent: capture, input, pasteboard and audio for the session
/// it was started in, reached through the network service.
///
/// launchd starts one in every graphical session. It holds no key and binds
/// no port, so a second user logging in no longer fights the first for them,
/// and it serves only its own account.
fn run_agent(arguments: &[String], config_path: &std::path::Path) -> ExitCode {
    // launchd starts a LoginWindow agent as root, and a user's agent as that
    // user; which session this is follows from that, and a flag can only
    // confirm it.
    let mut kind = if arcen_pier_macos::service::current_uid() == 0 {
        arcen_session::agent_relay::DesktopSessionKind::LoginWindow
    } else {
        arcen_session::agent_relay::DesktopSessionKind::User
    };
    for argument in arguments {
        match argument.as_str() {
            "--login-window" => kind = arcen_session::agent_relay::DesktopSessionKind::LoginWindow,
            other => {
                eprintln!("unknown agent argument: {other}");
                return ExitCode::FAILURE;
            }
        }
    }
    arcen_pier_macos::service::redirect_stderr_to_user_log();
    let host_setup = match load_session_host(config_path) {
        Ok(setup) => setup,
        Err(code) => return code,
    };
    let local_playback = host_setup.file_config.audio.local_playback;
    let mut session_policy = match host_setup.session_policy() {
        Ok(policy) => policy,
        Err(code) => return code,
    };
    // The login window takes virtual HID only, whatever the configuration
    // says: there the first CGEvent a root agent created blocked forever
    // inside SkyLight and froze the session with it.
    let program = arcen_pier_macos::service::PIER_PROGRAM;
    let backend = if kind == arcen_session::agent_relay::DesktopSessionKind::LoginWindow {
        arcen_pier_macos::input::InputBackend::VirtualHidOnly(program)
    } else {
        match host_setup.file_config.platform.input_backend {
            arcen_pier_macos::InputBackendChoice::VirtualHid => {
                arcen_pier_macos::input::InputBackend::VirtualKeyboard(program)
            }
            arcen_pier_macos::InputBackendChoice::Coregraphics
            | arcen_pier_macos::InputBackendChoice::Auto => {
                arcen_pier_macos::input::InputBackend::CoreGraphics
            }
        }
    };
    arcen_pier_macos::input::set_input_backend(backend);
    // Native tablet's devices belong to the installed Pier's entitled child.
    arcen_pier_macos::bridged_tablet::set_injector_program(program.into());
    // A user's agent serves that user alone. The login window belongs to
    // nobody yet: any account that authenticates may see it and sign in.
    session_policy.serving_uid = (kind == arcen_session::agent_relay::DesktopSessionKind::User)
        .then(arcen_pier_macos::service::current_uid);
    session_policy.login_window =
        kind == arcen_session::agent_relay::DesktopSessionKind::LoginWindow;

    // At the login window launchd starts this agent as soon as the session
    // exists, which can be before its window server does. Measured on the lab:
    // an agent started in the same second as loginwindow found no window
    // server and no display, stayed that way, and refused every Deck with
    // "inventory is empty" until it was restarted. So wait for a display
    // before offering this desktop to anyone.
    // Without a window server there is nothing to serve. Measured at the login
    // window: a process whose first connection failed never connected, even a
    // minute later, while a fresh process started then connected at once. So
    // exit and let launchd start a fresh one rather than offer a desktop that
    // does not exist.
    //
    // A user session has had its window server since before its agent ran,
    // and one that cannot read the cursor still streams without cursor
    // shapes, so it is not held to this. The connection is still made: it is
    // what creates the NSApplication cursor shapes need.
    if kind == arcen_session::agent_relay::DesktopSessionKind::LoginWindow {
        if !wait_for_the_window_server(kind) {
            eprintln!("agent: no window server yet; exiting so launchd starts a fresh agent");
            return ExitCode::from(75);
        }
    } else if !arcen_pier_macos::cursor_probe::connect_to_window_server() {
        tracing::warn!(
            target: arcen_telemetry::names::target::HID,
            "no window-server connection; cursor shapes will not be reported"
        );
    }
    // A Deck the agent before this one was serving when it died: the
    // session is locked before this agent serves anything (GAP-048).
    if let Some(uid) = session_policy
        .serving_uid
        .filter(|_| !session_policy.login_window)
    {
        let _ = arcen_pier_macos::lock_guard::recover_at_start(uid, session_policy.desk_privacy);
    }
    if arcen_pier_macos::timezone::supported_in_this_process(session_policy.login_window) {
        restore_stale_agent_timezone("startup");
    }
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("agent: start runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    // Only a host that will ever tap needs the System Audio Recording answer.
    let ask_audio_consent = host_setup.file_config.audio.enabled || local_playback.requires_mute();
    runtime.block_on(agent_loop(
        kind,
        local_playback,
        ask_audio_consent,
        session_policy,
        host_setup.telemetry,
        arcen_pier_macos::timezone_redirection_enabled(&host_setup.file_config),
    ))
}

/// `probe-cursor`'s options: `--passive`, `--pump`, `--seconds N`.
fn cursor_probe_options(
    args: &[String],
) -> Result<arcen_pier_macos::cursor_probe::ProbeOptions, String> {
    let mut options = arcen_pier_macos::cursor_probe::ProbeOptions::default();
    let mut args = args.iter();
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--passive" => options.sweep = false,
            "--pump" => options.pump_run_loop = true,
            "--seconds" => {
                let seconds = args
                    .next()
                    .and_then(|value| value.parse::<u64>().ok())
                    .filter(|seconds| (1..=60).contains(seconds))
                    .ok_or_else(|| "--seconds needs a whole number from 1 to 60".to_owned())?;
                options.sample_for = std::time::Duration::from_secs(seconds);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(options)
}

/// How long an agent that just connected is given to find its owner at the
/// console. The service only offers it a Deck while its session is on screen,
/// so this covers the moment of a switch rather than a first login.
const AGENT_CONSOLE_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Connects to the window server and waits until it reports a display, for a
/// few seconds. Returns whether both are available.
///
/// A process cannot list displays before it has a window-server connection,
/// so the connection is retried first; the display check follows it.
fn wait_for_the_window_server(kind: arcen_session::agent_relay::DesktopSessionKind) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    let mut connected = false;
    loop {
        connected = connected || arcen_pier_macos::cursor_probe::connect_to_window_server();
        let has_display =
            arcen_pier_macos::displays::probe().is_ok_and(|displays| !displays.is_empty());
        if connected && has_display {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            eprintln!(
                "agent: window server connected={connected}, display={has_display} ({kind:?})"
            );
            return connected && has_display;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// What an agent needs to open a session's audio side channel.
#[derive(Debug, Clone, Copy)]
struct AudioChannelRequest {
    kind: arcen_session::agent_relay::DesktopSessionKind,
    service_uid: Option<u32>,
    session: u64,
}

/// How many Decks an agent will hold in sign-in at once.
///
/// More than one, so a peer that connects and says nothing cannot keep
/// everyone else out for the length of the sign-in timeout. Only one of them
/// can go on to hold the session: the slot is taken after the password.
const AGENT_SIGN_IN_SLOTS: usize = 3;

fn restore_stale_agent_timezone(reason: &str) {
    match arcen_pier_macos::timezone::restore_stale_session_timezone() {
        Ok(Some(restored)) => tracing::warn!(
            target: arcen_telemetry::names::target::SESSION,
            target_timezone = %restored.target,
            unset_tz = restored.unset_tz,
            %reason,
            "restored stale session timezone from Agent Helper launchd sentinel"
        ),
        Ok(None) => {}
        Err(error) => tracing::warn!(
            target: arcen_telemetry::names::target::SESSION,
            %error,
            %reason,
            "could not restore stale session timezone"
        ),
    }
}

fn arm_timezone_sigterm_restore() {
    tokio::spawn(async move {
        let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        else {
            return;
        };
        let _ = signal.recv().await;
        let (done, wait_done) = tokio::sync::oneshot::channel();
        let _ = std::thread::Builder::new()
            .name("arcen-tz-sigterm-restore".to_owned())
            .spawn(move || {
                let _ = done.send(
                    arcen_pier_macos::timezone::restore_active_session_timezone_for_shutdown(),
                );
            });
        match tokio::time::timeout(std::time::Duration::from_secs(2), wait_done).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => tracing::warn!(
                target: arcen_telemetry::names::target::SESSION,
                %error,
                "could not restore active session timezone before SIGTERM exit"
            ),
            Ok(Err(error)) => tracing::warn!(
                target: arcen_telemetry::names::target::SESSION,
                %error,
                "timezone restore task failed before SIGTERM exit"
            ),
            Err(_) => tracing::warn!(
                target: arcen_telemetry::names::target::SESSION,
                "timed out restoring active session timezone before SIGTERM exit"
            ),
        }
        std::process::exit(0);
    });
}

async fn agent_loop(
    kind: arcen_session::agent_relay::DesktopSessionKind,
    local_playback: arcen_session::pier_config::LocalPlayback,
    ask_audio_consent: bool,
    session_policy: arcen_pier_macos::session::SessionPolicy,
    telemetry: arcen_pier_macos::observability::HostTelemetry,
    timezone_redirection: bool,
) -> ExitCode {
    advertise_host_audio();
    // A signed-in person can answer the prompts, so ask them all now, one at a
    // time. The login window has nobody to ask.
    if kind == arcen_session::agent_relay::DesktopSessionKind::User {
        tokio::spawn(ask_for_permissions_in_turn(ask_audio_consent));
        if arcen_pier_macos::timezone::supported_in_this_process(false) {
            arm_timezone_sigterm_restore();
        }
    } else {
        announce_permissions();
    }
    let service_uid =
        arcen_pier_macos::auth::resolve_account(arcen_pier_macos::service::SERVICE_ACCOUNT)
            .map(|account| account.uid);
    let admission = arcen_session::session_admission::SessionAdmissionRuntime::new();
    let waiting_reported = std::rc::Rc::new(std::cell::Cell::new(false));
    // A session holds AppKit and capture state that is not `Send`, so the
    // slots share this thread's executor rather than the runtime's workers.
    let slots = tokio::task::LocalSet::new();
    for _ in 0..AGENT_SIGN_IN_SLOTS {
        slots.spawn_local(agent_slot(
            kind,
            service_uid,
            local_playback,
            session_policy.clone(),
            telemetry.clone(),
            timezone_redirection,
            std::sync::Arc::clone(&admission),
            std::rc::Rc::clone(&waiting_reported),
        ));
    }
    slots.await;
    ExitCode::FAILURE
}

/// Sessions this agent is serving now, across its sign-in slots.
static ACTIVE_SESSIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Counts one served session for as long as it lives.
struct ActiveSession;

impl ActiveSession {
    fn begin() -> Self {
        ACTIVE_SESSIONS.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Self
    }
}

impl Drop for ActiveSession {
    fn drop(&mut self) {
        ACTIVE_SESSIONS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// Exits with 75, so launchd starts a fresh agent, once no session is being
/// served: this process's Core Audio connection is stale, and only a new
/// process gets a working one. A session in another slot runs to its end.
async fn restart_when_idle() -> ! {
    eprintln!(
        "agent: this process's Core Audio connection is stale; exiting so launchd starts a fresh \
         agent once no session is being served"
    );
    while ACTIVE_SESSIONS.load(std::sync::atomic::Ordering::Acquire) > 0 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    std::process::exit(75);
}

#[allow(clippy::too_many_arguments)]
async fn agent_slot(
    kind: arcen_session::agent_relay::DesktopSessionKind,
    service_uid: Option<u32>,
    local_playback: arcen_session::pier_config::LocalPlayback,
    session_policy: arcen_pier_macos::session::SessionPolicy,
    telemetry: arcen_pier_macos::observability::HostTelemetry,
    timezone_redirection: bool,
    admission: std::sync::Arc<arcen_session::session_admission::SessionAdmissionRuntime>,
    waiting_reported: std::rc::Rc<std::cell::Cell<bool>>,
) {
    let socket_path = std::path::Path::new(arcen_pier_macos::relay::AGENT_SOCKET);
    let mut backoff = std::time::Duration::from_secs(1);
    loop {
        if arcen_pier_macos::audio::core_audio_connection_stale() {
            restart_when_idle().await;
        }
        match arcen_pier_macos::relay::park(socket_path, kind, service_uid).await {
            Ok(attached) => {
                let arcen_pier_macos::relay::AttachedDeck {
                    stream,
                    peer,
                    session,
                } = attached;
                waiting_reported.set(false);
                backoff = std::time::Duration::from_secs(1);
                let peer = peer
                    .parse()
                    .unwrap_or_else(|_| std::net::SocketAddr::from(([0, 0, 0, 0], 0)));
                let mut socket = arcen_pier_macos::net::framed(
                    arcen_pier_macos::net::PierStream::Relayed(stream),
                )
                .await;
                let _ = Box::pin(serve_one(
                    &mut socket,
                    peer,
                    None,
                    local_playback,
                    AGENT_CONSOLE_GRACE,
                    &session_policy,
                    &telemetry,
                    timezone_redirection,
                    Some(&admission),
                    Some(AudioChannelRequest {
                        kind,
                        service_uid,
                        session,
                    }),
                ))
                .await;
            }
            Err(error) => {
                // Said once per outage, not once per retry or per slot: the
                // service being down for a restart is ordinary, and a line
                // every second would bury the one that explains it.
                if !waiting_reported.replace(true) {
                    eprintln!("agent: waiting for the Pier service: {error}");
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(std::time::Duration::from_secs(10));
            }
        }
    }
}

#[cfg(test)]
pub(crate) async fn resolve_bind_addr_for_test(
    host: &str,
    port: u16,
) -> Result<std::net::SocketAddr, String> {
    resolve_bind_addr(host, port).await
}

async fn resolve_bind_addr(host: &str, port: u16) -> Result<std::net::SocketAddr, String> {
    let mut addresses = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| format!("resolve QUIC bind host '{host}:{port}': {error}"))?;
    addresses
        .next()
        .ok_or_else(|| format!("resolve QUIC bind host '{host}:{port}': no addresses returned"))
}

/// Returns a telemetry scope naming the peer, and the user when one is known.
fn scope_with_user(
    sid: &arcen_telemetry::CorrelationId,
    user: Option<String>,
    peer: std::net::SocketAddr,
) -> arcen_pier_macos::observability::SessionScope {
    arcen_pier_macos::observability::SessionScope {
        sid: sid.clone(),
        user,
        peer: Some(peer.to_string()),
    }
}

struct SessionAudio {
    lease: Option<SessionAudioLease>,
    delivers: bool,
    /// An abandoned startup whose tap mutes the Mac for this session.
    _startup_mute: Option<StartupMuteCell>,
}

impl SessionAudio {
    /// No lease and no sound.
    const fn silent() -> Self {
        Self {
            lease: None,
            delivers: false,
            _startup_mute: None,
        }
    }
}

static CONSOLE_OWNER_SLOT: arcen_pier_macos::blocking::BlockingSlot =
    arcen_pier_macos::blocking::BlockingSlot::new();

enum SessionAudioLease {
    Capture(arcen_pier_macos::audio::AudioCaptureSession),
    Mute(arcen_pier_macos::audio::SystemAudioTap),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioStartupKind {
    None,
    MuteOnly,
    Capture,
}

static AUDIO_STARTUP_SLOT: arcen_pier_macos::blocking::BlockingSlot =
    arcen_pier_macos::blocking::BlockingSlot::new();

fn audio_startup_kind(
    audio: arcen_media::audio::ResolvedAudioStream,
    local_playback: arcen_session::pier_config::LocalPlayback,
) -> AudioStartupKind {
    match (
        audio.is_enabled()
            && matches!(
                audio.codec,
                Some(
                    arcen_protocol::wire::AudioCodec::Pcm | arcen_protocol::wire::AudioCodec::Opus
                )
            ),
        local_playback.requires_mute(),
    ) {
        (true, _) => AudioStartupKind::Capture,
        (false, true) => AudioStartupKind::MuteOnly,
        (false, false) => AudioStartupKind::None,
    }
}

fn start_capture_with_mute_lease(
    local_playback: arcen_session::pier_config::LocalPlayback,
    startup: &StartupMuteCell,
) -> Result<Option<SessionAudioLease>, arcen_pier_macos::audio::AudioError> {
    let tap = arcen_pier_macos::audio::SystemAudioTap::create(local_playback)?;
    *lock_startup_mute(startup) = StartupMute::Held(tap.mute_evidence());
    match arcen_pier_macos::audio::AudioCaptureSession::start_with_tap(tap) {
        Ok(session) => Ok(Some(SessionAudioLease::Capture(session))),
        Err(error) => {
            eprintln!("host audio recording unavailable, keeping mute lease: {error}");
            Ok(Some(SessionAudioLease::Mute(error.tap)))
        }
    }
}

/// Where an audio startup's mute stands, shared between its worker and the
/// sessions waiting for it or admitted on it.
enum StartupMute {
    /// No tap yet.
    Pending,
    /// The worker holds a tap; this is what it proved.
    Held(arcen_pier_macos::audio::MuteEvidence),
    /// A session stopped waiting while the worker held this tap.
    Abandoned(arcen_pier_macos::audio::MuteEvidence),
    /// The worker of an abandoned startup returned: its lease stays here,
    /// muting the Mac, until the last session admitted on it ends and drops
    /// the cell.
    Kept {
        evidence: arcen_pier_macos::audio::MuteEvidence,
        _lease: SessionAudioLease,
    },
    /// The worker returned to the session that waited for it.
    Finished,
}

type StartupMuteCell = std::sync::Arc<std::sync::Mutex<StartupMute>>;

/// The startup a session stopped waiting for, while it still mutes the Mac:
/// the System Audio Recording prompt blocks inside the capture start, after
/// the tap (and its mute) exist. Weak: the sessions admitted on it own it,
/// and its tap is released when the last of them ends.
static ABANDONED_AUDIO_STARTUP: std::sync::Mutex<
    Option<std::sync::Weak<std::sync::Mutex<StartupMute>>>,
> = std::sync::Mutex::new(None);

fn lock_startup_mute(cell: &StartupMuteCell) -> std::sync::MutexGuard<'_, StartupMute> {
    cell.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What the worker returns: an abandoned startup's lease is kept in the cell
/// instead of being dropped with the channel nobody reads any more, which
/// would unmute the Mac under the sessions admitted on it.
fn hand_over_startup(
    startup: &StartupMuteCell,
    result: Result<Option<SessionAudioLease>, arcen_pier_macos::audio::AudioError>,
) -> Result<Option<SessionAudioLease>, arcen_pier_macos::audio::AudioError> {
    let mut state = lock_startup_mute(startup);
    if let (StartupMute::Abandoned(evidence), Ok(Some(_))) = (&*state, &result) {
        let evidence = *evidence;
        if let Ok(Some(lease)) = result {
            *state = StartupMute::Kept {
                evidence,
                _lease: lease,
            };
        }
        return Ok(None);
    }
    *state = StartupMute::Finished;
    result
}

/// Ends a startup whose worker did not return normally (a panic dropped its
/// tap on the way out), so nothing is admitted on a mute that is gone.
struct StartupUnwound(StartupMuteCell);

impl Drop for StartupUnwound {
    fn drop(&mut self) {
        let mut state = lock_startup_mute(&self.0);
        if matches!(
            *state,
            StartupMute::Pending | StartupMute::Held(_) | StartupMute::Abandoned(_)
        ) {
            *state = StartupMute::Finished;
        }
    }
}

/// Stops waiting for `startup`, returning the mute its worker still holds.
fn abandon_audio_startup(
    startup: &StartupMuteCell,
) -> Option<arcen_pier_macos::audio::MuteEvidence> {
    let evidence = {
        let mut state = lock_startup_mute(startup);
        let StartupMute::Held(evidence) = *state else {
            return None;
        };
        *state = StartupMute::Abandoned(evidence);
        evidence
    };
    *ABANDONED_AUDIO_STARTUP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(std::sync::Arc::downgrade(startup));
    Some(evidence)
}

/// The mute an abandoned startup still holds, and the cell a session admitted
/// on it keeps alive.
fn mute_held_by_abandoned_startup()
-> Option<(arcen_pier_macos::audio::MuteEvidence, StartupMuteCell)> {
    let abandoned = ABANDONED_AUDIO_STARTUP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()?
        .upgrade()?;
    let evidence = match &*lock_startup_mute(&abandoned) {
        StartupMute::Abandoned(evidence) | StartupMute::Kept { evidence, .. } => *evidence,
        StartupMute::Pending | StartupMute::Held(_) | StartupMute::Finished => return None,
    };
    Some((evidence, abandoned))
}

/// Starts the local-playback privacy lease and, when negotiated, host audio.
///
/// The tap is acquired before the recorder is asked to attach to it. Mute is
/// admission policy; recording is a stream capability. Keeping those leases
/// separate prevents a recorder fault from tearing down the already-proven
/// privacy lease and refusing an otherwise usable desktop.
///
/// Audio failing is not the session failing. The Linux host says "continuing
/// without audio" and serves the desktop; so does this one. That includes a
/// capture start blocked on the System Audio Recording prompt once its tap
/// has proved the mute: the session is served without sound (so whoever
/// signed in can see and answer the prompt), and the waiting worker keeps the
/// Mac muted until it returns.
async fn start_session_audio(
    handshake: &arcen_pier_macos::session::Handshake,
    local_playback: arcen_session::pier_config::LocalPlayback,
) -> Result<SessionAudio, String> {
    let kind = audio_startup_kind(handshake.audio, local_playback);
    if handshake.audio.is_enabled()
        && !matches!(
            handshake.audio.codec,
            Some(arcen_protocol::wire::AudioCodec::Pcm | arcen_protocol::wire::AudioCodec::Opus)
        )
    {
        eprintln!(
            "host audio negotiated a codec this Pier cannot encode, continuing without audio"
        );
    }
    if kind == AudioStartupKind::None {
        return Ok(SessionAudio::silent());
    }
    if let Some((evidence, startup_mute)) = mute_held_by_abandoned_startup() {
        mute_policy_allows_session(local_playback, Some(evidence))?;
        eprintln!(
            "host audio is still waiting on the System Audio Recording prompt from an earlier \
             session; serving without sound, and that startup keeps local playback muted"
        );
        return Ok(SessionAudio {
            lease: None,
            delivers: false,
            _startup_mute: Some(startup_mute),
        });
    }

    let startup: StartupMuteCell = std::sync::Arc::new(std::sync::Mutex::new(StartupMute::Pending));
    let worker_startup = std::sync::Arc::clone(&startup);
    let result = tokio::time::timeout(
        AUDIO_START_BUDGET,
        arcen_pier_macos::blocking::run_exclusive(
            "arcen-macos-audio-startup",
            &AUDIO_STARTUP_SLOT,
            audio_startup_failure,
            move || {
                let _unwound = StartupUnwound(std::sync::Arc::clone(&worker_startup));
                let result = match kind {
                    AudioStartupKind::None => Ok(None),
                    AudioStartupKind::MuteOnly => {
                        arcen_pier_macos::audio::SystemAudioTap::create(local_playback)
                            .map(SessionAudioLease::Mute)
                            .map(Some)
                    }
                    AudioStartupKind::Capture => {
                        start_capture_with_mute_lease(local_playback, &worker_startup)
                    }
                };
                hand_over_startup(&worker_startup, result)
            },
        ),
    )
    .await;

    let result = match result {
        Ok(Ok(result)) => Some(result),
        Ok(Err(error)) => return Err(error),
        Err(_) => None,
    };
    match result {
        Some(Ok(Some(SessionAudioLease::Capture(session)))) => {
            mute_policy_allows_session(local_playback, Some(session.mute_evidence()))?;
            let delivers = confirm_audio_delivers(&session).await;
            Ok(SessionAudio {
                lease: Some(SessionAudioLease::Capture(session)),
                delivers,
                _startup_mute: None,
            })
        }
        Some(Ok(Some(SessionAudioLease::Mute(tap)))) => {
            mute_policy_allows_session(local_playback, Some(tap.mute_evidence()))?;
            Ok(SessionAudio {
                lease: Some(SessionAudioLease::Mute(tap)),
                delivers: false,
                _startup_mute: None,
            })
        }
        Some(Ok(None)) => Ok(SessionAudio::silent()),
        Some(Err(error)) => {
            if local_playback.requires_mute() {
                Err(format!(
                    "local playback mute could not be established: {error}"
                ))
            } else {
                eprintln!("host audio unavailable, continuing without it: {error}");
                Ok(SessionAudio::silent())
            }
        }
        None => audio_start_timed_out(local_playback, startup),
    }
}

/// A startup that missed [`AUDIO_START_BUDGET`]: served without sound when
/// its tap already proved the mute (the worker, blocked on the prompt, keeps
/// it), refused when a mute is required and nothing proved it.
fn audio_start_timed_out(
    local_playback: arcen_session::pier_config::LocalPlayback,
    startup: StartupMuteCell,
) -> Result<SessionAudio, String> {
    let message = format!(
        "host audio did not start within {} ms. If this host has never been granted system \
         audio recording, approve \"Arcen Agent Helper\" under System Settings > Privacy & \
         Security > System Audio Recording; the prompt blocks capture until it is answered.",
        AUDIO_START_BUDGET.as_millis(),
    );
    if let Some(evidence) = abandon_audio_startup(&startup)
        && mute_policy_allows_session(local_playback, Some(evidence)).is_ok()
    {
        eprintln!(
            "{message} Serving without sound meanwhile; the waiting startup keeps local playback \
             muted for as long as a session admitted on it runs."
        );
        return Ok(SessionAudio {
            lease: None,
            delivers: false,
            _startup_mute: Some(startup),
        });
    }
    if local_playback.requires_mute() {
        Err(format!(
            "local playback mute could not be established: {message}"
        ))
    } else {
        eprintln!("{message} continuing without audio.");
        Ok(SessionAudio::silent())
    }
}

/// How long each prompt is given before the next one is raised anyway.
const PERMISSION_PROMPT_TURN: std::time::Duration = std::time::Duration::from_secs(120);

/// Asks for every permission the helper needs, one at a time, when it starts.
///
/// Each one used to be raised by whatever first needed it. System audio
/// waited for the first Deck, so its prompt appeared on a Mac nobody was
/// watching while the session was refused. And Screen Recording and
/// Accessibility were asked in the same instant, so the second dialog opened
/// underneath the first and was found later, as a surprise.
///
/// So they are asked in turn, right after install while the person who
/// installed is still at the Mac, each waiting for the previous answer.
/// The order follows what can be observed. The system audio prompt holds the
/// tap until it is answered, and Accessibility reports a grant as soon as it is
/// given. Screen Recording is last because a new grant may only take effect
/// once the helper restarts, so waiting on it proves nothing.
///
/// A prompt nobody answers holds up the next one for at most
/// [`PERMISSION_PROMPT_TURN`].
async fn ask_for_permissions_in_turn(ask_audio: bool) {
    use arcen_pier_macos::permissions;

    if ask_audio {
        // Shares the exclusive slot with a session's audio startup, so it
        // never holds the output device alongside a session's tap. The tap it
        // creates leaves playback audible and is dropped at once.
        let consent = tokio::time::timeout(
            PERMISSION_PROMPT_TURN,
            arcen_pier_macos::blocking::run_exclusive(
                "arcen-macos-audio-consent",
                &AUDIO_STARTUP_SLOT,
                audio_startup_failure,
                arcen_pier_macos::audio::register_capture_consent,
            ),
        )
        .await;
        match consent {
            Ok(Ok(arcen_pier_macos::audio::CaptureConsent::Registered)) => {}
            Ok(Ok(arcen_pier_macos::audio::CaptureConsent::Unavailable)) => eprintln!(
                "No audio tap could be created. Host audio and muting local playback are \
                 unavailable; with audio.local_playback \"muted\" every session will be refused."
            ),
            Ok(Err(error)) => eprintln!("asking for system audio consent: {error}"),
            Err(_) => eprintln!(
                "The System Audio Recording prompt has not been answered; asking for the next \
                 permission meanwhile."
            ),
        }
        if arcen_pier_macos::audio::core_audio_connection_stale() {
            restart_when_idle().await;
        }
    }

    if !permissions::probe().accessibility {
        // Returns at once: the dialog belongs to another process.
        let _ = permissions::request_accessibility_access();
        let deadline = tokio::time::Instant::now() + PERMISSION_PROMPT_TURN;
        while !permissions::probe().accessibility && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        if !permissions::probe().accessibility {
            eprintln!(
                "Accessibility is not granted. Approve \"Arcen Agent Helper\" under \
                 System Settings > Privacy & Security > Accessibility; until then \
                 keyboard injection will not reach applications."
            );
        }
    }

    // Returns at once; the system shows its own dialog.
    if !permissions::probe().screen_recording && !permissions::request_screen_recording() {
        eprintln!(
            "Screen Recording is not granted. Approve \"Arcen Agent Helper\" under \
             System Settings > Privacy & Security > Screen & System Audio Recording; \
             until then this host serves a session with no picture."
        );
    }
}

/// What the Deck is told when audio refuses a session.
///
/// A close frame holds about 120 bytes, so the full explanation stays in the
/// agent log and the Deck gets the part its user can act on.
fn deck_reason_for_audio_refusal(reason: &str) -> &'static str {
    if arcen_pier_macos::audio::core_audio_connection_stale() {
        "The host's sound system restarted; reconnect in a few seconds."
    } else if reason.contains("System Audio Recording") {
        "At the host, allow System Audio Recording for Arcen Agent Helper, then reconnect."
    } else {
        arcen_session::local_playback::DECK_REASON_NOT_MUTED
    }
}

fn audio_startup_failure(failure: arcen_pier_macos::blocking::ExclusiveFailure) -> String {
    match failure {
        arcen_pier_macos::blocking::ExclusiveFailure::Busy => {
            "host audio is still starting, usually because the System Audio Recording prompt on \
             the host has not been answered. Someone at the host must choose Allow for \"Arcen \
             Agent Helper\" (System Settings > Privacy & Security > System Audio Recording). \
             Until then the host cannot mute its own speakers, so it refuses the session."
                .to_owned()
        }
        arcen_pier_macos::blocking::ExclusiveFailure::Spawn(error) => {
            format!("start audio startup worker: {error}")
        }
        arcen_pier_macos::blocking::ExclusiveFailure::Disconnected => {
            "audio startup worker ended without a result".to_owned()
        }
    }
}

fn mute_policy_allows_session(
    local_playback: arcen_session::pier_config::LocalPlayback,
    evidence: Option<arcen_pier_macos::audio::MuteEvidence>,
) -> Result<(), String> {
    arcen_session::local_playback::admit(local_playback, evidence)
        .map_err(|error| error.to_string())
}

/// Describes the stream this session is about to serve, for the record.
///
/// Read from the resolved plan rather than written out as literals. `chroma`
/// was hardcoded to "420", so a colour-fidelity session — the whole point of
/// which is 4:4:4 — recorded itself as 4:2:0, and the colour identity was
/// absent entirely. The shared schema says both Piers populate those fields
/// and explains why: `chroma` and `codec` alone describe a stream that could
/// be eight-bit or ten, BT.709 or PQ, and a host record that cannot state
/// what it encoded cannot be used to check any claim about it.
fn stream_start_fields(
    handshake: &arcen_pier_macos::session::Handshake,
) -> arcen_telemetry::lifecycle_fields::StreamStart<'_> {
    use arcen_media::session_plan::{PlanBitDepth, PlanChroma};
    let plan = &handshake.plan;
    arcen_telemetry::lifecycle_fields::StreamStart {
        encoder: "videotoolbox",
        codec: match handshake.codec {
            arcen_pier_macos::encode::EncoderCodec::H264 => "h264",
            arcen_pier_macos::encode::EncoderCodec::Hevc => "h265",
        },
        chroma: match plan.chroma {
            PlanChroma::Yuv420 => "420",
            PlanChroma::Yuv444 => "444",
        },
        // The size actually being served, which is not the host's own display
        // when one was arranged for this session. A record that says otherwise
        // is a record that disagrees with the picture.
        width: handshake.capture_width,
        height: handshake.capture_height,
        fps: Some(handshake.fps),
        display_refresh_hz: handshake.frame_rate.display_refresh_hz,
        fps_basis: Some(handshake.frame_rate.basis.token()),
        display_backend: None,
        pipeline: handshake
            .active_pipeline
            .as_ref()
            .map(|pipeline| pipeline.token()),
        color: Some(arcen_telemetry::lifecycle_fields::ColorIdentity {
            bit_depth: match plan.bit_depth {
                PlanBitDepth::Eight => "8",
                PlanBitDepth::Ten => "10",
            },
            // This host encodes video-range only; `stream.rs` names it.
            color_range: "limited",
            color_matrix: plan.matrix,
            color_primaries: plan.primaries,
            transfer: plan.transfer,
        }),
    }
}
/// Tells the client what audio this session will actually carry.
///
/// Only the v1 protocol defines this message, so `result()` returning `None`
/// means there is nothing to say and that is not a failure.
///
/// # Errors
///
/// Returns the reason the message could not be serialised or sent.
async fn send_audio_result<S>(
    socket: &mut S,
    stream: arcen_media::audio::ResolvedAudioStream,
) -> Result<(), String>
where
    S: futures_util::Sink<tokio_tungstenite::tungstenite::Message> + Unpin,
    S::Error: std::fmt::Display,
{
    send_audio_result_with_timeout(socket, stream, arcen_pier_macos::stream::write_timeout()).await
}

async fn send_audio_result_with_timeout<S>(
    socket: &mut S,
    stream: arcen_media::audio::ResolvedAudioStream,
    timeout: Duration,
) -> Result<(), String>
where
    S: futures_util::Sink<tokio_tungstenite::tungstenite::Message> + Unpin,
    S::Error: std::fmt::Display,
{
    let Some(result) = stream.result() else {
        return Ok(());
    };
    let json = serde_json::to_string(&result)
        .map_err(|error| format!("serialize audio result: {error}"))?;
    send_result_json(socket, json, timeout)
        .await
        .map_err(|error| format!("send audio result: {error}"))
}

async fn send_result_json<S>(socket: &mut S, json: String, timeout: Duration) -> Result<(), String>
where
    S: futures_util::Sink<tokio_tungstenite::tungstenite::Message> + Unpin,
    S::Error: std::fmt::Display,
{
    arcen_pier_macos::stream::send_message_with_timeout(
        socket,
        tokio_tungstenite::tungstenite::Message::Text(json),
        timeout,
    )
    .await
    .map_err(|error| error.to_string())
}

/// Tells the client what microphone this session will carry.
///
/// Sent whether or not one was asked for, because the message is how a Deck
/// learns the answer is no. This host has no importer, so the answer is always
/// no — and a Deck that is told nothing waits for the microphone it requested
/// until its own media timeout expires.
///
/// # Errors
///
/// Returns the reason the message could not be serialised or sent.
async fn send_microphone_result<S>(
    socket: &mut S,
    stream: arcen_media::audio::ResolvedMicrophoneStream,
) -> Result<(), String>
where
    S: futures_util::Sink<tokio_tungstenite::tungstenite::Message> + Unpin,
    S::Error: std::fmt::Display,
{
    send_microphone_result_with_timeout(socket, stream, arcen_pier_macos::stream::write_timeout())
        .await
}

async fn send_microphone_result_with_timeout<S>(
    socket: &mut S,
    stream: arcen_media::audio::ResolvedMicrophoneStream,
    timeout: Duration,
) -> Result<(), String>
where
    S: futures_util::Sink<tokio_tungstenite::tungstenite::Message> + Unpin,
    S::Error: std::fmt::Display,
{
    let json = serde_json::to_string(&stream.result())
        .map_err(|error| format!("serialize microphone result: {error}"))?;
    send_result_json(socket, json, timeout)
        .await
        .map_err(|error| format!("send microphone result: {error}"))
}

/// Keeps a capture session only once Core Audio has actually called back.
///
/// Starting is not delivering. Measured on a machine with the grant in place
/// and sound playing: the tap created, reported its format, honoured mute and
/// started the device — all five stages — and the callback never ran. A host
/// that treats "started" as "working" tells the Deck audio is enabled and then
/// sends nothing, and the Deck waits out its media timeout for packets that
/// are never coming. Sixty seconds of silence is worse than an honest
/// `CaptureUnavailable` in one.
async fn confirm_audio_delivers(session: &arcen_pier_macos::audio::AudioCaptureSession) -> bool {
    let deadline = tokio::time::Instant::now() + AUDIO_DELIVERY_BUDGET;
    while tokio::time::Instant::now() < deadline {
        if session.has_delivered() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    // Silence is not failure. A tap on a device nobody is playing to delivers
    // nothing because there is nothing to deliver, and treating that as a
    // broken tap meant sound only ever worked when something happened to be
    // playing at the moment of connection — connect to a quiet desktop, start
    // a video afterwards, and the session had already given up.
    if !arcen_pier_macos::audio::output_is_running() {
        tracing::info!(
            target: arcen_telemetry::names::target::MEDIA,
            "nothing is playing yet, so the tap has delivered nothing; keeping audio"
        );
        return true;
    }
    eprintln!(
        "host audio started but delivered nothing within {} ms, continuing without it. The tap \
         is running and Core Audio is not calling back: check that \"Arcen Agent Helper\" is \
         enabled under System Settings > Privacy & Security > System Audio Recording, which is \
         a different list from Screen & System Audio Recording.",
        AUDIO_DELIVERY_BUDGET.as_millis()
    );
    false
}

/// How long a session waits for the first audio callback before giving up.
///
/// A working tap on a busy device calls back within one packet interval. This
/// is generous by comparison and still short enough that a desktop is not held
/// waiting for sound that is not coming.
const AUDIO_DELIVERY_BUDGET: std::time::Duration = std::time::Duration::from_millis(400);

/// How long a session waits for host audio before serving the desktop without
/// it.
///
/// A tap on a machine that has already been granted consent creates in well
/// under this. The budget exists for the machine that has not: the prompt
/// blocks inside Apple's call, and a picture now is worth more than sound that
/// may never be permitted.
const AUDIO_START_BUDGET: std::time::Duration = std::time::Duration::from_millis(1_500);

/// Records a handshake or authentication failure.
///
/// The reason is classified, never quoted: an authentication log that repeats
/// what was typed becomes a credential store the first time someone puts a
/// password in the username box.
fn report_handshake_failure(
    telemetry: &arcen_pier_macos::observability::HostTelemetry,
    error: &arcen_pier_macos::session::HandshakeError,
    session_id: &arcen_telemetry::CorrelationId,
    peer: std::net::SocketAddr,
) {
    eprintln!("handshake failed: {error}");
    let (stage, reason_class) = handshake_failure_class(error);
    emit_session_event(
        telemetry,
        arcen_telemetry::LifecycleEventKind::SessionAuthFail,
        &scope_with_user(session_id, None, peer),
        arcen_telemetry::lifecycle_fields::session_auth_fail("pam", stage, reason_class),
        arcen_telemetry::names::target::AUTH,
        "session auth fail",
    );
}

/// The `stage` and `reason_class` of a failed handshake's `SESSION_AUTH_FAIL`.
fn handshake_failure_class(
    error: &arcen_pier_macos::session::HandshakeError,
) -> (&'static str, &'static str) {
    use arcen_pier_macos::session::HandshakeError;
    match error {
        HandshakeError::SetupRefused(refusal) => (refusal.log_stage(), refusal.log_reason_class()),
        HandshakeError::Rejected(_) => ("authenticate", "rejected"),
        HandshakeError::ApplicationHandshakeTimeout => {
            ("application_handshake_timeout", "rejected")
        }
        _ => ("handshake", "rejected"),
    }
}

/// Returns the machine's sound, reporting whether it came back.
///
/// A Mac left silent after a session, with nobody told why, is the worst
/// outcome here, so a failed release is surfaced rather than swallowed.
fn release_audio(lease: Option<SessionAudioLease>) -> bool {
    let Some(mut lease) = lease else {
        return true;
    };
    let result = match &mut lease {
        SessionAudioLease::Capture(session) => session.stop(),
        SessionAudioLease::Mute(tap) => tap.release(),
    };
    if let Err(error) = result {
        eprintln!("local audio was not restored: {error}");
        return false;
    }
    true
}

/// Emits a session record, doing nothing when its fields could not be built.
///
/// Field construction returns `Option` because the shared schema rejects a
/// value it does not declare. Folding that into the emit call keeps the
/// decision in one place instead of repeating an `if let` at every site, and
/// keeps a telemetry failure from reading like a session failure.
fn emit_session_event(
    telemetry: &arcen_pier_macos::observability::HostTelemetry,
    kind: arcen_telemetry::LifecycleEventKind,
    scope: &arcen_pier_macos::observability::SessionScope,
    fields: Option<arcen_telemetry::StructuredFields>,
    target: &str,
    message: &str,
) {
    if let Some(fields) = fields {
        telemetry.emit(kind, scope, fields, target, message);
    }
}

/// Serves one accepted connection, returning whether it failed.
///
/// Extracted from the accept loop so that the loop reads as accept, serve,
/// decide-whether-to-continue, and so the telemetry around a session sits
/// beside the session rather than between the accept and the next accept.
#[allow(clippy::too_many_lines)]
/// Every live session's shared Pier lifecycle in this agent. Steps are
/// reported where the agent already emits its session lifecycle telemetry;
/// ordering and the readiness-evidence rule are the shared crate's.
static LIFECYCLES: arcen_session::host_lifecycle::SessionLifecycles =
    arcen_session::host_lifecycle::SessionLifecycles::new();

fn log_lifecycle(
    key: &str,
    step: &'static str,
    report: arcen_session::host_lifecycle::LifecycleReport,
) {
    match report.to {
        Ok(state) => tracing::info!(
            target: arcen_telemetry::names::target::SESSION,
            session = key,
            step,
            from = report.from.map(|state| state.token()),
            state = state.token(),
            "Pier lifecycle"
        ),
        Err(error) => tracing::warn!(
            target: arcen_telemetry::names::target::SESSION,
            session = key,
            step,
            from = report.from.map(|state| state.token()),
            %error,
            "Pier lifecycle step refused"
        ),
    }
}

/// Ends a session's lifecycle however `serve_one` leaves: refused after
/// authentication, failed, or served.
struct LifecycleEnd(String);

impl Drop for LifecycleEnd {
    fn drop(&mut self) {
        log_lifecycle(&self.0, "ended", LIFECYCLES.ended(&self.0));
    }
}

fn apply_session_timezone(
    running_in_agent: bool,
    login_window: bool,
    enabled: bool,
    timezone: Option<&str>,
    session_id: &arcen_telemetry::CorrelationId,
) -> Option<arcen_pier_macos::timezone::SessionTimezoneLease> {
    if !enabled {
        return None;
    }
    if !running_in_agent {
        tracing::warn!(
            target: arcen_telemetry::names::target::SESSION,
            sid = %session_id,
            "macOS timezone redirection is available only in the per-session Agent Helper"
        );
        return None;
    }
    match arcen_pier_macos::timezone::begin_session_timezone(enabled, timezone, login_window) {
        arcen_pier_macos::timezone::SessionTimezoneOutcome::Applied(lease) => {
            let target = lease.target().to_owned();
            tracing::info!(
                target: arcen_telemetry::names::target::SESSION,
                sid = %session_id,
                to_timezone = %target,
                scope = "launchd_gui_session",
                "session timezone redirected for newly launched macOS apps"
            );
            Some(lease)
        }
        arcen_pier_macos::timezone::SessionTimezoneOutcome::UnsupportedAtLoginWindow => {
            tracing::warn!(
                target: arcen_telemetry::names::target::SESSION,
                sid = %session_id,
                "timezone redirection is unsupported at the macOS login window"
            );
            None
        }
        arcen_pier_macos::timezone::SessionTimezoneOutcome::UserDefinedTimezone => {
            tracing::info!(
                target: arcen_telemetry::names::target::SESSION,
                sid = %session_id,
                "timezone redirection skipped because the user already defines TZ"
            );
            None
        }
        arcen_pier_macos::timezone::SessionTimezoneOutcome::UnsupportedNotSessionAgent => {
            tracing::warn!(
                target: arcen_telemetry::names::target::SESSION,
                sid = %session_id,
                "timezone redirection is unsupported outside the launchd-managed Agent Helper"
            );
            None
        }
        arcen_pier_macos::timezone::SessionTimezoneOutcome::ShuttingDown => {
            tracing::warn!(
                target: arcen_telemetry::names::target::SESSION,
                sid = %session_id,
                "timezone redirection refused because the Agent Helper is shutting down"
            );
            None
        }
        arcen_pier_macos::timezone::SessionTimezoneOutcome::Absent => {
            tracing::warn!(
                target: arcen_telemetry::names::target::SESSION,
                sid = %session_id,
                "timezone redirection enabled but client timezone is absent; continuing without redirection"
            );
            None
        }
        arcen_pier_macos::timezone::SessionTimezoneOutcome::Invalid(message) => {
            tracing::warn!(
                target: arcen_telemetry::names::target::SESSION,
                sid = %session_id,
                timezone = %message,
                "authenticated client timezone is invalid; redirection skipped"
            );
            None
        }
        arcen_pier_macos::timezone::SessionTimezoneOutcome::Unsupported(timezone) => {
            tracing::warn!(
                target: arcen_telemetry::names::target::SESSION,
                sid = %session_id,
                %timezone,
                "authenticated client timezone is unavailable on this Mac; redirection skipped"
            );
            None
        }
        arcen_pier_macos::timezone::SessionTimezoneOutcome::Warning(message) => {
            tracing::warn!(
                target: arcen_telemetry::names::target::SESSION,
                sid = %session_id,
                error = %message,
                "timezone redirection failed; authenticated streaming continues"
            );
            None
        }
        arcen_pier_macos::timezone::SessionTimezoneOutcome::Disabled => None,
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn serve_one(
    socket: &mut arcen_pier_macos::net::PierSocket,
    peer: std::net::SocketAddr,
    stream_frames: Option<u64>,
    local_playback: arcen_session::pier_config::LocalPlayback,
    first_login_timeout: std::time::Duration,
    session_policy: &arcen_pier_macos::session::SessionPolicy,
    telemetry: &arcen_pier_macos::observability::HostTelemetry,
    timezone_redirection: bool,
    admission: Option<&std::sync::Arc<arcen_session::session_admission::SessionAdmissionRuntime>>,
    audio_channel_request: Option<AudioChannelRequest>,
) -> SessionOutcome {
    let mut stream_failed = false;
    println!("client connected from {peer}");
    let mut session_policy = session_policy.clone();
    session_policy.microphone_backend_available =
        arcen_pier_macos::microphone_input::backend_available();
    wake_displays().await;
    // One correlation id per accepted connection, so the auth result, the
    // stream start and the session end can be gathered from a log that
    // holds many sessions at once.
    let session_id = arcen_telemetry::CorrelationId::from_uuid_v4_bytes(
        arcen_pier_macos::observability::random_correlation_bytes(),
    );
    let session_started = std::time::Instant::now();
    match arcen_pier_macos::session::perform_for_peer(socket, peer.ip(), &session_policy, admission)
        .await
    {
        Ok(mut handshake) => {
            println!(
                "handshake complete: offered {}x{}",
                handshake.capture_width, handshake.capture_height
            );
            emit_session_event(
                telemetry,
                arcen_telemetry::LifecycleEventKind::SessionAuthOk,
                &scope_with_user(&session_id, Some(handshake.user.clone()), peer),
                arcen_telemetry::lifecycle_fields::session_auth_ok("pam", "console_user", None),
                arcen_telemetry::names::target::AUTH,
                "session auth ok",
            );
            let _active = ActiveSession::begin();
            // Never asleep under a Deck that is only watching.
            let awake = arcen_pier_macos::power::SleepAssertion::hold("Arcen Deck session");
            if !awake.is_held() {
                tracing::warn!(
                    target: arcen_telemetry::names::target::SESSION,
                    "the power manager refused to keep this Mac awake for the session"
                );
            }
            let _timezone_lease = apply_session_timezone(
                audio_channel_request.is_some(),
                session_policy.login_window,
                timezone_redirection,
                handshake.authenticated_timezone.as_deref(),
                &session_id,
            );
            let lifecycle = LifecycleEnd(session_id.to_string());
            log_lifecycle(
                &lifecycle.0,
                "authenticated",
                LIFECYCLES.authenticated(&lifecycle.0),
            );
            // The login window is served to whoever authenticated, so that
            // they can sign in; only a user's desktop must belong to them.
            if !session_policy.login_window {
                if let Err(reason) = admit_session(&handshake.user, first_login_timeout).await {
                    eprintln!("refusing the session: {reason}");
                    let _ = arcen_pier_macos::net::refuse_with_reason(socket, &reason).await;
                    return SessionOutcome::Refused;
                }
            }
            // Before anything of the session is shown: if this agent dies
            // while the Deck is attached, the session is still locked
            // (GAP-048).
            let lock_guard = arm_lock_guard(&session_policy).await;
            // The desk goes private before anything of the session is shown
            // (ADR 0025 §5), or the session is refused.
            let mut desk_privacy = match arm_desk_privacy(&session_policy).await {
                Ok(privacy) => privacy,
                Err(refusal) => {
                    tracing::error!(
                        target: arcen_telemetry::names::target::SESSION,
                        reason = refusal.token(),
                        "refusing the session: the desk could not be made private"
                    );
                    let _ = arcen_pier_macos::net::refuse_with_reason(
                        socket,
                        arcen_session::console_session::ConsoleRefusal::PrivacyUnavailable
                            .deck_reason(),
                    )
                    .await;
                    return SessionOutcome::Refused;
                }
            };

            let mut session_audio = match start_session_audio(&handshake, local_playback).await {
                Ok(audio) => audio,
                Err(reason) => {
                    eprintln!("refusing the session: {reason}");
                    // Without this the Deck sees only a reset connection.
                    let _ = arcen_pier_macos::net::refuse_with_reason(
                        socket,
                        deck_reason_for_audio_refusal(&reason),
                    )
                    .await;
                    return SessionOutcome::Refused;
                }
            };

            // The Deck is told what it is actually getting, not what was
            // negotiated. A client that negotiated audio and is never told
            // otherwise waits for it: measured here as a session that decoded
            // its first video frame and then sat for sixty seconds before
            // giving up with "timed out waiting for media". The Linux Pier has
            // always sent this result when capture cannot start, and macOS
            // sent nothing at all.
            let delivered = if session_audio.delivers {
                handshake.audio
            } else {
                arcen_media::audio::ResolvedAudioStream::disabled(
                    handshake.audio.mode,
                    arcen_protocol::messages::AudioStreamReason::CaptureUnavailable,
                )
            };
            if let Err(error) = send_audio_result(socket, delivered).await {
                eprintln!("ending session because the audio result was not delivered: {error}");
                return SessionOutcome::Failed;
            }
            // Audio never shares the session socket with video on the way
            // to the service, which puts it on the Deck's audio lane.
            let audio_channel = match audio_channel_request {
                Some(request)
                    if std::env::var("ARCEN_AUDIO_SIDE_CHANNEL").as_deref() != Ok("0") =>
                {
                    match arcen_pier_macos::relay::open_audio_channel(
                        std::path::Path::new(arcen_pier_macos::relay::AGENT_SOCKET),
                        request.kind,
                        request.service_uid,
                        request.session,
                    )
                    .await
                    {
                        Ok(channel) => Some(channel),
                        Err(error) => {
                            eprintln!(
                                "audio side channel unavailable; audio shares the session socket to the service: {error}"
                            );
                            None
                        }
                    }
                }
                _ => None,
            };
            let mut session_microphone = if handshake.microphone.is_enabled() {
                match arcen_pier_macos::microphone_input::MicrophoneSessionBinding::new(
                    handshake.microphone.generation,
                )
                .and_then(|binding| {
                    arcen_pier_macos::microphone_input::NativeMicrophoneDevice::open(&binding)
                        .map(|device| (binding, device))
                }) {
                    Ok((binding, device)) => {
                        match arcen_pier_macos::microphone_input::MicrophoneIngress::new(
                            binding,
                            handshake.microphone,
                            device,
                        ) {
                            Ok(ingress) => Some(ingress.with_session_log_id(session_id.clone())),
                            Err(error) => {
                                eprintln!("microphone disabled after negotiation: {error:?}");
                                None
                            }
                        }
                    }
                    Err(error) => {
                        eprintln!("microphone device unavailable after negotiation: {error:?}");
                        None
                    }
                }
            } else {
                None
            };
            let delivered_microphone =
                if handshake.microphone.is_enabled() && session_microphone.is_none() {
                    arcen_media::audio::ResolvedMicrophoneStream::disabled(
                        handshake.microphone.generation,
                        arcen_protocol::messages::MicrophoneStreamReason::BackendUnavailable,
                    )
                } else {
                    handshake.microphone
                };
            if let Err(error) = send_microphone_result(socket, delivered_microphone).await {
                eprintln!(
                    "ending session because the microphone result was not delivered: {error}"
                );
                return SessionOutcome::Failed;
            }

            emit_session_event(
                telemetry,
                arcen_telemetry::LifecycleEventKind::SessionStreamStart,
                &scope_with_user(&session_id, Some(handshake.user.clone()), peer),
                arcen_telemetry::lifecycle_fields::session_stream_start(stream_start_fields(
                    &handshake,
                )),
                arcen_telemetry::names::target::MEDIA,
                "session stream start",
            );
            log_lifecycle(
                &lifecycle.0,
                "stream_started",
                LIFECYCLES.stream_started(
                    &lifecycle.0,
                    arcen_session::host_lifecycle::NativeReadinessEvidence {
                        // PAM authenticated this user for this console.
                        session_identity: !handshake.user.is_empty(),
                        // The capture display resolved a real geometry.
                        outputs_verified: handshake.capture_width > 0
                            && handshake.capture_height > 0,
                        // The handshake resolved an encoder contract.
                        media_verified: handshake.fps > 0,
                        // The input session was negotiated in the handshake.
                        input_verified: true,
                    },
                ),
            );

            let privacy_lost = desk_privacy
                .as_ref()
                .map(arcen_pier_macos::desk_privacy::DeskPrivacy::lost);
            // A desk that can no longer be kept private ends the session
            // (`DeskPrivacy::lost`); the hand-back below locks it first.
            let outcome = serve_watched(
                |sent_frames| {
                    serve_desktop(
                        socket,
                        &handshake,
                        stream_frames,
                        telemetry.clone(),
                        session_id.clone(),
                        session_microphone.as_mut(),
                        // The capture this session negotiated, not `None`. The lease
                        // was started, announced to the Deck as enabled, and released
                        // at the end — and never handed to the streamer, so nothing
                        // ever drained it. The host promised audio and then held the
                        // only thing that could produce it.
                        if session_audio.delivers {
                            match session_audio.lease.as_mut() {
                                Some(SessionAudioLease::Capture(session)) => Some(session),
                                _ => None,
                            }
                        } else {
                            None
                        },
                        audio_channel,
                        sent_frames,
                    )
                },
                privacy_lost.as_deref(),
            )
            .await;
            if matches!(outcome, Watched::WatchEnded { .. }) {
                // Why the desk's privacy ended the session, as the shared
                // verdict says: a console another account (or the login
                // window) took over is not a privacy failure.
                let reason = deck_reason_for_privacy_end(
                    desk_privacy
                        .as_ref()
                        .and_then(arcen_pier_macos::desk_privacy::DeskPrivacy::end_verdict),
                    session_policy.serving_uid,
                );
                let _ = arcen_pier_macos::net::refuse_with_reason(socket, &reason).await;
            }
            if let Some(microphone) = session_microphone.as_mut() {
                let _ = microphone.shutdown_wait("session_end").await;
            }
            // How the stream ended, decided once (shared `StreamEnd`): what
            // the Deck is told, the `SESSION_END` class and the departure
            // the hand-back locks for. `None`: the desk watch ended it and
            // has told the Deck already.
            let (stream_end, reason_class, frames_sent) = match &outcome {
                Watched::WatchEnded { frames_sent } => {
                    stream_failed = true;
                    let end = arcen_session::console_session::desk_privacy::WatchSessionEnd::new(
                        desk_privacy
                            .as_ref()
                            .and_then(arcen_pier_macos::desk_privacy::DeskPrivacy::end_verdict),
                        *frames_sent,
                    );
                    (None, end.reason_class, end.frames_sent)
                }
                Watched::Stream(Ok(frames)) => (Some(StreamEnd::Completed), "completed", *frames),
                Watched::Stream(Err(ended)) => {
                    eprintln!("stream ended: {ended}");
                    // The real count, not zero. A client that vanishes after
                    // an hour is the ordinary way a remote session ends, and
                    // reporting nothing sent made it indistinguishable from a
                    // session that never produced a picture at all.
                    let end = StreamEnd::classify(ended.error.end_cause(), ended.stats.frames_sent);
                    if end == StreamEnd::CaptureStopped {
                        tracing::warn!(
                            target: arcen_telemetry::names::target::SESSION,
                            reason_class = end.reason_class(),
                            frames_sent = ended.stats.frames_sent,
                            detail = %ended.error,
                            "a person stopped screen capture with the system's Stop Sharing; the \
                             Deck is told and the session is locked"
                        );
                    }
                    (Some(end), end.reason_class(), ended.stats.frames_sent)
                }
            };
            if stream_end.is_some_and(StreamEnd::is_failure) {
                stream_failed = true;
            }
            // Frames sent is the field that separates "a client connected"
            // from "a desktop arrived"; without it a session that produced
            // nothing reads exactly like one that worked.
            emit_session_event(
                telemetry,
                arcen_telemetry::LifecycleEventKind::SessionEnd,
                &scope_with_user(&session_id, Some(handshake.user.clone()), peer),
                arcen_telemetry::lifecycle_fields::session_end(
                    reason_class,
                    session_started
                        .elapsed()
                        .as_millis()
                        .try_into()
                        .unwrap_or(u64::MAX),
                    frames_sent,
                ),
                arcen_telemetry::names::target::SESSION,
                "session end",
            );
            let mut hand_back = MacAttachmentEnd {
                socket,
                serving_uid: session_policy.serving_uid,
                desk_privacy: &mut desk_privacy,
                handshake: &mut handshake,
                audio: session_audio.lease.take(),
                lock_guard,
                audio_failed: false,
            };
            end_attachment(&mut hand_back, stream_end).await;
            if hand_back.audio_failed {
                stream_failed = true;
            }
            if stream_failed {
                SessionOutcome::Failed
            } else {
                SessionOutcome::Served
            }
        }
        Err(error) => {
            report_handshake_failure(telemetry, &error, &session_id, peer);
            SessionOutcome::Refused
        }
    }
}

/// Wakes sleeping displays before the handshake reads the inventory: a
/// sleeping display is not an active one, and a Mac whose screen had gone
/// dark refused every Deck with "inventory is empty".
///
/// Only when no display is awake. This runs before any password is checked,
/// and declaring activity over an awake display would let any peer that
/// connects postpone its idle sleep, and with it a "require password after
/// the display turns off" lock, indefinitely.
async fn wake_displays() {
    let asleep = || arcen_pier_macos::displays::probe().is_ok_and(|displays| displays.is_empty());
    if !asleep() || !arcen_pier_macos::power::declare_user_activity() {
        return;
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while asleep() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

static LOCK_GUARD_SLOT: arcen_pier_macos::blocking::BlockingSlot =
    arcen_pier_macos::blocking::BlockingSlot::new();

/// Records a signed-in user's attachment and starts its lock guard
/// (`lock_guard`): never at the login window (nobody is signed in) nor in
/// `serve`.
async fn arm_lock_guard(
    policy: &arcen_pier_macos::session::SessionPolicy,
) -> Option<arcen_pier_macos::lock_guard::LockGuard> {
    let uid = policy.serving_uid.filter(|_| !policy.login_window)?;
    let private = policy.desk_privacy;
    match arcen_pier_macos::blocking::run_exclusive(
        "arcen-macos-lock-guard",
        &LOCK_GUARD_SLOT,
        |failure| format!("{failure:?}"),
        move || arcen_pier_macos::lock_guard::LockGuard::arm(uid, private),
    )
    .await
    {
        Ok(guard) => Some(guard),
        Err(error) => {
            tracing::error!(
                target: arcen_telemetry::names::target::SESSION,
                %error,
                "the lock guard could not be armed"
            );
            None
        }
    }
}

static DESK_PRIVACY_SLOT: arcen_pier_macos::blocking::BlockingSlot =
    arcen_pier_macos::blocking::BlockingSlot::new();

/// Makes the desk private for a signed-in user's session unless the
/// administrator turned it off (`privacy.desk = "off"`): never at the login
/// window (nothing private is shown, and its only input is virtual HID,
/// which an event filter cannot tell from the desk's) nor in `serve`.
async fn arm_desk_privacy(
    policy: &arcen_pier_macos::session::SessionPolicy,
) -> Result<
    Option<arcen_pier_macos::desk_privacy::DeskPrivacy>,
    arcen_session::console_session::desk_privacy::DeskPrivacyRefusal,
> {
    use arcen_session::console_session::desk_privacy::{
        DeckInputRoute, DeskPrivacyRefusal, admits_input_route,
    };
    let Some(uid) = policy
        .serving_uid
        .filter(|_| policy.desk_privacy && !policy.login_window)
    else {
        return Ok(None);
    };
    admits_input_route(match arcen_pier_macos::input::input_backend() {
        arcen_pier_macos::input::InputBackend::CoreGraphics => DeckInputRoute::PostedEvents,
        arcen_pier_macos::input::InputBackend::VirtualKeyboard(_)
        | arcen_pier_macos::input::InputBackend::VirtualHidOnly(_) => {
            DeckInputRoute::VirtualHidDevices
        }
    })?;
    let privacy = arcen_pier_macos::blocking::run_exclusive(
        "arcen-macos-desk-privacy",
        &DESK_PRIVACY_SLOT,
        |_| DeskPrivacyRefusal::InputFilterUnavailable,
        move || arcen_pier_macos::desk_privacy::DeskPrivacy::arm(uid),
    )
    .await??;
    tracing::info!(
        target: arcen_telemetry::names::target::SESSION,
        "the desk is private while the Deck is attached"
    );
    Ok(Some(privacy))
}

/// What the Deck is told when the desk's privacy ends its session: the
/// shared [`WatchVerdict::deck_refusal`] for who holds the console now,
/// naming another account the way a refused connection does
/// (`console::other_user_refusal`).
///
/// [`WatchVerdict::deck_refusal`]: arcen_session::console_session::desk_privacy::WatchVerdict::deck_refusal
fn deck_reason_for_privacy_end(
    verdict: Option<arcen_session::console_session::desk_privacy::WatchVerdict>,
    serving_uid: Option<u32>,
) -> String {
    use arcen_session::console_session::ConsoleRefusal;
    use arcen_session::console_session::desk_privacy::WatchVerdict;
    let holder = arcen_pier_macos::console::console_holder();
    let refusal = match (verdict, serving_uid) {
        (Some(verdict), Some(uid)) => verdict.deck_refusal(uid, holder),
        _ => None,
    }
    .unwrap_or(ConsoleRefusal::PrivacyUnavailable);
    tracing::warn!(
        target: arcen_telemetry::names::target::SESSION,
        reason = refusal.token(),
        console_left = verdict == Some(WatchVerdict::ConsoleLeft),
        "the desk watch ends the Deck's session"
    );
    if refusal == ConsoleRefusal::OtherUser {
        arcen_pier_macos::console::other_user_refusal(
            arcen_pier_macos::console::console_owner().as_deref(),
        )
    } else {
        refusal.deck_reason().to_owned()
    }
}

/// How a stream under the desk watch ended.
#[derive(Debug)]
enum Watched<T> {
    /// The stream ended by itself.
    Stream(T),
    /// The desk watch ended the session and dropped the stream, and its
    /// statistics with it; `frames_sent` is the tally the stream kept.
    WatchEnded { frames_sent: u64 },
}

/// Runs the stream `serve` builds around this session's frame tally until
/// it ends, or until `privacy_lost` is set. The tally is made here and read
/// here after the stream is dropped, so a watch-ended session reports what
/// its own stream sent.
async fn serve_watched<T, F>(
    serve: impl FnOnce(arcen_telemetry::SentFrames) -> F,
    privacy_lost: Option<&std::sync::atomic::AtomicBool>,
) -> Watched<T>
where
    F: std::future::Future<Output = T>,
{
    let sent_frames = arcen_telemetry::SentFrames::default();
    let served = serve(sent_frames.clone());
    match privacy_lost {
        None => Watched::Stream(served.await),
        Some(lost) => tokio::select! {
            outcome = served => Watched::Stream(outcome),
            () = privacy_lost_wait(lost) => Watched::WatchEnded {
                frames_sent: sent_frames.count(),
            },
        },
    }
}

/// Resolves once the desk's privacy is lost.
async fn privacy_lost_wait(lost: &std::sync::atomic::AtomicBool) {
    while !lost.load(std::sync::atomic::Ordering::Acquire) {
        tokio::time::sleep(arcen_session::console_session::desk_privacy::DISPLAY_CHECK).await;
    }
}

/// How often a wait for the lock screen looks for an authenticated Deck
/// that asked for the session slot.
const NEW_DECK_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// Waits for the lock screen after a refused lock (or, on a private desk,
/// a console reading that was not this account's), deciding each step with
/// the shared `lock_wait::LockWait`: asked again every `LOCK_RETRY` for at
/// most `UNLOCKED_GRACE`; a console another account holds is left after
/// consecutive readings; a desk that was not private stops waiting when
/// `new_deck` says an authenticated Deck wants the slot. Every ending that
/// leaves the desk unlocked is logged as an error. `None` for a hand run
/// with no account (`serve`).
async fn await_lock_screen(
    mut locked: LockOutcome,
    serving_uid: Option<u32>,
    departure: arcen_session::console_session::Departure,
    private: bool,
    new_deck: impl Fn() -> bool,
) -> Option<arcen_session::console_session::lock_wait::LockWaitEnd> {
    use arcen_session::console_session::lock_wait::{LockWait, LockWaitEnd, LockWaitVerdict};
    serving_uid?;
    let started = std::time::Instant::now();
    let mut wait = LockWait::new(private);
    let end = 'wait: loop {
        let pause = match wait.next(locked, started.elapsed()) {
            LockWaitVerdict::Done(end) => break end,
            LockWaitVerdict::RetryAfter(pause) => pause,
        };
        let until = tokio::time::Instant::now() + pause;
        loop {
            if new_deck()
                && let Some(end) = wait.on_new_deck()
            {
                break 'wait end;
            }
            let now = tokio::time::Instant::now();
            if now >= until {
                break;
            }
            tokio::time::sleep(NEW_DECK_POLL.min(until - now)).await;
        }
        locked = lock_on_departure(serving_uid, departure).await;
    };
    match end {
        LockWaitEnd::GraceExpired => tracing::error!(
            target: arcen_telemetry::names::target::SESSION,
            departure = departure.token(),
            "the session would not lock after its Deck left: this Mac's desk is unlocked and \
             signed in. Lock it at the desk (Control-Command-Q) and check that its lock \
             screen is not turned off by policy"
        ),
        LockWaitEnd::YieldedToNewDeck => tracing::error!(
            target: arcen_telemetry::names::target::SESSION,
            departure = departure.token(),
            "the session had not locked when a new Deck signed in; the wait ends so that Deck \
             is served, and this Mac's desk is unlocked and signed in. Lock it at the desk \
             (Control-Command-Q) and check that its lock screen is not turned off by policy"
        ),
        LockWaitEnd::Locked | LockWaitEnd::ConsoleLeft => {}
    }
    Some(end)
}

/// What ending a Deck's attachment does to this Mac, one step at a time.
/// The order is [`end_attachment`]'s; the steps are [`MacAttachmentEnd`]'s,
/// and a test's.
trait AttachmentEnd {
    /// Sends the Deck a close reason.
    async fn tell_deck(&mut self, reason: &str);
    /// Locks the session for `departure` and proves it.
    async fn lock(&mut self, departure: arcen_session::console_session::Departure) -> LockOutcome;
    /// Keeps asking a session that refused to lock.
    async fn await_lock_screen(
        &mut self,
        locked: LockOutcome,
        departure: arcen_session::console_session::Departure,
    );
    /// Gives the desk its displays back and drops the session's own.
    fn restore_outputs(&mut self);
    /// Releases the session's sound.
    fn restore_audio(&mut self);
    /// Gives the desk its input back.
    fn release_input(&mut self);
    /// The agent handed the session back itself: the lock guard stands down.
    fn release_guard(&mut self);
}

/// Ends a Deck's attachment the one way every ending takes (ADR 0025:
/// disconnect locks, never ends), in the shared order
/// (`stream_end::leave_steps`): the Deck told why when it does not know,
/// the hand-back with the lock first, the guard released last. `end` is
/// `None` when the desk watch ended the stream (it told the Deck already).
async fn end_attachment(
    host: &mut impl AttachmentEnd,
    end: Option<arcen_session::console_session::stream_end::StreamEnd>,
) -> LockOutcome {
    use arcen_session::console_session::DetachStep;
    use arcen_session::console_session::stream_end::{LeaveStep, leave_departure, leave_steps};
    let departure = leave_departure(end);
    let mut locked = LockOutcome::NotOurs;
    for step in leave_steps(end, &arcen_session::console_session::DETACH_STEPS) {
        match step {
            LeaveStep::TellDeck(reason) => host.tell_deck(reason).await,
            LeaveStep::Detach(DetachStep::LockSession) => locked = host.lock(departure).await,
            LeaveStep::Detach(DetachStep::AwaitLockScreen) => {
                host.await_lock_screen(locked, departure).await;
            }
            LeaveStep::Detach(DetachStep::RestoreOutputs) => host.restore_outputs(),
            LeaveStep::Detach(DetachStep::RestoreAudio) => host.restore_audio(),
            LeaveStep::Detach(DetachStep::ReleaseInput) => host.release_input(),
            LeaveStep::ReleaseGuard => host.release_guard(),
        }
    }
    locked
}

use arcen_session::console_session::stream_end::StreamEnd;

/// This Mac's hand-back of one attachment.
struct MacAttachmentEnd<'a> {
    socket: &'a mut arcen_pier_macos::net::PierSocket,
    serving_uid: Option<u32>,
    desk_privacy: &'a mut Option<arcen_pier_macos::desk_privacy::DeskPrivacy>,
    handshake: &'a mut arcen_pier_macos::session::Handshake,
    audio: Option<SessionAudioLease>,
    lock_guard: Option<arcen_pier_macos::lock_guard::LockGuard>,
    /// The sound could not be given back.
    audio_failed: bool,
}

impl AttachmentEnd for MacAttachmentEnd<'_> {
    async fn tell_deck(&mut self, reason: &str) {
        if let Err(error) = arcen_pier_macos::net::refuse_with_reason(self.socket, reason).await {
            eprintln!("the Deck was not told why its session ended: {error}");
        }
    }

    async fn lock(&mut self, departure: arcen_session::console_session::Departure) -> LockOutcome {
        lock_on_departure(self.serving_uid, departure).await
    }

    async fn await_lock_screen(
        &mut self,
        locked: LockOutcome,
        departure: arcen_session::console_session::Departure,
    ) {
        let private = self.desk_privacy.is_some();
        if !arcen_session::console_session::lock_wait::waits_for_lock(private, locked) {
            return;
        }
        // A desk that was not private shows the session anyway: it gives
        // the host's slot up to an authenticated Deck rather than holding it
        // for the grace. A private desk keeps it until it locks.
        let admission = self.handshake.admission.as_ref();
        let offered = !private && admission.is_some_and(|admission| admission.offer_yield(true));
        let _ = await_lock_screen(locked, self.serving_uid, departure, private, || {
            offered
                && admission.is_some_and(arcen_pier_macos::session::AdmissionGuard::yield_requested)
        })
        .await;
        if offered && let Some(admission) = admission {
            let _ = admission.offer_yield(false);
        }
    }

    fn restore_outputs(&mut self) {
        if let Some(privacy) = self.desk_privacy.as_mut() {
            privacy.restore_outputs();
        }
        drop((
            self.handshake.arranged_display.take(),
            self.handshake.multi_monitor_displays.take(),
        ));
    }

    fn restore_audio(&mut self) {
        if !release_audio(self.audio.take()) {
            self.audio_failed = true;
        }
    }

    fn release_input(&mut self) {
        if let Some(privacy) = self.desk_privacy.take() {
            let counts = privacy.release_input();
            tracing::info!(
                target: arcen_telemetry::names::target::SESSION,
                displays = counts.displays,
                events_dropped = counts.events_dropped,
                filter_reenabled = counts.filter_reenabled,
                "the desk has its displays and input back"
            );
        }
    }

    fn release_guard(&mut self) {
        if let Some(guard) = self.lock_guard.take() {
            guard.release();
        }
    }
}

/// What [`lock_on_departure`] did (shared `lock_wait::LockAttempt`):
/// `NotOurs` is the login window, `serve`, or a console another account
/// took over.
type LockOutcome = arcen_session::console_session::lock_wait::LockAttempt;

/// Locks a signed-in user's session as its Deck leaves (ADR 0025 §4,
/// shared [`arcen_session::console_session::Departure`]): the desk finds
/// the lock screen, never an unlocked desktop someone else was using. Not the
/// login window (nobody is signed in) and not `serve`, which is a hand run
/// with no agent of its own. A console another account took over meanwhile
/// is that account's, and is left alone (read fail-closed:
/// `console::console_is_ours`). `left` is why the Deck left
/// (`deck_left`, or `capture_stopped` when sharing was stopped on the host),
/// as the log names it.
async fn lock_on_departure(
    serving_uid: Option<u32>,
    left: arcen_session::console_session::Departure,
) -> LockOutcome {
    use arcen_session::console_session::Departure;
    let Some(uid) = serving_uid else {
        return LockOutcome::NotOurs;
    };
    let departure = if arcen_pier_macos::console::console_is_ours(uid) {
        left
    } else {
        Departure::SessionChanged
    };
    if !departure.locks_session() {
        tracing::info!(
            target: arcen_telemetry::names::target::SESSION,
            departure = departure.token(),
            "the session is not locked: the console is no longer this account's"
        );
        return LockOutcome::NotOurs;
    }
    let locked = arcen_pier_macos::blocking::run_exclusive(
        "arcen-macos-session-lock",
        &SESSION_LOCK_SLOT,
        |failure| format!("{failure:?}"),
        || {
            arcen_pier_macos::screen_lock::lock_and_confirm(
                arcen_pier_macos::screen_lock::LOCK_CONFIRM_TIMEOUT,
            )
            .map_err(|error| error.to_string())
        },
    )
    .await
    .and_then(|locked| locked);
    match locked {
        Ok(()) => {
            tracing::info!(
                target: arcen_telemetry::names::target::SESSION,
                departure = departure.token(),
                "locked the session as its Deck left"
            );
            LockOutcome::Locked
        }
        Err(error) => {
            tracing::error!(
                target: arcen_telemetry::names::target::SESSION,
                departure = departure.token(),
                %error,
                "the session could not be locked as its Deck left"
            );
            LockOutcome::NotLocked
        }
    }
}

static SESSION_LOCK_SLOT: arcen_pier_macos::blocking::BlockingSlot =
    arcen_pier_macos::blocking::BlockingSlot::new();

/// What one served connection did.
enum SessionOutcome {
    /// Ran without a failure worth reporting.
    Served,
    /// Something failed; the process exit code should say so.
    Failed,
    /// The connection was refused before streaming and the loop should retry.
    Refused,
}

/// Asks for capture, input and audio consent, and says which are missing.
///
/// Requested from inside the agent because TCC attributes a request to the
/// process that makes it: asking from a terminal or an installer script
/// registers that terminal's responsible process, not this bundle, and leaves
/// the operator an empty Privacy list with nothing to switch on.
///
/// None of the answers is acted on. A refusal is a legitimate state — the host
/// still authenticates, serves input and clipboard, and reports what it cannot
/// do through its capability claims — so this names the missing toggle and
/// carries on.
fn announce_permissions() {
    let consent = arcen_pier_macos::permissions::request();
    if !consent.screen_recording {
        eprintln!(
            "Screen Recording is not granted. Approve \"Arcen Agent Helper\" under \
         System Settings > Privacy & Security > Screen & System Audio Recording; \
         until then this host serves a session with no picture."
        );
    }
    if !consent.accessibility {
        eprintln!(
            "Accessibility is not granted. Approve \"Arcen Agent Helper\" under \
         System Settings > Privacy & Security > Accessibility; until then \
         keyboard injection will not reach applications."
        );
    }
    advertise_host_audio();
}

/// Advertises host audio on every agent and serve path.
///
/// Called before any permission prompt so the hello does not depend on which
/// prompt path ran: when asking for permissions in turn replaced
/// `announce_permissions` for signed-in sessions, nothing set this any more,
/// every hello said `audio: false`, and the Deck never asked for sound.
fn advertise_host_audio() {
    // Audio is advertised, not proved, and deliberately not derived from the
    // screen-recording grant. Those are two different TCC services:
    // `kTCCServiceScreenCapture` backs "Screen & System Audio Recording" and
    // gates ScreenCaptureKit, while `kTCCServiceAudioCaptureOnly` backs the
    // separate "System Audio Recording" list and gates Core Audio process
    // taps. A host can hold the first and none of the second, which is
    // precisely the machine this was measured on: video streamed and every
    // tap delivered silence.
    //
    // An earlier version of this read the screen grant and called it the
    // answer, on the reasoning that Apple's pane names both. The pane names
    // both because ScreenCaptureKit can carry audio alongside video; a tap
    // taken on its own is a different permission with its own list, and no
    // public call reports its state. So the question is asked the only way
    // macOS allows — by attempting the tap inside a session that negotiated
    // audio, under a budget, and degrading to `CaptureUnavailable` when it
    // does not arrive.
    //
    // This used to be hardcoded false, which deadlocked: audio was advertised
    // only once capture had succeeded, and capture only ran once audio was
    // advertised, so it could never turn on. The Linux Pier advertises from
    // configuration and lets a session that cannot capture resolve to
    // disabled, which is the shape this now follows. Capture is attempted only
    // by a session that negotiated audio, so nothing is opened on a path that
    // did not ask for sound.
    // Advertised whenever this host has an output to tap at all. The session
    // resolves the truth: `register-audio-consent` is what puts the bundle in
    // the "System Audio Recording" list, and a session that cannot capture
    // tells the Deck `CaptureUnavailable` rather than leaving it waiting.
    arcen_pier_macos::session::set_audio_available(true);
}

/// Accepts Decks and serves them until told to stop.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn serve_loop(
    config: quinn::ServerConfig,
    tls: std::sync::Arc<arcen_pier_macos::tls_material::TlsLifecycle>,
    host: String,
    port: u16,
    once: bool,
    stream_frames: Option<u64>,
    local_playback: arcen_session::pier_config::LocalPlayback,
    first_login_timeout: std::time::Duration,
    session_policy: arcen_pier_macos::session::SessionPolicy,
    telemetry: arcen_pier_macos::observability::HostTelemetry,
    timezone_redirection: bool,
) -> ExitCode {
    let address = match resolve_bind_addr(&host, port).await {
        Ok(address) => address,
        Err(error) => {
            eprintln!("serve: {error}");
            return ExitCode::FAILURE;
        }
    };
    let listener = match arcen_pier_macos::net::Listener::bind(address, config) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("serve: {error}");
            return ExitCode::FAILURE;
        }
    };
    // Ask for capture and input consent from inside the agent itself.
    //
    // TCC attributes a request to the process that makes it, and lists a
    // subject in Privacy & Security only once it has asked. Requesting from a
    // terminal — over SSH, or from an installer script — registers whatever
    // that terminal's responsible process is, not this bundle, which is why an
    // installed helper could hold the port and still be absent from the
    // Privacy list with no way for an operator to approve it.
    //
    // The answer is not acted on. A refusal is a legitimate state: the host
    // still authenticates, still serves input and clipboard, and reports the
    // missing grant through its capability claims rather than refusing to run.
    announce_permissions();

    // A host that never starts and a host that started and accepted nothing
    // look the same in a log with no start record.
    if let Some(fields) = arcen_telemetry::lifecycle_fields::service_start(
        "arcen-pier-macos",
        arcen_pier_macos::VERSION,
        std::process::id(),
    ) {
        telemetry.emit(
            arcen_telemetry::LifecycleEventKind::ServiceStart,
            &arcen_pier_macos::observability::SessionScope::service(
                arcen_telemetry::CorrelationId::from_uuid_v4_bytes(
                    arcen_pier_macos::observability::random_correlation_bytes(),
                ),
            ),
            fields,
            arcen_telemetry::names::target::HEALTH,
            "service start",
        );
    }
    watch_tls(tls, &telemetry);
    println!(
        "{}",
        arcen_pier_macos::startup_build_line(&arcen_pier_macos::build_identity())
    );
    match listener.local_addr() {
        Ok(bound) => println!("listening on {bound}/udp"),
        Err(error) => {
            eprintln!("serve: {error}");
            return ExitCode::FAILURE;
        }
    }

    let admission_runtime = arcen_session::session_admission::SessionAdmissionRuntime::new();

    // Host-authoritative, and muted unless the operator said otherwise.
    let mut stream_failed = false;
    loop {
        let mut session = match listener
            .accept_with_session_admission(&admission_runtime)
            .await
        {
            Ok(arcen_pier_macos::net::SessionAdmissionAccept::Admitted(session)) => session,
            Ok(arcen_pier_macos::net::SessionAdmissionAccept::Refused { peer, reason }) => {
                eprintln!("refused {peer}: {reason}");
                stream_failed = true;
                if once {
                    listener.close();
                    return ExitCode::FAILURE;
                }
                continue;
            }
            Err(error) => {
                eprintln!("serve: {error}");
                return ExitCode::FAILURE;
            }
        };
        let peer = session.peer();
        let outcome = {
            let active = serve_one(
                session.socket_mut(),
                peer,
                stream_frames,
                local_playback,
                first_login_timeout,
                &session_policy,
                &telemetry,
                timezone_redirection,
                None,
                None,
            );
            tokio::pin!(active);
            let mut accepting = true;
            loop {
                tokio::select! {
                    outcome = &mut active => break outcome,
                    accepted = listener.accept_with_session_admission(&admission_runtime), if accepting => {
                        match accepted {
                            Ok(arcen_pier_macos::net::SessionAdmissionAccept::Admitted(_)) => {
                                eprintln!("serve: admitted a second session while one was active");
                                stream_failed = true;
                            }
                            Ok(arcen_pier_macos::net::SessionAdmissionAccept::Refused { peer, reason }) => {
                                eprintln!("refused {peer}: {reason}");
                            }
                            Err(error) => {
                                eprintln!("serve: {error}");
                                accepting = false;
                            }
                        }
                    }
                }
            }
        };
        drop(session);
        match outcome {
            SessionOutcome::Served => {}
            SessionOutcome::Failed => stream_failed = true,
            SessionOutcome::Refused => {
                stream_failed = true;
                if once {
                    listener.close();
                    return ExitCode::FAILURE;
                }
                continue;
            }
        }
        if once {
            listener.close();
            // A bounded run that produced no usable session is not a
            // successful smoke test, and automation reading only the exit
            // code would otherwise record it as one.
            return if stream_failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            };
        }
    }
}

/// Runs `new-host-cert`, matching the Linux host's flag vocabulary so the same
/// runbook works on either platform.
fn run_host_cert(arguments: &[String]) -> ExitCode {
    use arcen_transport::cert_provisioning::ProvisioningRequest;

    let mut directory = std::path::PathBuf::from(arcen_pier_macos::host_cert::DEFAULT_DIRECTORY);
    let mut request = ProvisioningRequest::Ensure;
    // Defaults to this Mac's own name when the operator names nothing, which
    // keeps the common case a single command.
    let mut subjects: Vec<String> = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--directory" => {
                index += 1;
                let Some(path) = arguments.get(index) else {
                    eprintln!("--directory requires a path");
                    return ExitCode::FAILURE;
                };
                directory = std::path::PathBuf::from(path);
            }
            // Both kinds are accepted and both are needed in practice: a Mac
            // reached by address needs an IP entry, and one reached by name
            // needs a DNS entry. `rcgen` classifies each string itself.
            flag @ ("--dns" | "--ip") => {
                let flag = flag.to_owned();
                index += 1;
                let Some(name) = arguments.get(index) else {
                    eprintln!("{flag} requires a value");
                    return ExitCode::FAILURE;
                };
                if name.is_empty() {
                    eprintln!("{flag} requires a non-empty value");
                    return ExitCode::FAILURE;
                }
                subjects.push(name.clone());
            }
            "--renew" => request = ProvisioningRequest::Renew,
            "--new-key" => request = ProvisioningRequest::Rekey,
            "--adopt-legacy" => request = ProvisioningRequest::AdoptLegacy,
            other => {
                eprintln!("unknown new-host-cert argument: {other}");
                return ExitCode::FAILURE;
            }
        }
        index += 1;
    }

    if subjects.is_empty() {
        subjects.push(hostname());
    }

    let now = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => duration.as_secs(),
        Err(error) => {
            eprintln!("system clock is before the Unix epoch: {error}");
            return ExitCode::FAILURE;
        }
    };

    match arcen_pier_macos::host_cert::provision(&directory, request, &subjects, now) {
        Ok(outcome) => {
            println!("{:?} in {}", outcome.action, directory.display());
            println!("certificate sha256: {}", outcome.certificate_sha256);
            if outcome.invalidated_pins {
                // Operators need to hear this before a Deck refuses to connect,
                // not after.
                println!(
                    "This replaced the host key. Every Deck that pinned the previous \
                     certificate must re-pin before it will connect again."
                );
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("new-host-cert: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Returns this host's name, falling back to a stable placeholder.
fn hostname() -> String {
    std::process::Command::new("/bin/hostname")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "arcen-pier".to_owned())
}

/// Converts a pixel count to the floating-point desktop rectangle input uses.
///
/// Display dimensions are far below the range where `f64` loses integer
/// precision, so this is exact for every real display.
#[allow(clippy::cast_precision_loss)]
fn pixels_to_f64(pixels: usize) -> f64 {
    pixels as f64
}

fn run_support_bundle(
    arguments: &[String],
    config: &arcen_pier_macos::PierFileConfig,
    startup: &arcen_pier_macos::StartupConfig,
) -> ExitCode {
    let options = match arcen_pier_macos::support_bundle::parse_options(arguments) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    match arcen_pier_macos::support_bundle::run(&options, config, startup) {
        Ok(path) => {
            println!("Support bundle written to {}", path.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("support bundle failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn probe_virtual_keyboard() -> ExitCode {
    use arcen_input::{KeyboardEvent, ModifierMask};
    arcen_pier_macos::input::set_input_backend(
        arcen_pier_macos::input::InputBackend::VirtualKeyboard(
            arcen_pier_macos::service::PIER_PROGRAM,
        ),
    );
    let bounds = arcen_pier_macos::input::DesktopBounds::new(0.0, 0.0, 1920.0, 1080.0);
    let mut controller = match arcen_pier_macos::input::InputController::new(bounds) {
        Ok(controller) => controller,
        Err(error) => {
            println!("virtual_keyboard=unavailable {error}");
            return ExitCode::FAILURE;
        }
    };
    if !controller.uses_virtual_keyboard() {
        println!("virtual_keyboard=refused (fell back to CGEvent)");
        return ExitCode::FAILURE;
    }
    let key = |controller: &mut arcen_pier_macos::input::InputController,
               key_id: u32,
               modifiers: u32,
               pressed: bool| {
        let _ = controller.key_event(&KeyboardEvent {
            key_id,
            pressed,
            modifiers: ModifierMask(modifiers),
            caps_lock_on: None,
            num_lock_on: None,
            scroll_lock_on: None,
            metadata: arcen_input::LowLatencyMetadata::default(),
        });
        std::thread::sleep(std::time::Duration::from_millis(15));
    };
    for character in "ARCEN HID OK".chars() {
        let code = u32::from(character);
        key(&mut controller, code, 0, true);
        key(&mut controller, code, 0, false);
    }
    let meta = arcen_pier_macos::input::keymap::MOD_META;
    key(&mut controller, 0x0100_0022, meta, true);
    key(&mut controller, 0x53, meta, true);
    key(&mut controller, 0x53, meta, false);
    key(&mut controller, 0x0100_0022, 0, false);
    let _ = controller.release_all();
    println!("virtual_keyboard=typed");
    ExitCode::SUCCESS
}

fn probe_desk_privacy(args: &[String]) -> ExitCode {
    let seconds = |flag: &str| -> Result<Option<std::time::Duration>, String> {
        match args.iter().position(|arg| arg == flag) {
            None => Ok(None),
            Some(index) => args
                .get(index + 1)
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|value| (1..=30).contains(value))
                .map(|value| Some(std::time::Duration::from_secs(value)))
                .ok_or_else(|| format!("{flag} takes 1 to 30 seconds")),
        }
    };
    let (listen, blank) = match (seconds("--listen"), seconds("--blank")) {
        (Ok(listen), Ok(blank)) => (listen.unwrap_or(std::time::Duration::from_secs(3)), blank),
        (Err(error), _) | (_, Err(error)) => {
            eprintln!("probe-desk-privacy: {error}");
            return ExitCode::FAILURE;
        }
    };
    let capture = args.iter().any(|arg| arg == "--capture");
    let report = arcen_pier_macos::desk_privacy::probe(listen, blank, capture);
    match serde_json::to_string_pretty(&report) {
        Ok(json) => println!("{json}"),
        Err(error) => eprintln!("probe-desk-privacy: {error}"),
    }
    let blank_ok = report.darkened_and_restored.unwrap_or(true);
    if report.filter_available && report.own_post_recognised && blank_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn parse_args(args: &[String]) -> Result<(&str, PathBuf, Vec<String>), String> {
    let mut command = "run";
    let mut config = PathBuf::from(arcen_pier_macos::DEFAULT_CONFIG_PATH);
    let mut support_args = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if matches!(
            command,
            "support-bundle"
                | "activate"
                | "probe-media"
                | "new-host-cert"
                | "serve"
                | "daemon"
                | "agent"
                | "launchd-plist"
                | "virtual-display-child"
                | "probe-audio"
                | "register-audio-consent"
                | "hid-injector"
        ) || command == "probe-desk-privacy"
            || command == "probe-cursor"
            || command == "lock-guard"
            || (cfg!(feature = "lab-pen-probe") && command == "probe-pen")
            || (cfg!(feature = "lab-bridged-hid-probe") && command == "probe-bridged-hid")
        {
            support_args.extend_from_slice(&args[index..]);
            break;
        }
        match args[index].as_str() {
            "validate-config" => command = "validate-config",
            "inventory" => command = "inventory",
            "permissions" => command = "permissions",
            "diagnostics" => command = "diagnostics",
            "probe-media" => command = "probe-media",
            "probe-input" => command = "probe-input",
            "probe-audio" => command = "probe-audio",
            "register-audio-consent" => command = "register-audio-consent",
            "core-audio-ready" => command = "core-audio-ready",
            "request-permissions" => command = "request-permissions",
            "probe-cursor" => command = "probe-cursor",
            "probe-desk-privacy" => command = "probe-desk-privacy",
            "probe-modes" => command = "probe-modes",
            "probe-scroll" => command = "probe-scroll",
            "probe-virtual-display" => command = "probe-virtual-display",
            "probe-virtual-hid" => command = "probe-virtual-hid",
            #[cfg(feature = "lab-pen-probe")]
            "probe-pen" => command = "probe-pen",
            "probe-pen-hid" => command = "probe-pen-hid",
            "probe-virtual-keyboard" => command = "probe-virtual-keyboard",
            "probe-clipboard" => command = "probe-clipboard",
            "probe-keyboard" => command = "probe-keyboard",
            "new-host-cert" => command = "new-host-cert",
            "serve" => command = "serve",
            "daemon" => command = "daemon",
            "agent" => command = "agent",
            "launchd-plist" => command = "launchd-plist",
            "hid-injector" => command = "hid-injector",
            "lock-guard" => command = "lock-guard",
            #[cfg(feature = "lab-bridged-hid-probe")]
            "probe-bridged-hid" => command = "probe-bridged-hid",
            "virtual-display-child" => command = "virtual-display-child",
            "keymap-watch" => command = "keymap-watch",
            "probe-keymap" => command = "probe-keymap",
            "install-service" => command = "install-service",
            "activate" => command = "activate",
            "uninstall-service" => command = "uninstall-service",
            "support-bundle" => command = "support-bundle",
            "--config" => {
                let path = args
                    .get(index + 1)
                    .ok_or_else(|| "--config requires a path".to_owned())?;
                config = PathBuf::from(path);
                index += 1;
            }
            "--help" | "-h" | "--version" | "-V" => {}
            argument => return Err(format!("unknown argument: {argument}")),
        }
        index += 1;
    }
    Ok((command, config, support_args))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{
        AudioStartupKind, audio_startup_kind, mute_policy_allows_session, parse_args,
        resolve_bind_addr_for_test, send_audio_result_with_timeout,
        send_microphone_result_with_timeout,
    };
    use std::time::{Duration, Instant};
    use tokio_tungstenite::tungstenite::Message;

    /// Records the hand-back in order; never touches this Mac.
    struct RecordedEnd {
        lock: super::LockOutcome,
        steps: Vec<String>,
    }

    impl super::AttachmentEnd for RecordedEnd {
        async fn tell_deck(&mut self, reason: &str) {
            self.steps.push(format!("tell:{reason}"));
        }
        async fn lock(
            &mut self,
            departure: arcen_session::console_session::Departure,
        ) -> super::LockOutcome {
            self.steps.push(format!("lock:{}", departure.token()));
            self.lock
        }
        async fn await_lock_screen(
            &mut self,
            locked: super::LockOutcome,
            departure: arcen_session::console_session::Departure,
        ) {
            self.steps
                .push(format!("await:{locked:?}:{}", departure.token()));
        }
        fn restore_outputs(&mut self) {
            self.steps.push("outputs".to_owned());
        }
        fn restore_audio(&mut self) {
            self.steps.push("audio".to_owned());
        }
        fn release_input(&mut self) {
            self.steps.push("input".to_owned());
        }
        fn release_guard(&mut self) {
            self.steps.push("guard".to_owned());
        }
    }

    fn stream_ended(
        error: &arcen_pier_macos::stream::StreamError,
        frames_sent: u64,
    ) -> arcen_session::console_session::stream_end::StreamEnd {
        arcen_session::console_session::stream_end::StreamEnd::classify(
            error.end_cause(),
            frames_sent,
        )
    }

    /// What `stream:didStopWithError:` reports for `code` in
    /// `SCStreamErrorDomain` (-3817: a person pressed Stop Sharing).
    fn stopped_by(code: i64) -> arcen_pier_macos::stream::StreamError {
        arcen_pier_macos::stream::StreamError::Capture(
            arcen_pier_macos::capture::CaptureError::StreamStopped(
                arcen_pier_macos::capture::StreamStop {
                    description: "The stream stopped.".to_owned(),
                    domain:
                        arcen_session::console_session::stream_end::SCREEN_CAPTURE_KIT_ERROR_DOMAIN
                            .to_owned(),
                    code,
                },
            ),
        )
    }

    #[tokio::test]
    async fn a_stream_the_system_stopped_is_not_told_and_still_locks() {
        use arcen_session::console_session::stream_end::StreamEnd;
        // The system stopped it, the window server connection was lost, a
        // display went away: the Deck may reconnect, the session locks.
        for code in [-3821, -3805, -3815] {
            let end = stream_ended(&stopped_by(code), 300);
            assert_eq!(end, StreamEnd::Failed, "{code}");
            let mut host = RecordedEnd {
                lock: super::LockOutcome::Locked,
                steps: Vec::new(),
            };
            super::end_attachment(&mut host, Some(end)).await;
            assert_eq!(
                host.steps.first().map(String::as_str),
                Some("lock:deck_left")
            );
            assert!(!host.steps.iter().any(|step| step.starts_with("tell:")));
            assert_eq!(host.steps.last().map(String::as_str), Some("guard"));
        }
    }

    #[tokio::test]
    async fn stopped_sharing_tells_the_deck_then_locks_and_releases_the_guard() {
        use arcen_session::console_session::stream_end::{DECK_REASON_SHARING_STOPPED, StreamEnd};
        // A person pressed Stop Sharing after a session that had streamed
        // for a while.
        let end = stream_ended(&stopped_by(-3817), 4_200);
        assert_eq!(end, StreamEnd::CaptureStopped);
        assert_eq!(end.reason_class(), "capture_stopped");

        let mut host = RecordedEnd {
            lock: super::LockOutcome::Locked,
            steps: Vec::new(),
        };
        let locked = super::end_attachment(&mut host, Some(end)).await;
        assert_eq!(locked, super::LockOutcome::Locked);
        assert_eq!(
            host.steps,
            [
                format!("tell:{DECK_REASON_SHARING_STOPPED}"),
                "lock:capture_stopped".to_owned(),
                "await:Locked:capture_stopped".to_owned(),
                "outputs".to_owned(),
                "audio".to_owned(),
                "input".to_owned(),
                "guard".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn a_refused_lock_is_awaited_before_anything_is_given_back() {
        let end = stream_ended(&stopped_by(-3817), 0);
        let mut host = RecordedEnd {
            lock: super::LockOutcome::NotLocked,
            steps: Vec::new(),
        };
        super::end_attachment(&mut host, Some(end)).await;
        let position = |step: &str| host.steps.iter().position(|seen| seen == step);
        let awaited = position("await:NotLocked:capture_stopped").expect("the lock is awaited");
        assert!(awaited < position("outputs").expect("outputs"));
        assert_eq!(host.steps.last().map(String::as_str), Some("guard"));
    }

    #[tokio::test]
    async fn a_dropped_deck_is_not_told_and_still_locks() {
        let end = stream_ended(
            &arcen_pier_macos::stream::StreamError::PeerGone("reset".to_owned()),
            10,
        );
        let mut host = RecordedEnd {
            lock: super::LockOutcome::Locked,
            steps: Vec::new(),
        };
        super::end_attachment(&mut host, Some(end)).await;
        assert_eq!(
            host.steps.first().map(String::as_str),
            Some("lock:deck_left")
        );
        assert!(!host.steps.iter().any(|step| step.starts_with("tell:")));

        // The desk watch told the Deck itself.
        let mut host = RecordedEnd {
            lock: super::LockOutcome::Locked,
            steps: Vec::new(),
        };
        super::end_attachment(&mut host, None).await;
        assert_eq!(
            host.steps.first().map(String::as_str),
            Some("lock:deck_left")
        );
        assert_eq!(host.steps.last().map(String::as_str), Some("guard"));
    }

    #[test]
    fn a_startup_waiting_on_the_prompt_keeps_admitting_on_its_proven_mute() {
        use super::{
            StartupMute, abandon_audio_startup, hand_over_startup, mute_held_by_abandoned_startup,
        };
        let muted = arcen_session::local_playback::MuteEvidence::observed(true, Some(true));
        let startup = std::sync::Arc::new(std::sync::Mutex::new(StartupMute::Pending));
        // Nothing to hold before the tap exists.
        assert!(abandon_audio_startup(&startup).is_none());
        *startup.lock().expect("startup") = StartupMute::Held(muted);
        assert_eq!(abandon_audio_startup(&startup), Some(muted));
        let (evidence, admitted) = mute_held_by_abandoned_startup().expect("still muting");
        assert_eq!(evidence, muted);
        assert!(
            mute_policy_allows_session(
                arcen_session::pier_config::LocalPlayback::Muted,
                Some(evidence)
            )
            .is_ok()
        );
        // A worker that returned without a tap proves nothing more.
        assert!(matches!(hand_over_startup(&startup, Ok(None)), Ok(None)));
        assert!(mute_held_by_abandoned_startup().is_none());
        // Nobody holds the startup: it is gone, not muting.
        drop((startup, admitted));
        assert!(mute_held_by_abandoned_startup().is_none());

        // A worker that unwound dropped its tap: nothing is admitted on it.
        let startup = std::sync::Arc::new(std::sync::Mutex::new(StartupMute::Held(muted)));
        assert_eq!(abandon_audio_startup(&startup), Some(muted));
        drop(super::StartupUnwound(std::sync::Arc::clone(&startup)));
        assert!(mute_held_by_abandoned_startup().is_none());
    }

    #[test]
    fn relays_are_bounded_overall_and_per_source() {
        use arcen_session::session_admission::{
            PREAUTH_PER_SOURCE, PreauthAdmission, PreauthRefusal,
        };
        let count = std::sync::Arc::new(PreauthAdmission::new(
            super::RELAY_LIMIT,
            PREAUTH_PER_SOURCE,
        ));
        let one: std::net::IpAddr = "203.0.113.1".parse().expect("address");
        let two: std::net::IpAddr = "203.0.113.2".parse().expect("address");
        let first = count.try_enter(one).expect("first");
        let second = count.try_enter(one).expect("second");
        assert_eq!(
            count.try_enter(one).err(),
            Some(PreauthRefusal::SourceBusy),
            "one address may not take every slot"
        );
        let third = count.try_enter(two).expect("another address");
        assert_eq!(
            count.try_enter(two).err(),
            Some(PreauthRefusal::Full),
            "overall bound"
        );
        drop(first);
        assert!(count.try_enter(one).is_ok(), "a slot comes back");
        drop((second, third));
    }

    #[test]
    #[cfg(not(feature = "lab-pen-probe"))]
    fn shipped_builds_have_no_pen_injecting_probe() {
        // `probe-pen` posts clicks under the installed agent's Accessibility
        // approval; any same-user process could start it as a launchd job.
        for args in [&["probe-pen"][..], &["probe-pen", "--strokes"]] {
            let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
            assert!(parse_args(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    #[cfg(not(feature = "lab-bridged-hid-probe"))]
    fn shipped_builds_have_no_bridged_hid_probe() {
        // `probe-bridged-hid` sends the Wacom mode switch to a bridged
        // tablet's HID device.
        for args in [&["probe-bridged-hid"][..], &["probe-bridged-hid", "--json"]] {
            let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
            assert!(parse_args(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    #[cfg(feature = "lab-pen-probe")]
    fn lab_builds_parse_the_pen_probe_and_its_arguments() {
        let args: Vec<String> = ["probe-pen", "--area", "1,2,3,4", "--strokes"]
            .iter()
            .map(|arg| (*arg).to_owned())
            .collect();
        let (command, _, support) = parse_args(&args).expect("lab build");
        assert_eq!(command, "probe-pen");
        assert_eq!(support, args[1..]);
    }

    #[test]
    fn parses_validation_and_config_path() {
        let args = vec![
            "validate-config".to_owned(),
            "--config".to_owned(),
            "/tmp/pier.json".to_owned(),
        ];
        let (command, path, _) = parse_args(&args).expect("valid arguments");
        assert_eq!(command, "validate-config");
        assert_eq!(path.to_str(), Some("/tmp/pier.json"));
    }

    #[test]
    fn rejects_missing_config_path() {
        let args = vec!["--config".to_owned()];
        assert!(parse_args(&args).is_err());
    }

    #[tokio::test]
    async fn a_session_the_desk_watch_ends_reports_the_frames_its_stream_sent() {
        use super::{Watched, serve_watched};
        use std::sync::atomic::{AtomicBool, Ordering};
        let lost = AtomicBool::new(false);
        // The stream `serve_one` runs, counting into the tally it is handed;
        // the watch ends the session after 42 frames and drops it.
        let outcome: Watched<u64> = serve_watched(
            |sent_frames| {
                let lost = &lost;
                async move {
                    for _ in 0..42 {
                        sent_frames.add_one();
                        tokio::task::yield_now().await;
                    }
                    lost.store(true, Ordering::Release);
                    std::future::pending::<u64>().await
                }
            },
            Some(&lost),
        )
        .await;
        assert!(
            matches!(outcome, Watched::WatchEnded { frames_sent: 42 }),
            "{outcome:?}"
        );

        // Without a watch the stream's own result stands.
        let outcome = serve_watched(|_| async { 7_u64 }, None).await;
        assert!(matches!(outcome, Watched::Stream(7)), "{outcome:?}");
    }

    #[tokio::test]
    async fn configured_loopback_bind_stays_on_loopback() {
        let address = resolve_bind_addr_for_test("127.0.0.1", 18444)
            .await
            .expect("loopback resolves");
        assert_eq!(address.ip(), std::net::IpAddr::from([127, 0, 0, 1]));
        assert_eq!(address.port(), 18444);
    }

    #[tokio::test]
    async fn audio_result_send_uses_the_stream_write_deadline() {
        struct PendingSink;
        impl futures_util::Sink<Message> for PendingSink {
            type Error = std::io::Error;
            fn poll_ready(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Result<(), Self::Error>> {
                std::task::Poll::Pending
            }
            fn start_send(self: std::pin::Pin<&mut Self>, _: Message) -> Result<(), Self::Error> {
                unreachable!("poll_ready never permits a send")
            }
            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Result<(), Self::Error>> {
                std::task::Poll::Ready(Ok(()))
            }
            fn poll_close(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Result<(), Self::Error>> {
                std::task::Poll::Ready(Ok(()))
            }
        }

        let mut sink = PendingSink;
        let started = Instant::now();
        let audio = arcen_media::audio::ResolvedAudioStream::disabled(
            arcen_media::audio::AudioProtocolMode::V1,
            arcen_protocol::messages::AudioStreamReason::CaptureUnavailable,
        );
        let error = send_audio_result_with_timeout(&mut sink, audio, Duration::from_millis(5))
            .await
            .expect_err("audio result writes must not wait forever");
        assert!(error.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(1));

        let mut sink = PendingSink;
        let started = Instant::now();
        let microphone = arcen_media::audio::ResolvedMicrophoneStream::disabled(
            1,
            arcen_protocol::messages::MicrophoneStreamReason::BackendUnavailable,
        );
        let error =
            send_microphone_result_with_timeout(&mut sink, microphone, Duration::from_millis(5))
                .await
                .expect_err("microphone result writes must not wait forever");
        assert!(error.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn parses_inventory_command_without_config() {
        let args = vec!["inventory".to_owned()];
        let (command, path, _) = parse_args(&args).expect("valid arguments");
        assert_eq!(command, "inventory");
        assert_eq!(
            path.to_str(),
            Some("/Library/Application Support/Arcen/pier.json")
        );
    }

    #[test]
    fn registering_audio_consent_is_its_own_command() {
        // macOS lists a subject under Privacy & Security only once it has
        // asked, and for system audio only a Core Audio process tap asks.
        // Until something asks there is nothing in the pane to switch on, so
        // audio stayed unavailable on a healthy host with no prompt and no
        // entry. This must stay a separate command: creating a tap holds the
        // output device, and doing it inside `serve` would contend with the
        // first session's own tap.
        let args = vec!["register-audio-consent".to_owned()];
        let (command, _, _) = parse_args(&args).expect("valid arguments");
        assert_eq!(command, "register-audio-consent");
    }

    #[test]
    fn mandatory_mute_without_evidence_refuses_the_session() {
        let refused = mute_policy_allows_session(
            arcen_session::pier_config::LocalPlayback::Muted,
            Some(arcen_pier_macos::audio::MuteEvidence {
                requested: true,
                observed_muted: Some(false),
                honoured: false,
            }),
        )
        .expect_err("unhonoured mandatory mute must refuse admission");
        assert!(refused.contains("mute was not honoured"));
        assert!(
            mute_policy_allows_session(arcen_session::pier_config::LocalPlayback::Muted, None)
                .is_err()
        );
        assert!(
            mute_policy_allows_session(arcen_session::pier_config::LocalPlayback::Audible, None)
                .is_ok(),
            "the explicit audible override does not require mute evidence",
        );
    }

    #[test]
    fn video_only_mute_starts_only_the_tap_lease() {
        let disabled = arcen_media::audio::ResolvedAudioStream::disabled(
            arcen_media::audio::AudioProtocolMode::V1,
            arcen_protocol::messages::AudioStreamReason::DisabledByPolicy,
        );

        assert_eq!(
            audio_startup_kind(disabled, arcen_session::pier_config::LocalPlayback::Muted),
            AudioStartupKind::MuteOnly,
            "a video-only muted session must not enter the recorder's sample-rate checks"
        );
        assert_eq!(
            audio_startup_kind(disabled, arcen_session::pier_config::LocalPlayback::Audible),
            AudioStartupKind::None
        );
    }

    #[test]
    fn negotiated_opus_starts_capture_for_the_opus_encoder() {
        let opus = arcen_media::audio::ResolvedAudioStream {
            mode: arcen_media::audio::AudioProtocolMode::V1,
            codec: Some(arcen_protocol::wire::AudioCodec::Opus),
            frame_spec: arcen_media::audio::AudioFrameSpec::V1,
            bitrate: arcen_media::audio::AudioBitrateTier::Kbps128,
            fec: false,
            dtx: false,
            reason: arcen_protocol::messages::AudioStreamReason::Enabled,
        };

        // The macOS Pier encodes Opus itself, so an Opus
        // negotiation needs the same capture a PCM one does.
        assert_eq!(
            audio_startup_kind(opus, arcen_session::pier_config::LocalPlayback::Audible),
            AudioStartupKind::Capture,
        );
        assert_eq!(
            audio_startup_kind(opus, arcen_session::pier_config::LocalPlayback::Muted),
            AudioStartupKind::Capture,
            "a muted session that sends audio holds its mute lease through the capture"
        );
    }

    #[test]
    fn parses_permissions_command_without_config() {
        let args = vec!["permissions".to_owned()];
        let (command, path, _) = parse_args(&args).expect("valid arguments");
        assert_eq!(command, "permissions");
        assert_eq!(
            path.to_str(),
            Some("/Library/Application Support/Arcen/pier.json")
        );
    }

    #[test]
    fn parses_diagnostics_command_with_config_path() {
        let args = vec![
            "diagnostics".to_owned(),
            "--config".to_owned(),
            "/tmp/pier.json".to_owned(),
        ];
        let (command, path, _) = parse_args(&args).expect("valid arguments");
        assert_eq!(command, "diagnostics");
        assert_eq!(path.to_str(), Some("/tmp/pier.json"));
    }

    #[test]
    fn preserves_support_bundle_arguments_after_command() {
        let args = vec![
            "--config".to_owned(),
            "/tmp/pier.json".to_owned(),
            "support-bundle".to_owned(),
            "--out".to_owned(),
            "/tmp/support".to_owned(),
        ];
        let (command, path, support_args) = parse_args(&args).expect("valid arguments");
        assert_eq!(command, "support-bundle");
        assert_eq!(path.to_str(), Some("/tmp/pier.json"));
        assert_eq!(
            support_args,
            vec!["--out".to_owned(), "/tmp/support".to_owned()]
        );
    }
}
