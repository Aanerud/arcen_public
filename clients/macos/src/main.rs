// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use arcen_deck::credentials::credentials_from_args;
use arcen_deck::display::metrics::{DisplayMetrics, LogicalRect, SafeAreaInsets};
use arcen_deck::pipeline::audio::PcmAudioPlayer;
use arcen_deck::pipeline::video_decoder::NativeVideoDecoder;
use arcen_deck::protocol::messages::{
    msg_type, AudioStreamResultMsg, ClientMonitor, CursorMode, HealthStatsMsg,
    InputCapabilityAvailability, MicrophoneStreamResultMsg, SafeAreaPolicyMsg, ServerHelloMsg,
    TabletModeMsg, TabletModeResultMsg, AUDIO_STREAM_RESULT, MICROPHONE_STREAM_RESULT,
    TABLET_MODE_RESULT,
};
use arcen_deck::transport::tls::{parse_fingerprint, TlsTrustConfig};
use arcen_deck::transport::websocket::{
    connect_smoke, spawn_session, AuthSubmission, ConnectOptions, FullFrameRequestGate,
    SessionCommand, SessionCommandSender, SessionEvent, SessionHandle, StreamProfile,
};
use arcen_deck::ui::app::{
    apply_automatic_remote_ui_scale_to_monitors, apply_automatic_remote_ui_scale_to_topology,
    monitors_for_displays_mode, primary_presentation_size, DisplaysMode,
};
use arcen_deck::ui::run_native_app;

/// Extra `--help` usage lines contributed by developer-only tooling. Empty
/// without the default-off `dev-tools` feature, so ordinary release help
/// output is byte-for-byte unchanged and never advertises the lab.
#[cfg(not(feature = "dev-tools"))]
const DEV_TOOLS_USAGE: &str = "";
#[cfg(not(feature = "usb-emulate-lab"))]
const USB_EMULATE_USAGE: &str = "";
#[cfg(feature = "usb-emulate-lab")]
const USB_EMULATE_USAGE: &str = "arcen-client input-smoke <host> [port] ... --usb-emulate wacom-intuos5 [--usb-emulate-strokes N] [--usb-emulate-lead-in-ms MS] [--usb-emulate-stroke scripted|landing|alternate|realistic|tilted|rest|hover-rest|upper-held]   (lab-only; implies --tablet-mode hard; plays a scripted pen stroke through an emulated Wacom Intuos5 instead of capturing a tablet; the same flags after `--connect HOST PORT` give the GUI the emulated pen)\n";
#[cfg(feature = "dev-tools")]
const DEV_TOOLS_USAGE: &str = "arcen-client virtual-monitor-lab [2|4] [--timeout-secs N]   (developer-only; requires ARCEN_ENABLE_VIRTUAL_MONITOR_LAB=1)\narcen-client probe-matrix [--parameter-sets <dir>] [--output <path>]   (developer-only; see docs/testing/color-matrix-results.json)\n";

/// `platform-report`: the Deck platform profile and the real decode
/// capability probe, one `key=value` per line, without a window or a Pier.
fn print_platform_report() {
    let platform = arcen_deck::platform::current();
    let capabilities = arcen_deck::pipeline::video_decoder::probe_decode_capabilities();
    println!("deck_platform={}", platform.label());
    println!("arch={}", platform.architecture().token());
    println!("macos={}", platform.macos());
    println!("rosetta={}", platform.translated());
    println!("av1_decode_possible={}", platform.av1_decode_possible());
    println!(
        "software_decode_expected={}",
        platform.software_decode_expected()
    );
    println!(
        "native_usb_helper_supported={}",
        platform.native_usb_helper_supported()
    );
    println!(
        "decode h264={} h265={} av1={} yuv444={} main10={} main12={} full_range={} \
         bt601={} bt2020ncl={}",
        capabilities.h264,
        capabilities.h265,
        capabilities.av1,
        capabilities.yuv444,
        capabilities.main10,
        capabilities.main12,
        capabilities.full_range,
        capabilities.bt601_matrix,
        capabilities.bt2020_ncl_matrix,
    );
    println!("decoder_backend={}", capabilities.decoder_backend_label());
    println!(
        "decode_path={}",
        platform
            .assess_decode_path(capabilities.hardware_accelerated)
            .token()
    );
}

#[tokio::main]
async fn main() {
    // Logging is infrastructure: bring it up before anything else so every
    // subsystem has a sink from the first line. The GUI later re-points the
    // level at the user's saved "Log level" preference; CLI paths keep Info.
    arcen_deck::logging::init(arcen_deck::logging::startup_profile());
    arcen_deck::logging::diagnostics::log_startup_banner();

    let args: Vec<String> = std::env::args().collect();
    let subcommand = args.get(1).map(String::as_str);
    if subcommand == Some("platform-report") {
        print_platform_report();
        arcen_deck::logging::shutdown();
        return;
    }
    if subcommand == Some("multi-monitor-harness") {
        run_multi_monitor_harness_subcommand(&args);
        arcen_deck::logging::shutdown();
        return;
    }
    #[cfg(feature = "native-tablet")]
    if subcommand == Some("usb-helper-status") {
        use arcen_deck::usb_helper_install::{install_state, HelperInstallState};
        let state = install_state();
        let label = match state {
            HelperInstallState::Enabled => "enabled",
            HelperInstallState::RequiresApproval => "requires_approval",
            HelperInstallState::NotRegistered => "not_registered",
            HelperInstallState::NotFound => "not_found",
            HelperInstallState::UnsupportedOs => "unsupported_os",
            HelperInstallState::Unknown(_) => "unknown",
        };
        println!("usb-helper-status state={label}");
        println!("usb-helper-status ready={}", state.is_ready());
        let (bundle_path, plist_path, plist_exists) = arcen_deck::usb_helper_install::diagnostics();
        println!("usb-helper-status bundle={bundle_path}");
        println!("usb-helper-status plist={plist_path}");
        println!("usb-helper-status plist_present={plist_exists}");
        println!("usb-helper-status guidance={}", state.guidance());
        arcen_deck::logging::shutdown();
        return;
    }
    #[cfg(feature = "native-tablet")]
    if subcommand == Some("usb-helper-install") {
        // Registers the bundled LaunchDaemon. Raises one administrator prompt;
        // after this launchd starts the helper as root and no password is ever
        // needed again. See docs/adr/0011-macos-privileged-usb-helper.md.
        let code = match arcen_deck::usb_helper_install::register() {
            Ok(state) => {
                println!("usb-helper-install ready={}", state.is_ready());
                println!("usb-helper-install {}", state.guidance());
                if state.is_ready() {
                    0
                } else {
                    if matches!(
                        state,
                        arcen_deck::usb_helper_install::HelperInstallState::RequiresApproval
                    ) {
                        arcen_deck::usb_helper_install::open_login_items_settings();
                    }
                    1
                }
            }
            Err(error) => {
                eprintln!("usb-helper-install failed: {error}");
                1
            }
        };
        arcen_deck::logging::shutdown();
        if code != 0 {
            std::process::exit(code);
        }
        return;
    }
    #[cfg(feature = "native-tablet")]
    if subcommand == Some("usb-helper-uninstall") {
        let code = match arcen_deck::usb_helper_install::unregister() {
            Ok(()) => {
                println!("usb-helper-uninstall removed the Arcen USB helper daemon");
                0
            }
            Err(error) => {
                eprintln!("usb-helper-uninstall failed: {error}");
                1
            }
        };
        arcen_deck::logging::shutdown();
        if code != 0 {
            std::process::exit(code);
        }
        return;
    }
    #[cfg(feature = "native-tablet")]
    if subcommand == Some("usb-claim-probe") {
        let code = match usb_claim_probe() {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("usb-claim-probe failed: {error}");
                1
            }
        };
        arcen_deck::logging::shutdown();
        if code != 0 {
            std::process::exit(code);
        }
        return;
    }
    #[cfg(feature = "native-tablet")]
    if subcommand == Some("usb-capture-probe") {
        let code = match usb_capture_probe() {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("usb-capture-probe failed: {error}");
                1
            }
        };
        arcen_deck::logging::shutdown();
        if code != 0 {
            std::process::exit(code);
        }
        return;
    }
    if subcommand == Some("multi-monitor-window-diagnostic") {
        run_multi_monitor_window_diagnostic_subcommand(&args);
        arcen_deck::logging::shutdown();
        return;
    }

    /// Identities of attached HID devices whose report descriptors classify
    /// as a pen tablet (`arcen_usb_bridge::classify_pen_tablet`). The lab
    /// probes below pick their device from this list, never by vendor id.
    #[cfg(feature = "native-tablet")]
    fn is_attached_pen_tablet(vendor_id: u16, product_id: u16) -> bool {
        arcen_deck::tablet::attached_pen_tablets().contains(&(vendor_id, product_id))
    }

    #[cfg(feature = "native-tablet")]
    fn usb_claim_probe() -> Result<(), String> {
        let device = nusb::list_devices()
            .map_err(|error| error.to_string())?
            .find(|device| is_attached_pen_tablet(device.vendor_id(), device.product_id()))
            .ok_or_else(|| "no pen tablet is attached".to_owned())?;
        let probe_identity = format!("{:04x}:{:04x}", device.vendor_id(), device.product_id());
        let interfaces: Vec<_> = device
            .interfaces()
            .map(|interface| {
                (
                    interface.interface_number(),
                    interface.class(),
                    interface.subclass(),
                    interface.protocol(),
                )
            })
            .collect();
        println!(
            "usb-claim-probe device={probe_identity} interfaces={}",
            interfaces.len()
        );
        let opened = device
            .open()
            .map_err(|error| format!("open physical USB device: {error}"))?;
        println!("usb-claim-probe device_open=ok");
        let mut claimed = 0_u8;
        for (number, class, subclass, protocol) in interfaces {
            match opened.claim_interface(number) {
                Ok(interface) => {
                    claimed = claimed.saturating_add(1);
                    println!(
                        "usb-claim-probe interface={number} class={class:02x}/{subclass:02x}/{protocol:02x} claim=ok"
                    );
                    drop(interface);
                }
                Err(error) => println!(
                    "usb-claim-probe interface={number} class={class:02x}/{subclass:02x}/{protocol:02x} claim=failed error={error}"
                ),
            }
        }
        if claimed == 0 {
            return Err("no pen tablet USB interface could be claimed".to_owned());
        }
        println!("usb-claim-probe claimed_interfaces={claimed}");
        Ok(())
    }

    #[cfg(feature = "native-tablet")]
    fn usb_capture_probe() -> Result<(), String> {
        use rusb::UsbContext;

        struct CaptureGuard {
            handle: rusb::DeviceHandle<rusb::Context>,
            captured: bool,
        }

        impl Drop for CaptureGuard {
            fn drop(&mut self) {
                if self.captured {
                    let _ = self.handle.attach_kernel_driver(0);
                }
            }
        }

        let context = rusb::Context::new().map_err(|error| error.to_string())?;
        let devices = context.devices().map_err(|error| error.to_string())?;
        let device = devices
            .iter()
            .find(|device| {
                device.device_descriptor().is_ok_and(|descriptor| {
                    is_attached_pen_tablet(descriptor.vendor_id(), descriptor.product_id())
                })
            })
            .ok_or_else(|| "no pen tablet is attached".to_owned())?;
        let descriptor = device
            .device_descriptor()
            .map_err(|error| format!("read device descriptor: {error}"))?;
        let probe_identity = format!(
            "{:04x}:{:04x}",
            descriptor.vendor_id(),
            descriptor.product_id()
        );
        let handle = device
            .open()
            .map_err(|error| format!("open physical USB device: {error}"))?;
        let mut guard = CaptureGuard {
            handle,
            captured: false,
        };
        println!(
            "usb-capture-probe device={probe_identity} configurations={}",
            descriptor.num_configurations()
        );

        guard
            .handle
            .detach_kernel_driver(0)
            .map_err(|error| format!("capture physical USB device: {error}"))?;
        guard.captured = true;
        println!("usb-capture-probe device_capture=ok");

        let configuration = device
            .active_config_descriptor()
            .map_err(|error| format!("read active configuration: {error}"))?;
        let mut claimed = Vec::new();
        for interface in configuration.interfaces() {
            let number = interface.number();
            match guard.handle.claim_interface(number) {
                Ok(()) => {
                    claimed.push(number);
                    println!("usb-capture-probe interface={number} claim=ok");
                }
                Err(error) => {
                    for number in claimed.drain(..) {
                        let _ = guard.handle.release_interface(number);
                    }
                    return Err(format!("claim interface {number}: {error}"));
                }
            }
        }
        for number in claimed.drain(..) {
            guard
                .handle
                .release_interface(number)
                .map_err(|error| format!("release interface {number}: {error}"))?;
        }
        guard
            .handle
            .attach_kernel_driver(0)
            .map_err(|error| format!("release captured USB device: {error}"))?;
        guard.captured = false;
        println!("usb-capture-probe device_release=ok");
        Ok(())
    }
    // Developer-only (`dev-tools`), absent from release builds and release
    // `--help`. Additionally refuses to run without an explicit runtime
    // opt-in; see `arcen_deck::ui::virtual_monitor_lab`.
    #[cfg(feature = "dev-tools")]
    {
        if subcommand == Some("virtual-monitor-lab") {
            run_virtual_monitor_lab_subcommand(&args);
            arcen_deck::logging::shutdown();
            return;
        }
        if subcommand == Some("probe-matrix") {
            run_probe_matrix_subcommand(&args);
            arcen_deck::logging::shutdown();
            return;
        }
    }
    if subcommand == Some("diagnose") {
        let code = run_diagnose_subcommand(&args).await;
        arcen_deck::logging::shutdown();
        if code != 0 {
            std::process::exit(code);
        }
        return;
    }
    if subcommand != Some("connect-smoke")
        && subcommand != Some("media-smoke")
        && subcommand != Some("input-smoke")
        && subcommand != Some("multi-monitor-smoke")
    {
        if args.iter().any(|arg| arg == "--help" || arg == "-h") {
            println!(
                "arcen-client protocol_version={}\n\
                usage:\n\
                  arcen-client\n\
                  arcen-client --connect <host> [port] [--ca-bundle PATH] [--pin-sha256 FP] [--insecure-skip-verify] [--username USER] [--password PASS | --password-file PATH] [--credentials-stdin] [--pipeline auto|speed|grading|hdr] [--codec h264|h265|av1] [--chroma yuv420|yuv444] [--video-selection exact|adaptive-performance|color-fidelity] [--bit-depth 8|10|12] [--color-range limited|full] [--color-matrix bt709|identity|bt601|bt2020ncl] [--transfer bt709|srgb|pq|hlg] [--color-primaries bt709|bt2020|display_p3] [--encode-intent interactive|quality] [--max-fps N] [--microphone]\n\
                  arcen-client connect-smoke <host> [port] [--ca-bundle PATH] [--pin-sha256 FP] [--insecure-skip-verify] [--username USER] [--password PASS | --password-file PATH] [--credentials-stdin] [--pipeline auto|speed|grading|hdr] [--video-selection exact|adaptive-performance|color-fidelity] [--cursor-mode local|host] [--displays-mode match_layout|single_primary|windowed] [--hidpi-displays MASK]\n\
                  arcen-client media-smoke <host> [port] [--ca-bundle PATH] [--pin-sha256 FP] [--insecure-skip-verify] [--username USER] [--password PASS | --password-file PATH] [--credentials-stdin] [--pipeline auto|speed|grading|hdr] [--video-selection exact|adaptive-performance|color-fidelity] [--video-only] [--microphone] [--cursor-mode local|host] [--displays-mode match_layout|single_primary|windowed] [--hidpi-displays MASK]\n\
                  arcen-client multi-monitor-smoke <host> [port] [--ca-bundle PATH] [--pin-sha256 FP] [--insecure-skip-verify] [--username USER] [--password PASS | --password-file PATH] [--credentials-stdin] [--accept-disclaimer] [--monitor-fixture PATH] [--hidpi-displays MASK] [--full-color-display ID] [--tablet-mode light|hard|off]\n\
                  arcen-client input-smoke <host> [port] [--ca-bundle PATH] [--pin-sha256 FP] [--insecure-skip-verify] [--username USER] [--password PASS | --password-file PATH] [--credentials-stdin] [--cursor-mode local|host] [--displays-mode match_layout|single_primary|windowed] [--tablet-mode light|hard|off] [--pen | --pen-full [--pen-wait-ms MS] [--pen-end-in-contact] | --pen-file PATH [--pen-go-file PATH]] [--text STRING]... [--keys SCRIPT | --keys-file PATH (quiet: tokens never printed) [--keyboard-shortcuts automatic|windows_app|swap_command_control|command_is_control] [--left-option-altgr]]... [--paste-file PATH [--paste-rate CHARS_PER_SECOND] [--paste-wait-ms MS] [--paste-click] [--paste-return]]\n\
                  arcen-client multi-monitor-harness [1|2|4] [--frames N]\n\
                  arcen-client multi-monitor-window-diagnostic [1|2|4] [--timeout-secs N]\n\
                  arcen-client platform-report\n\
                  arcen-client diagnose <host[:port]> [port] [--pin-sha256 FP | --pin-spki FP] [--ignore-remembered]   (connection test; sends no credentials)\n\
                {}{}transport: QUIC/TLS 1.3 over UDP; default port 18444\n\
                \n\
                {}\n\
                Source: {}",
                arcen_deck::protocol::PROTOCOL_VERSION,
                DEV_TOOLS_USAGE,
                USB_EMULATE_USAGE,
                arcen_deck::SOURCE_OFFER,
                arcen_deck::SOURCE_URL
            );
            arcen_deck::logging::shutdown();
            return;
        }
        let initial_connect = match quick_connect_options_from_cli_args(&args) {
            Ok(options) => options,
            Err(error) => {
                eprintln!("{error}");
                shutdown_and_exit(2);
            }
        };
        // Lab builds: `--connect … --usb-emulate wacom-intuos5` gives the GUI
        // the scripted pen too, so a host-led cursor can be proven on screen
        // without a tablet or any input on this Mac.
        if let Err(error) = select_usb_emulation(&args) {
            eprintln!("{error}");
            shutdown_and_exit(2);
        }
        if let Err(error) = run_native_app(initial_connect) {
            tracing::error!(
                target: arcen_deck::logging::target::UI,
                %error,
                "failed to start Arcen UI"
            );
            eprintln!("failed to start Arcen UI: {error}");
            shutdown_and_exit(1);
        }
        arcen_deck::logging::shutdown();
        return;
    }

    let options = match connect_options_from_cli_args(&args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error}");
            shutdown_and_exit(2);
        }
    };
    if let Err(error) = select_usb_emulation(&args) {
        eprintln!("{error}");
        shutdown_and_exit(2);
    }
    // The smoke subcommands double as a support diagnostic dump: emit the USB
    // inventory (tablets highlighted) so a sysadmin can capture client hardware
    // without the GUI. Synchronous here is fine — these are one-shot tools.
    arcen_deck::logging::diagnostics::log_usb_inventory();
    if matches!(subcommand, Some("media-smoke" | "multi-monitor-smoke")) {
        if let Err(error) = media_smoke(
            options,
            subcommand == Some("multi-monitor-smoke"),
            args.iter().any(|arg| arg == "--accept-disclaimer"),
            !args.iter().any(|arg| arg == "--video-only"),
        )
        .await
        {
            eprintln!("media-smoke failed: {error}");
            shutdown_and_exit(1);
        }
        arcen_deck::logging::shutdown();
        return;
    }
    if subcommand == Some("input-smoke") {
        let pen = if args.iter().any(|arg| arg == "--pen-full") {
            Some(PenSmokeScript::Full {
                wait_ms: flag_value(&args, "--pen-wait-ms")
                    .and_then(|ms| ms.parse().ok())
                    .unwrap_or(500),
                end_in_contact: args.iter().any(|arg| arg == "--pen-end-in-contact"),
            })
        } else if let Some(path) = flag_value(&args, "--pen-file") {
            match std::fs::read_to_string(&path)
                .map_err(|error| error.to_string())
                .and_then(|text| parse_pen_file(&text))
            {
                Ok(samples) => Some(PenSmokeScript::File {
                    samples,
                    go_file: flag_value(&args, "--pen-go-file"),
                }),
                Err(error) => {
                    eprintln!("--pen-file: {error}");
                    std::process::exit(2);
                }
            }
        } else if args.iter().any(|arg| arg == "--pen") {
            Some(PenSmokeScript::Strokes)
        } else {
            None
        };
        let mut scripts = flag_values(&args, "--keys");
        let quiet = flag_value(&args, "--keys-file").is_some();
        if let Some(path) = flag_value(&args, "--keys-file") {
            match std::fs::read_to_string(&path) {
                Ok(script) => scripts.push(script),
                Err(error) => {
                    eprintln!("--keys-file: {}", error.kind());
                    std::process::exit(2);
                }
            }
        }
        let keys = KeysSmoke {
            scripts,
            quiet,
            settings: arcen_input::keyboard::KeyboardSettings {
                shortcuts: flag_value(&args, "--keyboard-shortcuts")
                    .and_then(|token| arcen_input::keyboard::ShortcutPreference::from_token(&token))
                    .unwrap_or_default(),
                left_option_altgr: args.iter().any(|arg| arg == "--left-option-altgr"),
                typing: None,
            },
        };
        let paste = flag_value(&args, "--paste-file").map(|path| PasteSmoke {
            path,
            rate: flag_value(&args, "--paste-rate").and_then(|rate| rate.parse().ok()),
            wait_ms: flag_value(&args, "--paste-wait-ms")
                .and_then(|ms| ms.parse().ok())
                .unwrap_or(0),
            click: args.iter().any(|arg| arg == "--paste-click"),
            return_after: args.iter().any(|arg| arg == "--paste-return"),
        });
        if let Err(error) =
            input_smoke(options, flag_values(&args, "--text"), pen, keys, paste).await
        {
            eprintln!("input-smoke failed: {error}");
            shutdown_and_exit(1);
        }
        arcen_deck::logging::shutdown();
        return;
    }

    match connect_smoke(options).await {
        Ok(result) => {
            if let Some(hello) = result.server_hello {
                println!(
                    "connected uri={} server={} version={} codec={} bit_depth={} range={} \
                     matrix={} pixel_format={} encoder={} encoder_class={} active_pipeline={} state={}",
                    result.uri,
                    hello.server_name,
                    hello.version,
                    hello.codec,
                    hello.color_caps.active_bit_depth,
                    hello.color_caps.active_range,
                    hello.color_caps.active_matrix,
                    hello.color_caps.advertised_pix_fmt,
                    hello.encoder_backend,
                    hello.encoder_class,
                    hello.active_pipeline
                        .as_ref()
                        .map_or("absent", arcen_protocol::messages::ServedStreamPipeline::token),
                    result.fsm_state
                );
            }
        }
        Err(error) => {
            eprintln!("connect-smoke failed: {error}");
            shutdown_and_exit(1);
        }
    }
    arcen_deck::logging::shutdown();
}

fn shutdown_and_exit(code: i32) -> ! {
    arcen_deck::logging::shutdown();
    std::process::exit(code)
}

const DIAGNOSE_USAGE: &str = "usage: arcen-deck diagnose <host[:port]> [port] [--pin-sha256 FP | --pin-spki FP] [--ignore-remembered]\n\
Tests the way to a Pier step by step (name, UDP 18444, certificate, the Pier's\n\
greeting before sign-in, path MTU, round trip) and prints a report.\n\
No user name or password is asked for or sent.\n\
Without a pin it expects the identity this Deck remembers for that address.\n\
Exit status: 0 ready, 1 warnings, 2 cannot connect, 64 bad arguments.";

/// `diagnose <host[:port]>`: the connection test, as text.
async fn run_diagnose_subcommand(args: &[String]) -> i32 {
    use arcen_deck::transport::diagnose::{DiagnoseRequest, PinOrigin};
    use arcen_deck::transport::tls::parse_fingerprint;
    use arcen_session::connection_diagnostics::DiagnosticTarget;

    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{DIAGNOSE_USAGE}");
        return 0;
    }
    let mut positional = args
        .iter()
        .skip(2)
        .scan(false, |skip_value, arg| {
            let was_value = *skip_value;
            *skip_value = matches!(arg.as_str(), "--pin-sha256" | "--pin-spki");
            Some((was_value, arg))
        })
        .filter(|(was_value, arg)| !was_value && !arg.starts_with("--"))
        .map(|(_, arg)| arg.as_str());
    let Some(address) = positional.next() else {
        eprintln!("{DIAGNOSE_USAGE}");
        return 64;
    };
    let port = match positional.next().map(str::parse::<u16>) {
        None => None,
        Some(Ok(port)) if port != 0 => Some(port),
        Some(_) => {
            eprintln!("diagnose: the port must be a number from 1 to 65535");
            return 64;
        }
    };
    let target = match DiagnosticTarget::parse_with_port(address, port) {
        Ok(target) => target,
        Err(error) => {
            eprintln!("diagnose: {error}");
            return 64;
        }
    };
    let supplied = match (
        flag_value(args, "--pin-sha256"),
        flag_value(args, "--pin-spki"),
    ) {
        (Some(_), Some(_)) => {
            eprintln!("diagnose: give either --pin-sha256 or --pin-spki, not both");
            return 64;
        }
        (Some(value), None) => Some((arcen_transport::tls::PinKind::CertificateSha256, value)),
        (None, Some(value)) => Some((
            arcen_transport::tls::PinKind::SubjectPublicKeyInfoSha256,
            value,
        )),
        (None, None) => None,
    };
    let request = match supplied {
        Some((kind, value)) => match parse_fingerprint(&value) {
            Ok(digest) => DiagnoseRequest {
                target,
                pin: Some((
                    arcen_transport::tls::TlsPin::new(kind, digest),
                    PinOrigin::Supplied,
                )),
            },
            Err(error) => {
                eprintln!("diagnose: {error}");
                return 64;
            }
        },
        None if args.iter().any(|arg| arg == "--ignore-remembered") => {
            DiagnoseRequest { target, pin: None }
        }
        None => DiagnoseRequest::with_remembered_pin(target),
    };
    let report = arcen_deck::transport::diagnose::run(request).await;
    print!(
        "{}",
        report.render_text(&arcen_deck::transport::diagnose::deck_label())
    );
    report.exit_code()
}

/// `multi-monitor-harness [1|2|4] [--frames N]`: drives the synthetic
/// multi-monitor producer/test harness (`arcen_deck::pipeline::synthetic_multi_monitor`)
/// with no real host, decoder, or network connection, and prints a one-line
/// isolation report. Exits nonzero when isolation could not be verified so it
/// is usable as a CI/diagnostic smoke check.
fn run_multi_monitor_harness_subcommand(args: &[String]) {
    let monitor_count = args
        .get(2)
        .filter(|value| !value.starts_with("--"))
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(2);
    let frames_per_monitor = args
        .iter()
        .position(|arg| arg == "--frames")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(8);

    match arcen_deck::pipeline::synthetic_multi_monitor::run_isolation_harness(
        monitor_count,
        frames_per_monitor,
    ) {
        Ok(report) => {
            println!(
                "{}",
                arcen_deck::pipeline::synthetic_multi_monitor::format_report(&report)
            );
            if !report.isolation_verified {
                eprintln!("multi-monitor-harness: routing isolation could not be verified");
                shutdown_and_exit(1);
            }
        }
        Err(error) => {
            eprintln!("multi-monitor-harness failed: {error}");
            shutdown_and_exit(2);
        }
    }
}

/// `multi-monitor-window-diagnostic [1|2|4] [--timeout-secs N]`: opens real
/// native macOS fullscreen windows (one per synthetic monitor id) with no
/// real host, network connection, or decoder, proving the eframe/egui/winit
/// stack can genuinely fullscreen-bind and paint one distinct window per
/// negotiated monitor before closing everything and printing a one-line
/// report. This is a manual/visual diagnostic: it takes over the display
/// (real `NSWindow` fullscreen Space transitions) and should only be run
/// interactively on a machine the operator owns, never in CI or an
/// unattended/shared session.
fn run_multi_monitor_window_diagnostic_subcommand(args: &[String]) {
    let monitor_count = args
        .get(2)
        .filter(|value| !value.starts_with("--"))
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(2);
    let timeout_secs = args
        .iter()
        .position(|arg| arg == "--timeout-secs")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(10);

    match arcen_deck::ui::multi_window_diagnostic::run_native_window_diagnostic(
        monitor_count,
        std::time::Duration::from_secs(timeout_secs),
    ) {
        Ok(report) => {
            println!("{report}");
            if !report.isolation_verified || !report.unconfirmed_session_monitor_ids.is_empty() {
                eprintln!(
                    "multi-monitor-window-diagnostic: not every window confirmed a genuine \
                     fullscreen bind, or isolation could not be verified"
                );
                shutdown_and_exit(1);
            }
        }
        Err(error) => {
            eprintln!("multi-monitor-window-diagnostic failed: {error}");
            shutdown_and_exit(2);
        }
    }
}

/// `virtual-monitor-lab [2|4] [--timeout-secs N]`: developer-only. Tiles 2
/// (halves) or 4 (quadrants) real, decorated native windows *inside one
/// attached display*, paints each from a real routed synthetic frame, and
/// prints the deterministic scripted shared-emitter input trace. Requires
/// `ARCEN_ENABLE_VIRTUAL_MONITOR_LAB=1` on top of the default-off
/// `dev-tools` build feature; exits 1 when a tile did not bind or isolation
/// could not be verified, and 2 on error (including the missing opt-in).
#[cfg(feature = "dev-tools")]
fn run_virtual_monitor_lab_subcommand(args: &[String]) {
    let window_count = args
        .get(2)
        .filter(|value| !value.starts_with("--"))
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(2);
    let timeout_secs = args
        .iter()
        .position(|arg| arg == "--timeout-secs")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(20);

    match arcen_deck::ui::virtual_monitor_lab::run_virtual_monitor_lab(
        window_count,
        Duration::from_secs(timeout_secs),
    ) {
        Ok(report) => {
            println!("{report}");
            println!("{}", report.format_tiles());
            println!("{}", report.format_trace());
            if !report.fully_verified() {
                eprintln!(
                    "virtual-monitor-lab: not every tiled window bound on the target display, \
                     or paint isolation could not be verified"
                );
                shutdown_and_exit(1);
            }
        }
        Err(error) => {
            eprintln!("virtual-monitor-lab failed: {error}");
            shutdown_and_exit(2);
        }
    }
}

/// `probe-matrix [--parameter-sets <dir>] [--output <path>]`:
/// developer-only (`dev-tools`). Runs every row of
/// `arcen_media::video::PROBE_MATRIX` through a real `VTDecompressionSession`
/// built from host-produced parameter sets and prints (or writes) a JSON
/// report shaped like `docs/testing/color-matrix-results.json`. See the
/// `arcen_deck::probe_matrix` module docs for the on-disk input contract.
/// Never fails the whole run because one row could not decode -- a failing
/// row is itself the finding -- so this exits 0 once every row has been
/// attempted; only argument or I/O errors (e.g. an unwritable `--output`
/// path, or a JSON serialisation failure) exit non-zero.
#[cfg(feature = "dev-tools")]
fn run_probe_matrix_subcommand(args: &[String]) {
    let options = arcen_deck::probe_matrix::ProbeMatrixOptions {
        parameter_sets_dir: flag_value(args, "--parameter-sets").map(std::path::PathBuf::from),
        output_path: flag_value(args, "--output").map(std::path::PathBuf::from),
    };
    match arcen_deck::probe_matrix::execute(&options) {
        Ok(json) => match &options.output_path {
            Some(path) => println!("probe-matrix: wrote {}", path.display()),
            None => println!("{json}"),
        },
        Err(error) => {
            eprintln!("probe-matrix failed: {error}");
            shutdown_and_exit(2);
        }
    }
}

fn connect_options_from_cli_args(args: &[String]) -> Result<ConnectOptions, String> {
    let Some(host) = args.get(2).cloned() else {
        return Err("missing host".to_string());
    };
    let port = args
        .get(3)
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(18_444);
    let options = connect_options_from_parts(args, host, port)?;
    if options.microphone_enabled && args.get(1).map(String::as_str) != Some("media-smoke") {
        return Err("--microphone requires media-smoke for verified capture".to_string());
    }
    Ok(options)
}

fn quick_connect_options_from_cli_args(args: &[String]) -> Result<Option<ConnectOptions>, String> {
    let Some(connect_index) = args.iter().position(|arg| arg == "--connect") else {
        return Ok(None);
    };
    let host = args
        .get(connect_index + 1)
        .cloned()
        .ok_or_else(|| "--connect requires a host".to_string())?;
    let port = args
        .get(connect_index + 2)
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(18_444);
    connect_options_from_parts(args, host, port).map(Some)
}

fn parse_video_selection(
    value: &str,
) -> Result<arcen_protocol::messages::VideoSelectionIntent, String> {
    match value {
        "exact" => Ok(arcen_protocol::messages::VideoSelectionIntent::Exact),
        "adaptive-performance" | "adaptive_performance" => {
            Ok(arcen_protocol::messages::VideoSelectionIntent::AdaptivePerformance)
        }
        "color-fidelity" | "color_fidelity" | "colour-fidelity" | "colour_fidelity" => {
            Ok(arcen_protocol::messages::VideoSelectionIntent::ColorFidelity)
        }
        _ => Err(
            "--video-selection must be exact, adaptive-performance, or color-fidelity".to_string(),
        ),
    }
}

fn parse_pipeline(value: &str) -> Result<arcen_media::video::PipelineId, String> {
    match value {
        "auto" => Ok(arcen_media::video::PipelineId::Auto),
        "speed" => Ok(arcen_media::video::PipelineId::Speed),
        "grading" => Ok(arcen_media::video::PipelineId::Grading),
        "hdr" => Ok(arcen_media::video::PipelineId::Hdr),
        _ => Err("--pipeline must be auto, speed, grading, or hdr".to_string()),
    }
}

fn stream_profile_for_pipeline(pipeline: arcen_media::video::PipelineId) -> StreamProfile {
    let contract = arcen_media::video::pipeline_contract(pipeline);
    StreamProfile {
        codec: contract.colour.codec.token().to_string(),
        chroma: contract.colour.chroma.token().to_string(),
        video_selection: contract.selection,
        max_fps: contract.max_fps,
        bit_depth: contract.colour.bit_depth.token().to_string(),
        color_range: contract.colour.range.token().to_string(),
        color_matrix: contract.colour.matrix.token().to_string(),
        transfer: contract.colour.transfer.token().to_string(),
        color_primaries: contract.colour.primaries.token().to_string(),
        encode_intent: contract.intent.token().to_string(),
        motion_priority: contract.priority.token().to_string(),
        pipeline: contract.id.request_wire(),
    }
}

fn has_profile_override_flags(args: &[String]) -> bool {
    [
        "--codec",
        "--chroma",
        "--video-selection",
        "--max-fps",
        "--bit-depth",
        "--color-range",
        "--color-matrix",
        "--transfer",
        "--color-primaries",
        "--encode-intent",
        "--motion-priority",
    ]
    .iter()
    .any(|flag| flag_value(args, flag).is_some())
}

fn active_chroma_from_pixel_format(
    pixel_format: &str,
) -> Option<arcen_deck::protocol::ChromaSubsampling> {
    let value = pixel_format.to_ascii_lowercase();
    if value.contains("444") || value.starts_with("gbrp") {
        Some(arcen_deck::protocol::ChromaSubsampling::Yuv444)
    } else if value.contains("422") {
        Some(arcen_deck::protocol::ChromaSubsampling::Yuv422)
    } else if value.contains("420") || value.starts_with("p010") || value.starts_with("nv12") {
        Some(arcen_deck::protocol::ChromaSubsampling::Yuv420)
    } else {
        None
    }
}

fn connect_options_from_parts(
    args: &[String],
    host: String,
    port: u16,
) -> Result<ConnectOptions, String> {
    connect_options_from_parts_with_monitors(args, host, port, arcen_deck::display::enumerate())
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SmokeMonitorFixture {
    monitors: Vec<SmokeMonitor>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SmokeMonitor {
    id: u32,
    x: i32,
    y: i32,
    logical_width: u32,
    logical_height: u32,
    backing_width: u32,
    backing_height: u32,
    rotation_degrees: u16,
    #[serde(default)]
    safe_area_top: u32,
    #[serde(default)]
    safe_area_bottom: u32,
    #[serde(default)]
    safe_area_left: u32,
    #[serde(default)]
    safe_area_right: u32,
    refresh_hz: u32,
    primary: bool,
    name: String,
    width_mm: f32,
    height_mm: f32,
    vendor: u32,
    model: u32,
    serial: u32,
}

fn smoke_monitor_fixture(
    args: &[String],
) -> Result<Option<Vec<(ClientMonitor, DisplayMetrics)>>, String> {
    let Some(path) = flag_value(args, "--monitor-fixture") else {
        return Ok(None);
    };
    let metadata = std::fs::metadata(&path)
        .map_err(|error| format!("stat monitor fixture {path:?}: {error}"))?;
    if metadata.len() > 64 * 1024 {
        return Err("monitor fixture exceeds 64 KiB".to_string());
    }
    let payload =
        std::fs::read(&path).map_err(|error| format!("read monitor fixture {path:?}: {error}"))?;
    let fixture: SmokeMonitorFixture = serde_json::from_slice(&payload)
        .map_err(|error| format!("parse monitor fixture {path:?}: {error}"))?;
    if fixture.monitors.is_empty() {
        return Err("monitor fixture contains no displays".to_string());
    }
    fixture
        .monitors
        .into_iter()
        .map(|monitor| {
            let rotation = match monitor.rotation_degrees {
                0 => arcen_media::Rotation::Degrees0,
                90 => arcen_media::Rotation::Degrees90,
                180 => arcen_media::Rotation::Degrees180,
                270 => arcen_media::Rotation::Degrees270,
                other => return Err(format!("invalid fixture rotation {other}")),
            };
            let arrangement = LogicalRect::new(
                monitor.x,
                monitor.y,
                monitor.logical_width,
                monitor.logical_height,
            )
            .map_err(|error| format!("invalid fixture arrangement: {error}"))?;
            let metrics = DisplayMetrics::new(
                monitor.id,
                arrangement,
                monitor.backing_width,
                monitor.backing_height,
                rotation,
                SafeAreaInsets {
                    top: monitor.safe_area_top,
                    bottom: monitor.safe_area_bottom,
                    left: monitor.safe_area_left,
                    right: monitor.safe_area_right,
                },
            )
            .map_err(|error| format!("invalid fixture display {}: {error}", monitor.id))?;
            let stream = metrics.native_stream_extent();
            Ok((
                ClientMonitor {
                    id: monitor.id,
                    x: monitor.x,
                    y: monitor.y,
                    width_px: stream.width(),
                    height_px: stream.height(),
                    scale: metrics.scale().get(),
                    refresh_hz: monitor.refresh_hz,
                    is_primary: monitor.primary,
                    name: monitor.name,
                    width_mm: monitor.width_mm,
                    height_mm: monitor.height_mm,
                    vendor: monitor.vendor,
                    model: monitor.model,
                    serial: monitor.serial,
                    edid: String::new(),
                    color: None,
                },
                metrics,
            ))
        })
        .collect::<Result<Vec<_>, String>>()
        .map(Some)
}

fn connect_options_from_parts_with_monitors(
    args: &[String],
    host: String,
    port: u16,
    live_monitors: Vec<ClientMonitor>,
) -> Result<ConnectOptions, String> {
    let use_quic = true;
    let use_tls = true;
    if port == 0 {
        return Err("direct QUIC connections require a nonzero UDP port".to_string());
    }
    let (username, password) = credentials_from_args(args).map_err(|error| error.to_string())?;
    let insecure_skip_verify = args.iter().any(|arg| arg == "--insecure-skip-verify");

    let tls = if insecure_skip_verify {
        TlsTrustConfig::insecure_dev_escape_hatch(true)
    } else if let Some(ca_bundle) = flag_value(args, "--ca-bundle") {
        TlsTrustConfig::private_ca(ca_bundle.into())
    } else if let Some(fingerprint) = flag_value(args, "--pin-sha256") {
        TlsTrustConfig::pinned(parse_fingerprint(&fingerprint).map_err(|error| error.to_string())?)
    } else {
        TlsTrustConfig::system_ca()
    };

    let default_profile = StreamProfile::default();
    let requested_pipeline = flag_value(args, "--pipeline")
        .map(|value| parse_pipeline(&value))
        .transpose()?;
    let mut profile =
        requested_pipeline.map_or_else(|| default_profile.clone(), stream_profile_for_pipeline);
    if has_profile_override_flags(args) {
        profile.pipeline = None;
        profile.video_selection = arcen_protocol::messages::VideoSelectionIntent::Exact;
    }
    if let Some(value) = flag_value(args, "--codec") {
        profile.codec = value;
    }
    if let Some(value) = flag_value(args, "--chroma") {
        profile.chroma = value;
    }
    if let Some(value) = flag_value(args, "--video-selection") {
        profile.video_selection = parse_video_selection(&value)?;
    }
    if let Some(value) = flag_value(args, "--max-fps").and_then(|value| value.parse::<u32>().ok()) {
        profile.max_fps = value;
    }
    if let Some(value) = flag_value(args, "--bit-depth") {
        profile.bit_depth = value;
    }
    if let Some(value) = flag_value(args, "--color-range") {
        profile.color_range = value;
    }
    if let Some(value) = flag_value(args, "--color-matrix") {
        profile.color_matrix = value;
    }
    if let Some(value) = flag_value(args, "--transfer") {
        profile.transfer = value;
    }
    if let Some(value) = flag_value(args, "--color-primaries") {
        profile.color_primaries = value;
    }
    if let Some(value) = flag_value(args, "--encode-intent") {
        profile.encode_intent = value;
    }
    if let Some(value) = flag_value(args, "--motion-priority") {
        profile.motion_priority = value;
    }
    let fixture_displays = smoke_monitor_fixture(args)?;
    let monitors = fixture_displays.as_ref().map_or(live_monitors, |displays| {
        displays
            .iter()
            .map(|(monitor, _)| monitor.clone())
            .collect()
    });
    let displays_mode = displays_mode_from_args(args)?;
    // Smoke/quick-connect reproduces the shipped defaults: the notch-area
    // opt-in is a GUI setting, and HiDPI streaming is off unless the
    // diagnostic `--hidpi-displays` mask (bit 0 = first display) opts in;
    // Automatic UI scale then normalizes a point-sized stream to 1x/96 DPI
    // and a backing-pixel one to its backing ratio.
    let hidpi_mask = flag_value(args, "--hidpi-displays")
        .map(|value| {
            value
                .parse::<u8>()
                .map_err(|_| format!("--hidpi-displays expects a bit mask 0-255: {value}"))
        })
        .transpose()?
        .unwrap_or(0);
    let presentation = primary_presentation_size(&monitors, false, hidpi_mask & 1 != 0);
    let mut selected_monitors = monitors_for_displays_mode(displays_mode, &monitors, presentation);
    apply_automatic_remote_ui_scale_to_monitors(&mut selected_monitors, hidpi_mask);
    let multi_monitor_topology = (args.get(1).map(String::as_str) == Some("multi-monitor-smoke"))
        .then(|| {
            let mut topology = match fixture_displays.as_ref() {
                Some(displays) => arcen_deck::display::topology::build_requested_topology_from(
                    displays, false, hidpi_mask,
                ),
                None => arcen_deck::display::topology::build_requested_topology(false, hidpi_mask),
            }
            .map_err(|error| format!("multi-monitor topology preflight failed: {error}"))?;
            if std::env::var("ARCEN_MULTI_MONITOR_ROTATE_SECONDARY")
                .is_ok_and(|value| value == "180")
            {
                topology = rotate_secondary_for_smoke(&topology)?;
            }

            if topology.monitors().len() < 2 {
                return Err("multi-monitor-smoke requires at least two active displays".to_string());
            }
            topology = apply_automatic_remote_ui_scale_to_topology(&topology, hidpi_mask);
            let full_color_display_ids = match flag_value(args, "--full-color-display") {
                Some(value) if value.eq_ignore_ascii_case("all") => topology
                    .monitors()
                    .iter()
                    .map(|monitor| monitor.monitor().identity.id.clone())
                    .collect(),
                Some(value) => vec![value],
                None => Vec::new(),
            };
            Ok(
                arcen_deck::transport::multi_monitor::RequestedMultiMonitorSelection {
                    topology,
                    safe_area_policy: SafeAreaPolicyMsg::StandardFullscreen,
                    full_color_display_ids,
                },
            )
        })
        .transpose()?;
    if let Some(selection) = &multi_monitor_topology {
        for selected in &mut selected_monitors {
            let Some(requested) = selection
                .topology
                .monitors()
                .iter()
                .find(|requested| requested.monitor().identity.id == selected.id.to_string())
            else {
                continue;
            };
            selected.width_px = requested.monitor().width_px;
            selected.height_px = requested.monitor().height_px;
        }
    }

    let tablet_mode = flag_value(args, "--tablet-mode");
    let tablet_mode = match (tablet_mode.as_deref(), flag_value(args, "--usb-emulate")) {
        (None | Some("hard"), Some(_)) => Some("hard".to_owned()),
        (Some(other), Some(_)) => {
            return Err(format!(
                "--usb-emulate needs --tablet-mode hard, not '{other}'"
            ));
        }
        (_, None) => tablet_mode,
    };
    let tablet_mode_requested = match tablet_mode.as_deref() {
        Some("light") | None if !args.iter().any(|arg| arg == "--no-tablet-input") => {
            TabletModeMsg::LocalTermination
        }
        Some("hard") => TabletModeMsg::UsbTabletBridge,
        Some("off") | None => TabletModeMsg::DisabledMouseCompat,
        Some(other) => {
            return Err(format!(
                "invalid --tablet-mode '{other}'; accepted values: light, hard, off"
            ));
        }
    };
    Ok(ConnectOptions {
        host,
        port,
        use_tls,
        username,
        password,
        timeout: Duration::from_secs(30),
        tls,
        profile,
        monitors: selected_monitors,
        displays_mode: displays_mode.as_wire().to_string(),
        multi_monitor_topology,
        // The smoke commands never replace a running remote desktop: that is
        // an explicit interactive decision, not an automation default.
        replace_incompatible_desktop: false,
        timezone: arcen_deck::timezone::current_identifier(),
        cursor_preference: cursor_mode_from_args(args)?,
        clipboard_enabled: true,
        microphone_enabled: args.iter().any(|arg| arg == "--microphone"),
        tablet_input_enabled: !matches!(tablet_mode_requested, TabletModeMsg::DisabledMouseCompat),
        tablet_mode_requested,
        telemetry: std::sync::Arc::new(arcen_deck::observability::ClientTelemetry::default()),
        quic_enabled: use_quic,
    })
}

fn displays_mode_from_args(args: &[String]) -> Result<DisplaysMode, String> {
    let Some(value) = flag_value(args, "--displays-mode") else {
        return Ok(DisplaysMode::MatchLayout);
    };
    DisplaysMode::from_wire(&value).ok_or_else(|| {
        format!(
            "invalid --displays-mode '{value}'; accepted values: {}",
            DisplaysMode::WIRE_VALUES.join(", ")
        )
    })
}

fn cursor_mode_from_args(args: &[String]) -> Result<CursorMode, String> {
    match flag_value(args, "--cursor-mode").as_deref() {
        None | Some("local") => Ok(CursorMode::Local),
        Some("host") => Ok(CursorMode::Host),
        Some(value) => Err(format!(
            "invalid --cursor-mode '{value}'; accepted values: local, host"
        )),
    }
}

/// Host pointer positions seen by the emulated-pen smoke (a host that reads
/// the pen itself reports where it moved the pointer): every record is
/// printed, and the ones applied in sequence are counted.
#[cfg(feature = "usb-emulate-lab")]
#[derive(Default)]
struct SmokeHostPointer {
    pointer: arcen_input::host_pointer::DeckHostPointer,
    positions: u64,
    first_at: Option<(f64, f64)>,
    last_at: Option<(f64, f64)>,
}

#[cfg(feature = "usb-emulate-lab")]
impl SmokeHostPointer {
    /// Applies one `CursorPosition` record; returns its log line.
    fn record(&mut self, bytes: &[u8]) -> String {
        match self.pointer.apply(bytes) {
            Ok(Some(position)) => {
                self.positions += 1;
                let fraction = position.fraction();
                self.first_at = self.first_at.or(fraction);
                self.last_at = fraction;
                format!(
                    "cursor-position seq={} x={} y={} space={}x{} on_stream={}",
                    position.sequence,
                    position.x,
                    position.y,
                    position.space_width,
                    position.space_height,
                    position.on_stream
                )
            }
            Ok(None) => "cursor-position dropped: stale sequence".to_string(),
            Err(error) => format!("cursor-position dropped: {error}"),
        }
    }

    fn summary(&self) -> String {
        let fraction = |at: Option<(f64, f64)>| {
            at.map_or_else(|| "-".to_string(), |(x, y)| format!("{x:.3},{y:.3}"))
        };
        format!(
            "host_pointer_positions={} first={} last={} dropped={}",
            self.positions,
            fraction(self.first_at),
            fraction(self.last_at),
            self.pointer.dropped()
        )
    }
}

/// Holds the session while the emulated tablet plays its scripted strokes,
/// then closes it.
#[cfg(feature = "usb-emulate-lab")]
async fn emulated_pen_smoke(session: &mut SessionHandle) -> Result<(), String> {
    use arcen_deck::usb_emulate::{selected, status, PlaybackStatus};
    let (device, playback) = selected().ok_or_else(|| "no emulated USB device".to_string())?;
    let budget = Duration::from_secs(150)
        + arcen_usb_bridge::RESTING_HOLD
        + playback.lead_in
        + playback.gap * u32::try_from(playback.strokes).unwrap_or(u32::MAX);
    let deadline = tokio::time::Instant::now() + budget;
    let mut host_pointer = SmokeHostPointer::default();
    let outcome = loop {
        match status() {
            PlaybackStatus::Done { strokes, reports } => break Ok((strokes, reports)),
            PlaybackStatus::Failed(error) => break Err(error),
            _ if tokio::time::Instant::now() >= deadline => {
                break Err(format!(
                    "emulated playback did not finish within {budget:?}"
                ));
            }
            _ => {}
        }
        match tokio::time::timeout(Duration::from_millis(100), session.events.recv()).await {
            Ok(Some(SessionEvent::Ended(end))) => break Err(end.message),
            Ok(Some(SessionEvent::MediaReady)) => {
                let _ = session.media.take_batch();
            }
            // Where the host reports its pointer while the bridged pen
            // moves it (client-drawn cursor authority).
            Ok(Some(SessionEvent::CursorPosition(bytes))) => {
                println!("{} stroke={:?}", host_pointer.record(&bytes), status());
            }
            Ok(None) => break Err("session ended during emulated playback".to_string()),
            Ok(Some(_)) | Err(_) => {}
        }
    };
    // Let the host drain the last reports before the device detaches.
    tokio::time::sleep(Duration::from_millis(750)).await;
    close_smoke_session(session).await;
    let (strokes, reports) = outcome?;
    println!("input-smoke {}", host_pointer.summary());
    println!(
        "input-smoke complete usb_hard=true emulated={} strokes={strokes} reports={reports}",
        device.name()
    );
    Ok(())
}

async fn input_smoke(
    mut options: ConnectOptions,
    texts: Vec<String>,
    pen: Option<PenSmokeScript>,
    keys: KeysSmoke,
    paste: Option<PasteSmoke>,
) -> Result<(), String> {
    let hard_mode = options.tablet_mode_requested == TabletModeMsg::UsbTabletBridge;
    let mut text_capabilities = None;
    let mut host_hello = None;
    let mut auth = Some(AuthSubmission {
        username: std::mem::take(&mut options.username),
        password: std::mem::take(&mut options.password),
    });
    let mut session = spawn_session(options);
    let mut hello_ready = false;
    let mut hard_mode_ready = !hard_mode;
    while !(hello_ready && hard_mode_ready) {
        let event = tokio::time::timeout(Duration::from_secs(30), session.events.recv())
            .await
            .map_err(|_| "timed out waiting for server hello".to_string())?
            .ok_or_else(|| "session ended before server hello".to_string())?;
        match event {
            SessionEvent::ServerHello(hello) => {
                if !server_reports_remote_input(&hello) {
                    return Err(
                        "host reports remote input unavailable for this session".to_string()
                    );
                }
                if hard_mode && !hello.usb_hard_v1 {
                    return Err("host did not advertise Hard USB v1".to_string());
                }
                text_capabilities = Some((
                    hello.input_capabilities.text_commit,
                    hello.input_capabilities.text_commit_unicode,
                ));
                host_hello = Some(hello);
                hello_ready = true;
            }
            SessionEvent::Ended(end) => return Err(end.message),
            SessionEvent::MediaReady => {
                let _ = session.media.take_batch();
            }
            SessionEvent::AuthRequired(request) => {
                if request.disclaimer.is_some() {
                    return Err("DisclaimerRequired".to_string());
                }
                session
                    .commands
                    .send(SessionCommand::SubmitAuth(auth.take().ok_or_else(
                        || "host requested authentication twice".to_string(),
                    )?))
                    .map_err(|_| "session closed before authentication".to_string())?;
            }
            SessionEvent::Authenticated(_) => {
                session
                    .commands
                    .send(SessionCommand::AcceptAuthentication)
                    .map_err(|_| "session closed before authentication acceptance".to_string())?;
            }
            SessionEvent::CertificateUntrusted(_) => {
                return Err("server certificate requires interactive trust".to_string());
            }
            SessionEvent::Json(value) => {
                if hard_mode
                    && msg_type(&value) == Some(arcen_protocol::messages::CURSOR_MODE_RESULT)
                {
                    println!("cursor-mode-result {value}");
                }
                if hard_mode && msg_type(&value) == Some(TABLET_MODE_RESULT) {
                    let result = serde_json::from_value::<TabletModeResultMsg>(value)
                        .map_err(|error| error.to_string())?;
                    if !result.accepted || result.active != TabletModeMsg::UsbTabletBridge {
                        return Err(format!(
                            "Hard USB mode rejected: {}",
                            result.reason.as_str()
                        ));
                    }
                    println!("input-smoke tablet accepted: {}", result.reason.as_str());
                    hard_mode_ready = true;
                } else if msg_type(&value)
                    == Some(arcen_deck::protocol::messages::CURSOR_MODE_RESULT)
                {
                    if let Ok(result) = serde_json::from_value::<
                        arcen_deck::protocol::messages::CursorModeResultMsg,
                    >(value)
                    {
                        println!(
                            "input-smoke cursor active={:?} cursor_positions={}",
                            result.active, result.cursor_positions
                        );
                    }
                }
            }
            SessionEvent::MicrophoneActive(_)
            | SessionEvent::ConnectionStatusHint(_)
            | SessionEvent::CursorUpdate(_)
            | SessionEvent::CursorPosition(_)
            | SessionEvent::CursorLaneReset
            | SessionEvent::PresentationRecoveryRequested => {}
        }
    }

    tokio::time::sleep(Duration::from_millis(500)).await;
    #[cfg(feature = "usb-emulate-lab")]
    if hard_mode && arcen_deck::usb_emulate::selected().is_some() {
        return emulated_pen_smoke(&mut session).await;
    }
    if hard_mode {
        for sequence in 1_u64..=750 {
            let phase = (sequence % 100) as f64 / 100.0;
            session
                .commands
                .send(SessionCommand::HardUsbPen(arcen_input::PenEvent {
                    x: phase,
                    y: 1.0 - phase,
                    pressure: if sequence % 50 < 25 { 0.5 } else { 0.0 },
                    tilt_x_degrees: 20.0,
                    tilt_y_degrees: -10.0,
                    rotation_degrees: 0.0,
                    tool: arcen_input::PenTool::Tip,
                    in_proximity: true,
                    touching: sequence % 50 < 25,
                    buttons: 0,
                    metadata: arcen_input::LowLatencyMetadata {
                        sequence,
                        timestamp_ns: sequence,
                        coalescable: true,
                    },
                }))
                .map_err(|_| "session closed while sending Hard USB pen state".to_string())?;
            tokio::time::sleep(Duration::from_millis(8)).await;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        close_smoke_session(&mut session).await;
        println!("input-smoke complete usb_hard=true pen_states=750");
        return Ok(());
    }

    if let Some(paste) = paste {
        return paste_smoke(&mut session, &paste, text_capabilities).await;
    }
    if !texts.is_empty() {
        return text_smoke(&mut session, &texts, text_capabilities).await;
    }
    if let Some(script) = pen {
        return match script {
            PenSmokeScript::Strokes => pen_smoke(&mut session).await,
            PenSmokeScript::Full {
                wait_ms,
                end_in_contact,
            } => pen_full_smoke(&mut session, wait_ms, end_in_contact).await,
            PenSmokeScript::File { samples, go_file } => {
                pen_file_smoke(&mut session, &samples, go_file.as_deref()).await
            }
        };
    }
    if !keys.scripts.is_empty() {
        let hello = host_hello.ok_or_else(|| "no server hello".to_string())?;
        return keys_smoke(&mut session, &hello, &keys).await;
    }

    let events = [
        serde_json::json!({"type":"mouse_move","x":0.5,"y":0.5,"server_x":-1,"server_y":-1,"sequence":1,"timestamp_ns":0,"coalescable":true}),
        serde_json::json!({"type":"mouse_button","x":0.5,"y":0.5,"button":2,"pressed":true,"server_x":-1,"server_y":-1,"sequence":2,"timestamp_ns":0,"coalescable":false}),
        serde_json::json!({"type":"mouse_button","x":0.5,"y":0.5,"button":2,"pressed":false,"server_x":-1,"server_y":-1,"sequence":3,"timestamp_ns":0,"coalescable":false}),
        serde_json::json!({"type":"mouse_scroll","x":0.5,"y":0.5,"dx":0.0,"dy":-1.0,"server_x":-1,"server_y":-1,"sequence":4,"timestamp_ns":0,"coalescable":false}),
        serde_json::json!({"type":"mouse_scroll","x":0.5,"y":0.5,"dx":1.0,"dy":0.0,"server_x":-1,"server_y":-1,"sequence":5,"timestamp_ns":0,"coalescable":false}),
        serde_json::json!({"type":"key_event","scan_code":16777248,"pressed":true,"modifiers":1,"server_x":-1,"server_y":-1,"sequence":6,"timestamp_ns":0,"coalescable":false}),
        serde_json::json!({"type":"key_event","scan_code":16777248,"pressed":false,"modifiers":1,"server_x":-1,"server_y":-1,"sequence":7,"timestamp_ns":0,"coalescable":false}),
        serde_json::json!({"type":"key_event","scan_code":16777249,"pressed":true,"modifiers":2,"server_x":-1,"server_y":-1,"sequence":8,"timestamp_ns":0,"coalescable":false}),
        serde_json::json!({"type":"key_reset_modifiers","reason":"input-smoke held-state teardown"}),
    ];
    for event in events {
        session
            .commands
            .send(SessionCommand::Json(event))
            .map_err(|_| "session closed while sending input".to_string())?;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    close_smoke_session(&mut session).await;
    println!("input-smoke complete input_events=8 reset_events=1 keyboard=3 mouse=3 scroll=2");
    Ok(())
}

/// Lab (`input-smoke --keys SCRIPT`): physical Mac keystrokes through the
/// same shared keyboard policy the Deck window applies, resolved from this
/// host's hello and the given connection settings.
#[derive(Debug)]
struct KeysSmoke {
    scripts: Vec<String>,
    /// Never print the tokens or key edges (`--keys-file`: a script that
    /// may type a secret, such as a password into a streamed login window).
    quiet: bool,
    settings: arcen_input::keyboard::KeyboardSettings,
}

/// A script token's physical key: `cmd`, `ctrl`, `shift`, `lopt`, `ropt`,
/// `iso` (the key next to left Shift), `grave`, a letter or digit, or a
/// named key.
fn smoke_deck_key(
    name: &str,
    policy: arcen_input::keyboard::KeyboardPolicy,
) -> Option<arcen_input::keyboard::DeckKey> {
    use arcen_input::keyboard::DeckKey;
    Some(match name {
        "cmd" => DeckKey::Command,
        "ctrl" => DeckKey::Control,
        "shift" => DeckKey::Shift,
        "lopt" => DeckKey::LeftOption,
        "ropt" => DeckKey::RightOption,
        "iso" if policy.intl_backslash => DeckKey::Other(arcen_protocol::keys::KEY_INTL_BACKSLASH),
        "iso" | "grave" => DeckKey::Other(0x60),
        "minus" => DeckKey::Other(0x2D),
        "equal" => DeckKey::Other(0x3D),
        "lbracket" => DeckKey::Other(0x5B),
        "rbracket" => DeckKey::Other(0x5D),
        "backslash" => DeckKey::Other(0x5C),
        "semicolon" => DeckKey::Other(0x3B),
        "quote" => DeckKey::Other(0x27),
        "comma" => DeckKey::Other(0x2C),
        "period" => DeckKey::Other(0x2E),
        "slash" => DeckKey::Other(0x2F),
        "space" => DeckKey::Other(0x20),
        "enter" => DeckKey::Other(0x0100_0004),
        "esc" => DeckKey::Other(0x0100_0000),
        "tab" => DeckKey::Other(0x0100_0001),
        "backspace" => DeckKey::Other(0x0100_0003),
        "home" => DeckKey::Other(0x0100_0010),
        "end" => DeckKey::Other(0x0100_0011),
        "left" => DeckKey::Other(0x0100_0012),
        "up" => DeckKey::Other(0x0100_0013),
        "right" => DeckKey::Other(0x0100_0014),
        "down" => DeckKey::Other(0x0100_0015),
        _ => {
            let [byte] = name.as_bytes() else {
                return None;
            };
            match byte.to_ascii_uppercase() {
                letter @ (b'A'..=b'Z' | b'0'..=b'9') => DeckKey::Other(u32::from(letter)),
                _ => return None,
            }
        }
    })
}

async fn keys_smoke(
    session: &mut SessionHandle,
    hello: &ServerHelloMsg,
    keys: &KeysSmoke,
) -> Result<(), String> {
    use arcen_input::keyboard::{DeckKey, DeckModifiers, ShortcutTranslator};
    use arcen_input::text::OptionSide;

    let platform = hello.host_platform();
    let extended_keys =
        arcen_protocol::messages::supports_extended_keys_v1(hello.input_capabilities);
    let host_meta_mask =
        arcen_protocol::messages::supports_meta_pointer_mask_v1(hello.input_capabilities);
    let policy =
        arcen_input::keyboard::resolve_keyboard_policy(platform, extended_keys, keys.settings)
            .with_host_meta_mask(host_meta_mask);
    println!(
        "input-smoke keys platform={platform:?} extended_keys={extended_keys} policy={policy:?}"
    );
    let mut translator = ShortcutTranslator::new(policy);
    let mut held = DeckModifiers::default();
    let mut sequence = 1_000_u64;
    let mut sent = 0_usize;
    let mut position = 0_usize;
    for script in &keys.scripts {
        for token in script.split_whitespace() {
            position += 1;
            // A quiet script may type a secret: errors name the token's
            // position, never its text.
            let named = |token: &str| {
                if keys.quiet {
                    format!("token #{position}")
                } else {
                    token.to_owned()
                }
            };
            if let Some(millis) = token.strip_prefix("sleep:") {
                let millis = millis
                    .parse::<u64>()
                    .map_err(|_| format!("bad sleep {}", named(token)))?;
                tokio::time::sleep(Duration::from_millis(millis)).await;
                continue;
            }
            let mut edges = Vec::new();
            let mut reset = false;
            let mut click = None;
            let mut scroll = false;
            if token == "scroll" {
                edges = translator.settle_command_for_pointer();
                scroll = true;
            } else if token == "click" {
                // A left click at the centre, after settling a held-back
                // Command as the Deck window does before pointer input.
                edges = translator.settle_command_for_pointer();
                click = Some((0.5, 0.5));
            } else if let Some(at) = token.strip_prefix("click:") {
                // `click:X,Y`: a left click at that fraction of the desktop.
                let (x, y) = at
                    .split_once(',')
                    .and_then(|(x, y)| Some((x.parse::<f64>().ok()?, y.parse::<f64>().ok()?)))
                    .filter(|(x, y)| (0.0..=1.0).contains(x) && (0.0..=1.0).contains(y))
                    .ok_or_else(|| format!("bad click {}", named(token)))?;
                edges = translator.settle_command_for_pointer();
                click = Some((x, y));
            } else if token == "reset" {
                edges = translator.release_all();
                held = DeckModifiers::default();
                reset = true;
            } else {
                let (phases, name): (&[bool], &str) = match token.as_bytes().first() {
                    Some(b'+') => (&[true], &token[1..]),
                    Some(b'-') => (&[false], &token[1..]),
                    _ => (&[true, false], token),
                };
                let key = smoke_deck_key(name, policy)
                    .ok_or_else(|| format!("unknown key in {}", named(token)))?;
                for &pressed in phases {
                    match key {
                        DeckKey::Command => held.command = pressed,
                        DeckKey::Control => held.control = pressed,
                        DeckKey::Shift => held.shift = pressed,
                        DeckKey::LeftOption | DeckKey::RightOption => {
                            let left = matches!(held.option, OptionSide::Left | OptionSide::Both);
                            let right = matches!(held.option, OptionSide::Right | OptionSide::Both);
                            let (left, right) = if key == DeckKey::LeftOption {
                                (pressed, right)
                            } else {
                                (left, pressed)
                            };
                            held.option = match (left, right) {
                                (false, false) => OptionSide::None,
                                (true, false) => OptionSide::Left,
                                (false, true) => OptionSide::Right,
                                (true, true) => OptionSide::Both,
                            };
                        }
                        DeckKey::Other(_) => {}
                    }
                    edges.extend(translator.key(key, pressed, false, held));
                }
            }
            let described: Vec<String> = edges
                .iter()
                .map(|edge| {
                    format!(
                        "{}{:#x}/{:#04x}",
                        if edge.pressed { '+' } else { '-' },
                        edge.qt_key,
                        edge.modifiers
                    )
                })
                .collect();
            if !keys.quiet {
                println!("input-smoke key {token} -> [{}]", described.join(" "));
            }
            for edge in edges {
                sequence += 1;
                session
                    .commands
                    .send(SessionCommand::Json(serde_json::json!({
                        "type": "key_event",
                        "scan_code": edge.qt_key,
                        "pressed": edge.pressed,
                        "modifiers": edge.modifiers,
                        "server_x": -1,
                        "server_y": -1,
                        "sequence": sequence,
                        "timestamp_ns": 0,
                        "coalescable": false,
                    })))
                    .map_err(|_| "session closed while sending keys".to_string())?;
                sent += 1;
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
            if scroll {
                sequence += 1;
                session
                    .commands
                    .send(SessionCommand::Json(serde_json::json!({
                        "type": "mouse_scroll",
                        "x": 0.5,
                        "y": 0.5,
                        "dx": 0.0,
                        "dy": -1.0,
                        "server_x": -1,
                        "server_y": -1,
                        "sequence": sequence,
                        "timestamp_ns": 0,
                        "coalescable": false,
                    })))
                    .map_err(|_| "session closed while scrolling".to_string())?;
            }
            if let Some((x, y)) = click {
                sequence += 1;
                session
                    .commands
                    .send(SessionCommand::Json(serde_json::json!({
                        "type": "mouse_move", "x": x, "y": y, "server_x": -1, "server_y": -1,
                        "sequence": sequence, "timestamp_ns": 0, "coalescable": false,
                    })))
                    .map_err(|_| "session closed while clicking".to_string())?;
                tokio::time::sleep(Duration::from_millis(60)).await;
                for pressed in [true, false] {
                    sequence += 1;
                    session
                        .commands
                        .send(SessionCommand::Json(serde_json::json!({
                            "type": "mouse_button",
                            "x": x,
                            "y": y,
                            "button": 1,
                            "pressed": pressed,
                            "server_x": -1,
                            "server_y": -1,
                            "sequence": sequence,
                            "timestamp_ns": 0,
                            "coalescable": false,
                        })))
                        .map_err(|_| "session closed while clicking".to_string())?;
                    tokio::time::sleep(Duration::from_millis(40)).await;
                }
            }
            if reset {
                session
                    .commands
                    .send(SessionCommand::Json(serde_json::json!({
                        "type": "key_reset_modifiers",
                        "reason": "input-smoke keys reset",
                    })))
                    .map_err(|_| "session closed while resetting keys".to_string())?;
            }
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    close_smoke_session(session).await;
    println!("input-smoke complete key_events={sent}");
    Ok(())
}

/// Lab (`input-smoke --pen`): typed pen strokes (`pen_event`) across the
/// host desktop, each with a pressure ramp and tilt, then out of proximity.
async fn pen_smoke(session: &mut SessionHandle) -> Result<(), String> {
    let mut sent = 0_u32;
    let strokes = [
        ((0.15, 0.2), (0.85, 0.8), 45.0_f32, -30.0_f32),
        ((0.85, 0.2), (0.15, 0.8), -45.0, 30.0),
    ];
    for ((x0, y0), (x1, y1), tilt_x, tilt_y) in strokes {
        for step in 0..=120_u32 {
            let t = f64::from(step) / 120.0;
            let touching = (1..120).contains(&step);
            let pressure = if touching {
                (std::f64::consts::PI * t).sin() as f32 * 0.95 + 0.02
            } else {
                0.0
            };
            let pen = serde_json::json!({
                "type": "pen_event",
                "x": x0 + (x1 - x0) * t,
                "y": y0 + (y1 - y0) * t,
                "server_x": -1,
                "server_y": -1,
                "pressure": pressure,
                "tilt_x_degrees": tilt_x * (std::f64::consts::PI * t).cos() as f32,
                "tilt_y_degrees": tilt_y * (std::f64::consts::PI * t).sin() as f32,
                "rotation_degrees": 0.0,
                "tool": "tip",
                "in_proximity": true,
                "touching": touching,
                "buttons": 0,
            });
            session
                .commands
                .send(SessionCommand::Json(pen))
                .map_err(|_| "session closed while sending pen".to_string())?;
            sent += 1;
            tokio::time::sleep(Duration::from_millis(8)).await;
        }
        let away = serde_json::json!({
            "type": "pen_event", "x": x1, "y": y1, "server_x": -1, "server_y": -1,
            "pressure": 0.0, "tool": "tip", "in_proximity": false, "touching": false, "buttons": 0,
        });
        session
            .commands
            .send(SessionCommand::Json(away))
            .map_err(|_| "session closed while sending pen".to_string())?;
        sent += 1;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    close_smoke_session(session).await;
    println!("input-smoke complete pen_events={sent}");
    Ok(())
}

/// Which typed-pen script `input-smoke` sends.
#[derive(Clone, Debug, PartialEq)]
enum PenSmokeScript {
    /// `--pen`: two tilted strokes with a pressure ramp.
    Strokes,
    /// `--pen-full [--pen-wait-ms MS]`: every pen state a host must honour,
    /// with fixed values a host-side probe can check (`pen_full_script`),
    /// after waiting `wait_ms` for the remote desktop to settle.
    /// `--pen-end-in-contact` closes the session with the tip down.
    Full { wait_ms: u64, end_in_contact: bool },
    /// `--pen-file PATH [--pen-go-file GO]`: samples from a file
    /// (`parse_pen_file`), so a lab script can keep every point inside a
    /// window it has checked. With `GO`, nothing is sent until that file
    /// reads `go` (the script checks the host's windows while this session
    /// is live, since a session can rearrange them); any other content, or
    /// none within 60 s, ends the session without a sample.
    File {
        samples: Vec<(PenScriptSample, u64)>,
        go_file: Option<String>,
    },
}

/// Parses a `--pen-file` script: one sample per line,
/// `x y pressure tilt_x tilt_y rotation tip|eraser in_proximity touching buttons pause_ms`
/// (`x`/`y` normalized 0..=1, flags 0/1); blank lines and `#` comments are
/// skipped. Every line is checked before anything is sent.
fn parse_pen_file(text: &str) -> Result<Vec<(PenScriptSample, u64)>, String> {
    let mut samples = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = || format!("line {}: expected 11 fields", index + 1);
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [x, y, pressure, tilt_x, tilt_y, rotation, tool, prox, touch, buttons, pause] =
            fields.as_slice()
        else {
            return Err(bad());
        };
        let number = |value: &str| value.parse::<f64>().map_err(|_| bad());
        let flag = |value: &str| match value {
            "0" => Ok(false),
            "1" => Ok(true),
            _ => Err(bad()),
        };
        let (x, y) = (number(x)?, number(y)?);
        if !(0.0..=1.0).contains(&x) || !(0.0..=1.0).contains(&y) {
            return Err(format!("line {}: x and y must be within 0..=1", index + 1));
        }
        let sample = PenScriptSample {
            x,
            y,
            pressure: number(pressure)? as f32,
            tilt_x_degrees: number(tilt_x)? as f32,
            tilt_y_degrees: number(tilt_y)? as f32,
            rotation_degrees: number(rotation)? as f32,
            eraser: match *tool {
                "tip" => false,
                "eraser" => true,
                _ => return Err(bad()),
            },
            in_proximity: flag(prox)?,
            touching: flag(touch)?,
            buttons: buttons.parse().map_err(|_| bad())?,
        };
        samples.push((sample, pause.parse().map_err(|_| bad())?));
    }
    if samples.is_empty() {
        return Err("no samples".to_string());
    }
    Ok(samples)
}

/// Lab (`input-smoke --pen-file PATH`): sends the file's samples with their
/// pauses and increasing sequence numbers.
async fn pen_file_smoke(
    session: &mut SessionHandle,
    samples: &[(PenScriptSample, u64)],
    go_file: Option<&str>,
) -> Result<(), String> {
    if let Some(path) = go_file {
        println!("input-smoke pen waiting for go file");
        let mut verdict = None;
        for _ in 0..600 {
            if let Ok(text) = std::fs::read_to_string(path) {
                if !text.trim().is_empty() {
                    verdict = Some(text.trim().to_string());
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if verdict.as_deref() != Some("go") {
            close_smoke_session(session).await;
            return Err(format!("pen not sent: go file said {verdict:?}"));
        }
    }
    for (index, (sample, pause)) in samples.iter().enumerate() {
        let sequence = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
        session
            .commands
            .send(SessionCommand::Json(sample.json(sequence)))
            .map_err(|_| "session closed while sending pen".to_string())?;
        tokio::time::sleep(Duration::from_millis(*pause)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    close_smoke_session(session).await;
    println!("input-smoke complete pen_file_events={}", samples.len());
    Ok(())
}

/// One `pen_event` of the `--pen-full` script.
#[derive(Clone, Copy, Debug, PartialEq)]
struct PenScriptSample {
    x: f64,
    y: f64,
    pressure: f32,
    tilt_x_degrees: f32,
    tilt_y_degrees: f32,
    rotation_degrees: f32,
    eraser: bool,
    in_proximity: bool,
    touching: bool,
    buttons: u16,
}

impl PenScriptSample {
    const fn out_of_range(x: f64, y: f64) -> Self {
        Self {
            x,
            y,
            pressure: 0.0,
            tilt_x_degrees: 0.0,
            tilt_y_degrees: 0.0,
            rotation_degrees: 0.0,
            eraser: false,
            in_proximity: false,
            touching: false,
            buttons: 0,
        }
    }

    fn json(self, sequence: u64) -> serde_json::Value {
        serde_json::json!({
            "type": "pen_event",
            "x": self.x,
            "y": self.y,
            "server_x": -1,
            "server_y": -1,
            "pressure": self.pressure,
            "tilt_x_degrees": self.tilt_x_degrees,
            "tilt_y_degrees": self.tilt_y_degrees,
            "rotation_degrees": self.rotation_degrees,
            "tool": if self.eraser { "eraser" } else { "tip" },
            "in_proximity": self.in_proximity,
            "touching": self.touching,
            "buttons": self.buttons,
            "sequence": sequence,
            "timestamp_ns": 0,
            "coalescable": true,
        })
    }
}

/// The `--pen-full` script, in phases a host probe can tell apart by
/// position (each phase has its own row):
/// 1. hover only, y 0.30: in range, never touching, tilt (+20, -10);
/// 2. tip stroke, y 0.45: pressure 0.10 → 1.00 linear, tilt (+40, +25),
///    rotation 90, then a lift that stays in range;
/// 3. barrel stroke, y 0.55: button bit 0, pressure 0.50, tilt (-40, -25);
/// 4. eraser, y 0.65: the eraser end hovers, then rubs at pressure 0.70;
/// 5. second button tap at (0.50, 0.75): button bit 1, pressure 0.30;
/// 6. corner taps at (0, 0), (1, 1) and (0.25, 0.85), pressures 0.25, 0.50
///    and 0.75, for the coordinate mapping;
/// 7. a short stroke at y 0.92 whose lift lands 0.05 further right, as a
///    real pen lifting mid-motion does.
///
/// Every phase ends out of range, unless `end_in_contact` adds a final
/// stroke at y 0.95 that never lifts (the session closes under the pen).
fn pen_full_script(end_in_contact: bool) -> Vec<PenScriptSample> {
    const STEPS: u32 = 60;
    let mut samples = Vec::new();
    let row = |samples: &mut Vec<PenScriptSample>, y: f64, template: PenScriptSample| {
        for step in 0..=STEPS {
            let t = f64::from(step) / f64::from(STEPS);
            let mut sample = template;
            sample.x = 0.2 + 0.6 * t;
            sample.y = y;
            if template.touching && template.pressure < 0.0 {
                // A negative template pressure asks for the linear ramp.
                sample.pressure = (0.1 + 0.9 * t) as f32;
            }
            samples.push(sample);
        }
    };
    let hover = PenScriptSample {
        pressure: 0.0,
        tilt_x_degrees: 20.0,
        tilt_y_degrees: -10.0,
        in_proximity: true,
        ..PenScriptSample::out_of_range(0.0, 0.0)
    };
    row(&mut samples, 0.30, hover);
    samples.push(PenScriptSample::out_of_range(0.8, 0.30));

    let tip = PenScriptSample {
        pressure: -1.0,
        tilt_x_degrees: 40.0,
        tilt_y_degrees: 25.0,
        rotation_degrees: 90.0,
        in_proximity: true,
        touching: true,
        ..PenScriptSample::out_of_range(0.0, 0.0)
    };
    samples.push(PenScriptSample {
        x: 0.2,
        y: 0.45,
        pressure: 0.0,
        touching: false,
        ..tip
    });
    row(&mut samples, 0.45, tip);
    samples.push(PenScriptSample {
        x: 0.8,
        y: 0.45,
        pressure: 0.0,
        touching: false,
        ..tip
    });
    samples.push(PenScriptSample::out_of_range(0.8, 0.45));

    let barrel = PenScriptSample {
        pressure: 0.5,
        tilt_x_degrees: -40.0,
        tilt_y_degrees: -25.0,
        in_proximity: true,
        touching: true,
        buttons: 0b01,
        ..PenScriptSample::out_of_range(0.0, 0.0)
    };
    row(&mut samples, 0.55, barrel);
    samples.push(PenScriptSample::out_of_range(0.8, 0.55));

    let eraser_hover = PenScriptSample {
        eraser: true,
        in_proximity: true,
        ..PenScriptSample::out_of_range(0.0, 0.0)
    };
    for step in 0..5 {
        samples.push(PenScriptSample {
            x: 0.2,
            y: 0.65 - 0.01 * f64::from(5 - step),
            ..eraser_hover
        });
    }
    row(
        &mut samples,
        0.65,
        PenScriptSample {
            pressure: 0.7,
            touching: true,
            ..eraser_hover
        },
    );
    samples.push(PenScriptSample {
        x: 0.8,
        y: 0.65,
        ..eraser_hover
    });
    samples.push(PenScriptSample {
        eraser: true,
        ..PenScriptSample::out_of_range(0.8, 0.65)
    });

    let tap = |samples: &mut Vec<PenScriptSample>, x: f64, y: f64, pressure: f32, buttons: u16| {
        let hover = PenScriptSample {
            in_proximity: true,
            buttons,
            ..PenScriptSample::out_of_range(x, y)
        };
        samples.push(hover);
        for _ in 0..3 {
            samples.push(PenScriptSample {
                pressure,
                touching: true,
                ..hover
            });
        }
        samples.push(hover);
        samples.push(PenScriptSample::out_of_range(x, y));
    };
    tap(&mut samples, 0.5, 0.75, 0.3, 0b10);
    tap(&mut samples, 0.0, 0.0, 0.25, 0);
    tap(&mut samples, 1.0, 1.0, 0.5, 0);
    tap(&mut samples, 0.25, 0.85, 0.75, 0);

    let moving = PenScriptSample {
        pressure: 0.6,
        in_proximity: true,
        touching: true,
        ..PenScriptSample::out_of_range(0.0, 0.0)
    };
    for step in 0..=10 {
        samples.push(PenScriptSample {
            x: 0.40 + 0.01 * f64::from(step),
            y: 0.92,
            ..moving
        });
    }
    samples.push(PenScriptSample {
        x: 0.55,
        y: 0.92,
        pressure: 0.0,
        touching: false,
        ..moving
    });
    samples.push(PenScriptSample::out_of_range(0.55, 0.92));

    if end_in_contact {
        for step in 0..=10 {
            samples.push(PenScriptSample {
                x: 0.60 + 0.01 * f64::from(step),
                y: 0.95,
                ..moving
            });
        }
    }
    samples
}

/// Lab (`input-smoke --pen-full`): sends `pen_full_script`, 8 ms apart,
/// with increasing sequence numbers.
async fn pen_full_smoke(
    session: &mut SessionHandle,
    wait_ms: u64,
    end_in_contact: bool,
) -> Result<(), String> {
    let script = pen_full_script(end_in_contact);
    tokio::time::sleep(Duration::from_millis(wait_ms)).await;
    for (index, sample) in script.iter().enumerate() {
        let sequence = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
        session
            .commands
            .send(SessionCommand::Json(sample.json(sequence)))
            .map_err(|_| "session closed while sending pen".to_string())?;
        let pause = if sample.in_proximity { 8 } else { 250 };
        tokio::time::sleep(Duration::from_millis(pause)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    close_smoke_session(session).await;
    println!("input-smoke complete pen_full_events={}", script.len());
    Ok(())
}

/// Lab (`input-smoke --paste-file PATH`): Paste as Keystrokes of a file's
/// text, through the same shared planner and pacing as the Deck's menu
/// command. Prints counts only, never the text.
struct PasteSmoke {
    path: String,
    rate: Option<u32>,
    /// Waits this long after the hello (a lab script starts the target
    /// application meanwhile).
    wait_ms: u64,
    /// Clicks the centre of the desktop before typing.
    click: bool,
    /// Presses Return after the text (a paste never types a final line
    /// break itself).
    return_after: bool,
}

async fn paste_smoke(
    session: &mut SessionHandle,
    paste: &PasteSmoke,
    capabilities: Option<(InputCapabilityAvailability, InputCapabilityAvailability)>,
) -> Result<(), String> {
    use arcen_deck::protocol::messages::{TextCommitMsg, TextCommitResultMsg, TEXT_COMMIT_RESULT};
    use arcen_input::text::{prepare_paste, HostTextTruth, PasteSettings, PasteTyping, PasteUnit};
    let (text_commit, unicode) = capabilities.ok_or("no server hello")?;
    let text = zeroize::Zeroizing::new(
        std::fs::read_to_string(&paste.path).map_err(|error| format!("--paste-file: {error}"))?,
    );
    let host = HostTextTruth {
        text_commit: text_commit == InputCapabilityAvailability::Available,
        unicode: unicode == InputCapabilityAvailability::Available,
    };
    let settings = PasteSettings {
        chars_per_second: paste
            .rate
            .unwrap_or(arcen_input::text::paste::DEFAULT_CHARS_PER_SECOND),
        ..PasteSettings::default()
    }
    .clamped();
    println!(
        "input-smoke paste host text_commit={text_commit:?} text_commit_unicode={unicode:?} rate={}",
        settings.chars_per_second
    );
    let prepared = prepare_paste(&text, host).map_err(|refusal| refusal.message())?;
    let summary = prepared.summary();
    println!(
        "input-smoke paste items={} keys={} dropped={} trailing_breaks={}",
        summary.items, summary.keys, summary.dropped, summary.trailing_breaks
    );
    // A first input reaches the session, so its layout is read, and every
    // key is released, as the Deck does before typing.
    for json in [
        serde_json::json!({"type":"mouse_move","x":0.5,"y":0.5,"server_x":-1,"server_y":-1,"sequence":1,"timestamp_ns":0,"coalescable":true}),
        serde_json::json!({"type":"key_reset_modifiers","reason":"input-smoke paste"}),
    ] {
        session
            .commands
            .send(SessionCommand::Json(json))
            .map_err(|_| "session closed while sending input".to_string())?;
    }
    tokio::time::sleep(Duration::from_millis(500 + paste.wait_ms)).await;
    if paste.click {
        for (sequence, pressed) in [(2_u64, true), (3, false)] {
            session
                .commands
                .send(SessionCommand::Json(serde_json::json!({
                    "type": "mouse_button", "x": 0.5, "y": 0.5, "button": 1,
                    "pressed": pressed, "server_x": -1, "server_y": -1,
                    "sequence": sequence, "timestamp_ns": 0, "coalescable": false,
                })))
                .map_err(|_| "session closed while clicking".to_string())?;
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let mut typing = PasteTyping::new(prepared, settings);
    let started = std::time::Instant::now();
    let (mut sequence, mut commits, mut keys, mut refused) = (1000_u64, 0_u32, 0_u32, 0_u32);
    let mut unverified = 0_u32;
    loop {
        let poll = typing.poll(std::time::Instant::now(), false);
        for unit in poll.units {
            match unit {
                PasteUnit::Text(run) => {
                    sequence += 1;
                    commits += 1;
                    let message = TextCommitMsg::new(run, sequence);
                    session
                        .commands
                        .send(SessionCommand::Json(
                            serde_json::to_value(message).map_err(|error| error.to_string())?,
                        ))
                        .map_err(|_| "session closed while typing".to_string())?;
                }
                PasteUnit::Key(key) => {
                    keys += 1;
                    for pressed in [true, false] {
                        sequence += 1;
                        session
                            .commands
                            .send(SessionCommand::Json(serde_json::json!({
                                "type": "key_event",
                                "scan_code": key.qt_key(),
                                "pressed": pressed,
                                "modifiers": 0,
                                "server_x": -1,
                                "server_y": -1,
                                "sequence": sequence,
                                "timestamp_ns": 0,
                                "coalescable": false,
                            })))
                            .map_err(|_| "session closed while typing".to_string())?;
                    }
                }
            }
        }
        while let Ok(event) = session.events.try_recv() {
            if let SessionEvent::Json(value) = event {
                if msg_type(&value) == Some(TEXT_COMMIT_RESULT) {
                    if let Ok(result) = serde_json::from_value::<TextCommitResultMsg>(value) {
                        refused += result.refused_chars;
                        unverified += u32::from(result.unverified);
                    }
                }
            }
        }
        let Some(after) = poll.wake_after else {
            break;
        };
        tokio::time::sleep(after.max(Duration::from_millis(1))).await;
    }
    let elapsed = started.elapsed();
    if paste.return_after {
        tokio::time::sleep(Duration::from_millis(300)).await;
        for pressed in [true, false] {
            sequence += 1;
            session
                .commands
                .send(SessionCommand::Json(serde_json::json!({
                    "type": "key_event", "scan_code": arcen_protocol::keys::KEY_RETURN,
                    "pressed": pressed, "modifiers": 0, "server_x": -1, "server_y": -1,
                    "sequence": sequence, "timestamp_ns": 0, "coalescable": false,
                })))
                .map_err(|_| "session closed while typing".to_string())?;
        }
        println!("input-smoke paste return pressed");
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    while let Ok(event) = session.events.try_recv() {
        if let SessionEvent::Json(value) = event {
            if msg_type(&value) == Some(TEXT_COMMIT_RESULT) {
                if let Ok(result) = serde_json::from_value::<TextCommitResultMsg>(value) {
                    refused += result.refused_chars;
                    unverified += u32::from(result.unverified);
                }
            }
        }
    }
    close_smoke_session(session).await;
    println!(
        "input-smoke complete paste_items={} text_commits={commits} keys={keys} refused_chars={refused} unverified_commits={unverified} elapsed_ms={}",
        typing.progress().1,
        elapsed.as_millis()
    );
    Ok(())
}

/// Sends each `--text` as committed text, the way the Deck sends its input
/// method's commits, and prints what the host reported: its layout and every
/// refusal (the characters and why). Text is printed only as code points.
async fn text_smoke(
    session: &mut SessionHandle,
    texts: &[String],
    capabilities: Option<(InputCapabilityAvailability, InputCapabilityAvailability)>,
) -> Result<(), String> {
    use arcen_deck::protocol::messages::{
        HostKeymapMsg, TextCommitMsg, TextCommitResultMsg, HOST_KEYMAP, TEXT_COMMIT_RESULT,
    };
    let (text_commit, unicode) = capabilities.ok_or("no server hello")?;
    println!("input-smoke text host text_commit={text_commit:?} text_commit_unicode={unicode:?}");
    if text_commit != InputCapabilityAvailability::Available {
        return Err("host does not advertise committed text".to_string());
    }
    let code_points = |text: &str| {
        text.chars()
            .map(|character| format!("U+{:04X}", u32::from(character)))
            .collect::<Vec<_>>()
            .join(" ")
    };
    // A first input reaches the session, so its layout is read and sent.
    session
        .commands
        .send(SessionCommand::Json(serde_json::json!({"type":"mouse_move","x":0.5,"y":0.5,"server_x":-1,"server_y":-1,"sequence":1,"timestamp_ns":0,"coalescable":true})))
        .map_err(|_| "session closed while sending input".to_string())?;
    let mut refused = 0_u32;
    let mut results = 0_u32;
    let drain = |session: &mut SessionHandle, refused: &mut u32, results: &mut u32| {
        while let Ok(event) = session.events.try_recv() {
            let SessionEvent::Json(value) = event else {
                continue;
            };
            match msg_type(&value) {
                Some(HOST_KEYMAP) => {
                    if let Ok(keymap) = serde_json::from_value::<HostKeymapMsg>(value) {
                        println!(
                            "input-smoke host_keymap generation={} layouts={:?} active={:?} positions={}",
                            keymap.generation,
                            keymap.layouts,
                            keymap.active_layout,
                            keymap.keys.len()
                        );
                    }
                }
                Some(TEXT_COMMIT_RESULT) => {
                    if let Ok(result) = serde_json::from_value::<TextCommitResultMsg>(value) {
                        *results += 1;
                        *refused += result.refused_chars;
                        println!(
                            "input-smoke text_commit_result sequence={} delivered={} refused={} reason={} unverified={} refused_code_points=[{}]",
                            result.sequence,
                            result.delivered_chars,
                            result.refused_chars,
                            result.reason.token(),
                            result.unverified,
                            code_points(&result.refused_text)
                        );
                    }
                }
                _ => {}
            }
        }
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    drain(session, &mut refused, &mut results);
    for (index, text) in texts.iter().enumerate() {
        let sequence = u64::try_from(index).unwrap_or(u64::MAX) + 100;
        let message = TextCommitMsg::new(text.clone(), sequence);
        message
            .validate()
            .map_err(|error| format!("--text {}: {error}", index + 1))?;
        println!(
            "input-smoke text_commit sequence={sequence} code_points=[{}]",
            code_points(text)
        );
        session
            .commands
            .send(SessionCommand::Json(
                serde_json::to_value(message).map_err(|error| error.to_string())?,
            ))
            .map_err(|_| "session closed while sending text".to_string())?;
        tokio::time::sleep(Duration::from_millis(400)).await;
        drain(session, &mut refused, &mut results);
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    drain(session, &mut refused, &mut results);
    close_smoke_session(session).await;
    println!(
        "input-smoke complete text_commits={} results={results} refused_chars={refused}",
        texts.len()
    );
    Ok(())
}

/// Ends a smoke session the way the Deck app does: a clean close the Pier
/// sees at once, instead of a vanished process whose connection it must
/// time out before the desktop can be attached again.
async fn close_smoke_session(session: &mut SessionHandle) {
    let _ = session.commands.send(SessionCommand::Close);
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(event) = session.events.recv().await {
            if matches!(event, SessionEvent::Ended(_)) {
                break;
            }
        }
    })
    .await;
}

fn server_reports_remote_input(hello: &ServerHelloMsg) -> bool {
    let typed_input = hello.input_protocol_version > 0
        && hello.input_capabilities.absolute_pointer == InputCapabilityAvailability::Available;
    let legacy_input = hello
        .device_capabilities
        .get("input")
        .and_then(|input| input.get("available"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    typed_input || legacy_input
}

/// Smoke clipboard files from a comma-separated path list: name, wire kind
/// (`.png` is an image, anything else UTF-8 text) and bytes.
fn smoke_clipboard_files(
    variable: &str,
) -> Result<
    std::collections::VecDeque<(
        String,
        arcen_protocol::messages::ClipboardContentKind,
        Vec<u8>,
    )>,
    String,
> {
    let Ok(list) = std::env::var(variable) else {
        return Ok(std::collections::VecDeque::new());
    };
    list.split(',')
        .filter(|path| !path.is_empty())
        .map(|path| {
            let bytes =
                std::fs::read(path).map_err(|error| format!("{variable}: {path}: {error}"))?;
            let kind = if path.to_ascii_lowercase().ends_with(".png") {
                arcen_protocol::messages::ClipboardContentKind::ImagePng
            } else {
                arcen_protocol::messages::ClipboardContentKind::TextUtf8
            };
            let name = std::path::Path::new(path).file_name().map_or_else(
                || path.to_owned(),
                |name| name.to_string_lossy().into_owned(),
            );
            Ok((name, kind, bytes))
        })
        .collect()
}

/// JSON-lines measurement record written by `media-smoke` when
/// `ARCEN_MEDIA_SMOKE_STATS` names a file.
struct SmokeStatsWriter {
    file: std::io::BufWriter<std::fs::File>,
}

impl SmokeStatsWriter {
    fn create(path: &str) -> Result<Self, String> {
        std::fs::File::create(path)
            .map(|file| Self {
                file: std::io::BufWriter::new(file),
            })
            .map_err(|error| format!("cannot create stats file {path}: {error}"))
    }

    fn line(&mut self, value: &serde_json::Value) {
        use std::io::Write as _;
        let _ = serde_json::to_writer(&mut self.file, value);
        let _ = self.file.write_all(b"\n");
    }
}

impl Drop for SmokeStatsWriter {
    fn drop(&mut self) {
        use std::io::Write as _;
        let _ = self.file.flush();
    }
}

fn unix_ms_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1000.0
}

async fn media_smoke(
    mut options: ConnectOptions,
    require_multi_monitor: bool,
    accept_disclaimer: bool,
    verify_audio: bool,
) -> Result<(), String> {
    let deck_build = arcen_deck::build_identity::current();
    println!(
        "deck-build product={} version={} build_id={} revision={} profile={} features={} signing={}",
        deck_build.product,
        deck_build.version,
        deck_build.build_id,
        deck_build.source_revision,
        deck_build.build_profile,
        deck_build.feature_profile,
        deck_build.signing_state.as_deref().unwrap_or("unknown")
    );
    let requested_codec = options.profile.codec.clone();
    let requested_chroma = options.profile.chroma.clone();
    let requested_displays_mode = options.displays_mode.clone();
    let requested_multi_monitor_topology = options.multi_monitor_topology.clone();
    let microphone_requested = options.microphone_enabled;
    let tablet_input_requested = options.tablet_input_enabled;
    let mut auth = Some(AuthSubmission {
        username: std::mem::take(&mut options.username),
        password: std::mem::take(&mut options.password),
    });
    // ARCEN_MEDIA_SMOKE_RESUME=1 opts into direct resume the way the Deck
    // app does (the host may then hold the desktop for a reconnect window).
    let mut session = if std::env::var("ARCEN_MEDIA_SMOKE_RESUME").is_ok_and(|value| value == "1") {
        let nonce = arcen_deck::reconnect::fresh_holder_nonce()
            .map_err(|error| format!("resume holder nonce: {error}"))?;
        arcen_deck::transport::websocket::spawn_session_opt_in(options, nonce)
    } else {
        spawn_session(options)
    };
    let result: Result<(), String> = async {
    let mut decoders = BTreeMap::<u16, NativeVideoDecoder>::new();
    let mut expected_monitor_ids = BTreeSet::<u16>::new();
    let mut expected_wire_profiles = BTreeMap::<
        u16,
        (
            arcen_deck::protocol::VideoCodec,
            arcen_deck::protocol::ChromaSubsampling,
        ),
    >::new();
    let mut validated_wire_profiles = BTreeSet::<u16>::new();
    let mut received_monitor_ids = BTreeSet::<u16>::new();
    let mut decoded_monitor_ids = BTreeSet::<u16>::new();
    let mut input_probe_points = Vec::<MultiMonitorInputProbePoint>::new();
    let mut input_probe_sent = false;
    let mut input_probe_confirmed = !require_multi_monitor;
    let mut require_last_pen_type = false;
    let mut probe_pen = true;
    let mut audio = PcmAudioPlayer::awaiting_stream_result();
    let discard_audio =
        std::env::var("ARCEN_AUDIO_SINK").is_ok_and(|value| value.eq_ignore_ascii_case("discard"));
    let mut video_packets = 0_u64;
    let mut audio_packets = 0_u64;
    let mut audio_packets_consumed = 0_u64;
    let mut audio_nonzero_samples = 0_u64;
    let mut audio_peak = 0_u16;
    let mut server_supports_audio = false;
    let mut server_supports_h265 = false;
    let mut server_supports_av1 = false;
    let mut server_supports_yuv444 = false;
    let mut server_active_codec = None;
    let mut server_active_chroma = None;
    let mut host_active_pipeline = "absent".to_string();
    let mut decoded_summary: Option<String> = None;
    let mut decoded_resolution: Option<(usize, usize)> = None;
    let mut full_frame_requests = FullFrameRequestGate::default();
    let target_video_packets = std::env::var("ARCEN_MEDIA_SMOKE_FRAMES")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1)
        .max(1);
    let target_duration = std::env::var("ARCEN_MEDIA_SMOKE_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs);
    let started = Instant::now();
    let smoke_timeout = std::env::var("ARCEN_MEDIA_SMOKE_TIMEOUT_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| target_duration.unwrap_or_default() + Duration::from_secs(60));
    let mut last_health_ping = Instant::now();
    let mut health_sequence = 0_u64;
    // Nothing but the authentication answer may be queued before the host
    // accepted the sign-in: the session reads any other command there as
    // declining it. A sign-in can take longer than one health interval
    // (PAM, a same-user liveness proof).
    let mut authentication_accepted = false;
    let mut last_health = None;
    let mut microphone_result_seen = false;
    // Set when the host answers that this session carries no audio. Without it
    // the smoke waits out the full media timeout for packets the host has
    // already said are not coming.
    let mut audio_refused = false;
    let mut microphone_active = false;
    // ARCEN_MEDIA_DUMP=<path>: skip decoding, write the raw Annex-B
    // video elementary stream to <path> so ffprobe can verify the live wire
    // codec/profile/chroma end-to-end. Static SCK desktops may stop producing
    // unchanged frames, so acceptance runs can lower the default 150-AU target.
    let dump_target_aus = std::env::var("ARCEN_MEDIA_DUMP_AUS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(150)
        .max(1);
    let mut dump_file = match std::env::var("ARCEN_MEDIA_DUMP") {
        Ok(path) => Some(
            std::fs::File::create(&path)
                .map_err(|error| format!("cannot create dump file {path}: {error}"))?,
        ),
        Err(_) => None,
    };
    let mut dumped_aus = 0_u32;
    // ARCEN_MEDIA_SMOKE_STATS=<path>: write one JSON line per received video
    // packet (wall-clock receive time, host timestamp, size), per health
    // ping/pong and per host health_stats, for offline path measurements.
    let mut smoke_stats = match std::env::var("ARCEN_MEDIA_SMOKE_STATS") {
        Ok(path) => Some(SmokeStatsWriter::create(&path)?),
        Err(_) => None,
    };
    let mut dump_complete = false;
    // ARCEN_MEDIA_SMOKE_RESIZE=WxH[,WxH...]: once video decodes, ask the host
    // for each size in turn (`display_update`), one every
    // ARCEN_MEDIA_SMOKE_RESIZE_EVERY seconds (default 4), and report each
    // result and each decoded size change. Needs `--displays-mode windowed`.
    let mut resize_plan = std::env::var("ARCEN_MEDIA_SMOKE_RESIZE")
        .ok()
        .map(|value| {
            value
                .split(',')
                .filter_map(|size| {
                    let (width, height) = size.trim().split_once('x')?;
                    Some((width.parse::<u32>().ok()?, height.parse::<u32>().ok()?))
                })
                .collect::<std::collections::VecDeque<_>>()
        })
        .unwrap_or_default();
    let resize_every = Duration::from_secs(
        std::env::var("ARCEN_MEDIA_SMOKE_RESIZE_EVERY")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(4),
    );
    let mut next_resize_at: Option<Instant> = None;
    // ARCEN_MEDIA_SMOKE_INPUT=1: send absolute pointer motion every 50 ms
    // once video decodes (input across resizes and recoveries).
    let smoke_input = std::env::var("ARCEN_MEDIA_SMOKE_INPUT").is_ok_and(|value| value == "1");
    let smoke_escape = std::env::var("ARCEN_MEDIA_SMOKE_ESCAPE").is_ok_and(|value| value == "1");
    let smoke_cursor_report =
        std::env::var("ARCEN_MEDIA_SMOKE_CURSOR").is_ok_and(|value| value == "1");
    let mut smoke_cursor = arcen_input::cursor::DeckCursorState::default();
    let mut smoke_pointer = arcen_input::host_pointer::DeckHostPointer::default();
    let mut input_sequence = 0_u64;
    let mut last_input_at = Instant::now();
    let mut resize_sequence = 0_u64;
    let mut last_decoded_size: Option<(usize, usize)> = None;
    // ARCEN_MEDIA_SMOKE_CLIPBOARD_SEND=path[,path...]: once video decodes,
    // offer each file as the Deck clipboard (`.png` as an image, anything
    // else as UTF-8 text), one every ARCEN_MEDIA_SMOKE_CLIPBOARD_EVERY
    // seconds (default 3). Each host clipboard item that arrives is reported
    // by kind, size and which ARCEN_MEDIA_SMOKE_CLIPBOARD_EXPECT file it
    // equals; never by content.
    let mut clipboard_plan = smoke_clipboard_files("ARCEN_MEDIA_SMOKE_CLIPBOARD_SEND")?;
    let clipboard_expected = smoke_clipboard_files("ARCEN_MEDIA_SMOKE_CLIPBOARD_EXPECT")?;
    let clipboard_every = Duration::from_secs(
        std::env::var("ARCEN_MEDIA_SMOKE_CLIPBOARD_EVERY")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(3),
    );
    let mut clipboard_sequence = 0_u64;
    let mut next_clipboard_at: Option<Instant> = None;

    loop {
        if full_frame_requests
            .retry_after()
            .is_some_and(|delay| delay.is_zero())
        {
            let _ = full_frame_requests.send_due(&session.commands);
        }
        let remaining = smoke_timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            let audio_note = pending_audio_note(
                audio_requirement_satisfied(
                    require_multi_monitor,
                    verify_audio,
                    server_supports_audio,
                    audio_packets_consumed,
                    audio_refused,
                ),
                video_packets,
            );
            return Err(format!(
                "media smoke timed out after {}s (video_packets={video_packets}, audio_packets={audio_packets}, received_monitors={:?}, decoded_monitors={:?}, microphone_active={microphone_active}, input_probe_confirmed={input_probe_confirmed}, decoders_wanting_keyframe={:?}){audio_note}",
                smoke_timeout.as_secs(),
                received_monitor_ids,
                decoded_monitor_ids,
                decoders
                    .iter()
                    .filter(|(_, decoder)| decoder.wants_keyframe())
                    .map(|(monitor, _)| *monitor)
                    .collect::<Vec<_>>()
            ));
        }
        let wait = full_frame_requests
            .retry_after()
            .map_or(remaining, |retry| retry.min(remaining));
        let event = match tokio::time::timeout(wait, session.events.recv()).await {
            Ok(event) => event,
            Err(_)
                if full_frame_requests
                    .retry_after()
                    .is_some_and(|delay| delay.is_zero()) =>
            {
                let _ = full_frame_requests.send_due(&session.commands);
                continue;
            }
            // The deadline passed while waiting: the check at the top of the
            // loop reports what arrived and what is still missing, rather
            // than a bare timeout that reads as "nothing arrived".
            Err(_) => continue,
        }
        .ok_or_else(|| "session ended before media arrived".to_string())?;
        if smoke_input
            && last_decoded_size.is_some()
            && last_input_at.elapsed() >= Duration::from_millis(50)
        {
            input_sequence += 1;
            // ARCEN_MEDIA_SMOKE_ESCAPE=1: press and release Escape twice
            // first (a GNOME desktop starts in the Activities overview).
            if smoke_escape && input_sequence <= 4 {
                let _ = session
                    .commands
                    .send(SessionCommand::Json(serde_json::json!({
                        "type": "key_event", "scan_code": 0x0100_0000_u32,
                        "pressed": input_sequence % 2 == 1, "modifiers": 0,
                        "sequence": input_sequence
                    })));
            } else {
                let phase = (input_sequence % 100) as f64 / 100.0;
                let _ = session
                    .commands
                    .send(SessionCommand::Json(serde_json::json!({
                        "type": "mouse_move", "x": phase, "y": 1.0 - phase,
                        "server_x": -1, "server_y": -1, "sequence": input_sequence,
                        "timestamp_ns": 0, "coalescable": true
                    })));
            }
            last_input_at = Instant::now();
        }
        if last_decoded_size.is_some() && !clipboard_plan.is_empty() {
            let due = *next_clipboard_at.get_or_insert_with(Instant::now);
            if Instant::now() >= due {
                if let Some((name, kind, bytes)) = clipboard_plan.pop_front() {
                    clipboard_sequence += 1;
                    let size = bytes.len();
                    let queued = arcen_deck::clipboard::ClipboardItem::new(
                        clipboard_sequence,
                        kind,
                        bytes,
                        false,
                    )
                    .is_some_and(|item| session.clipboard.queue_outbound(item));
                    println!(
                        "clipboard-sent sequence={clipboard_sequence} kind={kind:?} bytes={size} file={name} queued={queued} elapsed_ms={}",
                        started.elapsed().as_millis()
                    );
                }
                next_clipboard_at = Some(Instant::now() + clipboard_every);
            }
        }
        while let Some(item) = session.clipboard.take_inbound() {
            let matches = clipboard_expected
                .iter()
                .find(|(_, kind, bytes)| *kind == item.kind && *bytes == item.bytes)
                .map_or("none", |(name, _, _)| name.as_str());
            println!(
                "clipboard-received sequence={} kind={:?} bytes={} truncated={} matches={matches} elapsed_ms={}",
                item.sequence,
                item.kind,
                item.bytes.len(),
                item.truncated,
                started.elapsed().as_millis()
            );
        }
        if next_resize_at.is_some_and(|at| Instant::now() >= at) {
            if let Some((width, height)) = resize_plan.pop_front() {
                resize_sequence += 1;
                println!(
                    "resize-request sequence={resize_sequence} {width}x{height} elapsed_ms={} audio_packets={audio_packets}",
                    started.elapsed().as_millis()
                );
                let _ = session
                    .commands
                    .send(SessionCommand::Json(serde_json::json!({
                        "type": "display_update",
                        "sequence": resize_sequence,
                        "width": width,
                        "height": height,
                        "scale": 1.0,
                        "reason": "media-smoke"
                    })));
                next_resize_at = Some(Instant::now() + resize_every);
            } else {
                next_resize_at = None;
            }
        }
        if authentication_accepted && last_health_ping.elapsed() >= Duration::from_secs(5) {
            health_sequence += 1;
            let _ = session
                .commands
                .send(SessionCommand::Json(serde_json::json!({
                    "type": "health_ping",
                    "timestamp_ms": started.elapsed().as_millis() as u64,
                    "sequence": health_sequence
                })));
            last_health_ping = Instant::now();
            if let Some(stats) = smoke_stats.as_mut() {
                stats.line(&serde_json::json!({
                    "k": "ping", "seq": health_sequence, "tx": unix_ms_f64(),
                }));
            }
        }
        match event {
            SessionEvent::CertificateUntrusted(_) => {
                return Err("server certificate requires interactive trust".to_string());
            }
            SessionEvent::ServerHello(hello) => {
                println!(
                    "server={} codec={} bit_depth={} range={} matrix={} primaries={} transfer={} \
                     pixel_format={} encoder={} encoder_class={} active_pipeline={} size={}x{} supports_audio={} \
                     yuv444={} av1={}",
                    hello.server_name,
                    hello.codec,
                    hello.color_caps.active_bit_depth,
                    hello.color_caps.active_range,
                    hello.color_caps.active_matrix,
                    hello.color_caps.active_primaries,
                    hello.color_caps.active_transfer,
                    hello.color_caps.advertised_pix_fmt,
                    hello.encoder_backend,
                    hello.encoder_class,
                    hello.active_pipeline
                        .as_ref()
                        .map_or("absent", arcen_protocol::messages::ServedStreamPipeline::token),
                    hello.screen_width,
                    hello.screen_height,
                    hello.supports_audio,
                    hello.supports_yuv444,
                    hello.supports_av1
                );
                match hello.build_identity() {
                    Ok(Some(identity)) => println!(
                        "host-build product={} version={} build_id={} revision={} profile={} features={} artifact_sha256={} signing={}",
                        identity.product,
                        identity.version,
                        identity.build_id,
                        identity.source_revision,
                        identity.build_profile,
                        identity.feature_profile,
                        identity.artifact_sha256.as_deref().unwrap_or("unavailable"),
                        identity.signing_state.as_deref().unwrap_or("unknown")
                    ),
                    Ok(None) => println!("host-build unavailable"),
                    Err(error) => return Err(format!("invalid host build identity: {error}")),
                }
                server_supports_audio = hello.supports_audio;
                server_supports_h265 = hello.supports_h265;
                server_supports_av1 = hello.supports_av1;
                server_supports_yuv444 = hello.supports_yuv444;
                host_active_pipeline = hello
                    .active_pipeline
                    .as_ref()
                    .map_or(
                        "absent",
                        arcen_protocol::messages::ServedStreamPipeline::token,
                    )
                    .to_string();
                server_active_codec = match hello.codec.as_str() {
                    "h264" => Some(arcen_deck::protocol::VideoCodec::H264),
                    "h265" => Some(arcen_deck::protocol::VideoCodec::H265),
                    "av1" => Some(arcen_deck::protocol::VideoCodec::Av1),
                    _ => None,
                };
                server_active_chroma =
                    active_chroma_from_pixel_format(&hello.color_caps.advertised_pix_fmt);
                if require_multi_monitor {
                    // Tablet input off (`--tablet-mode off`, `--no-tablet-input`)
                    // probes regions with pointer buttons instead of the pen,
                    // for a host without a pen device.
                    probe_pen = tablet_input_requested;
                    if probe_pen
                        && hello.input_capabilities.pen != InputCapabilityAvailability::Available
                    {
                        return Err(
                            "host did not advertise native pen input for the multi-monitor session \
                             (run with --tablet-mode off to probe without a pen)"
                                .to_string(),
                        );
                    }
                    require_last_pen_type = probe_pen && hello.server_name.contains("Windows");
                    let capability = hello
                        .multi_monitor_v1()
                        .map_err(|error| format!("invalid multi-monitor ServerHello: {error}"))?
                        .ok_or_else(|| {
                            "host did not return a multi_monitor_v1 capability".to_string()
                        })?;
                    let applied = capability.applied_topology().ok_or_else(|| {
                        "host did not return an applied multi-monitor topology".to_string()
                    })?;
                    expected_monitor_ids = applied
                        .monitors()
                        .iter()
                        .map(|monitor| monitor.session_monitor_id)
                        .collect();
                    expected_wire_profiles = applied
                        .monitors()
                        .iter()
                        .map(|monitor| {
                            let codec = match monitor.media_plan.codec.as_str() {
                                "h264" => Ok(arcen_deck::protocol::VideoCodec::H264),
                                "h265" => Ok(arcen_deck::protocol::VideoCodec::H265),
                                "av1" => Ok(arcen_deck::protocol::VideoCodec::Av1),
                                other => Err(format!(
                                    "monitor {} advertised unsupported codec {other:?}",
                                    monitor.session_monitor_id
                                )),
                            }?;
                            let chroma = match monitor.media_plan.chroma.as_str() {
                                "yuv420" => Ok(arcen_deck::protocol::ChromaSubsampling::Yuv420),
                                "yuv444" => Ok(arcen_deck::protocol::ChromaSubsampling::Yuv444),
                                other => Err(format!(
                                    "monitor {} advertised unsupported chroma {other:?}",
                                    monitor.session_monitor_id
                                )),
                            }?;
                            Ok((monitor.session_monitor_id, (codec, chroma)))
                        })
                        .collect::<Result<BTreeMap<_, _>, String>>()?;
                    let requested = requested_multi_monitor_topology.as_ref().ok_or_else(|| {
                        "multi-monitor smoke lost its requested topology".to_string()
                    })?;
                    input_probe_points = applied
                        .monitors()
                        .iter()
                        .take(2)
                        .map(|monitor| {
                            let requested_monitor = requested
                                .topology
                                .monitors()
                                .iter()
                                .find(|candidate| {
                                    candidate.monitor.identity.id
                                        == monitor.client_display_id.as_str()
                                })
                                .ok_or_else(|| {
                                    format!(
                                        "applied monitor {} has no requested display",
                                        monitor.client_display_id.as_str()
                                    )
                                })?;
                            Ok(MultiMonitorInputProbePoint {
                                region_generation: applied.topology_generation(),
                                region_id: u32::from(monitor.session_monitor_id),
                                logical_x: i64::from(requested_monitor.logical_width)
                                    .saturating_mul(120)
                                    .saturating_sub(1)
                                    / 2,
                                logical_y: i64::from(requested_monitor.logical_height)
                                    .saturating_mul(120)
                                    .saturating_sub(1)
                                    / 2,
                            })
                        })
                        .collect::<Result<Vec<_>, String>>()?;
                    if expected_monitor_ids.len() < 2 {
                        return Err(format!(
                            "host applied only {} monitor(s)",
                            expected_monitor_ids.len()
                        ));
                    }
                    for monitor_id in &expected_monitor_ids {
                        decoders.insert(*monitor_id, NativeVideoDecoder::new());
                    }
                    println!(
                        "multi-monitor applied monitors={} desktop={}x{} carrier={}",
                        expected_monitor_ids.len(),
                        applied.desktop_width_px(),
                        applied.desktop_height_px(),
                        applied.selected_carrier()
                    );
                } else {
                    decoders.insert(0, NativeVideoDecoder::new());
                }
                full_frame_requests.request();
                let _ = full_frame_requests.send_due(&session.commands);
            }
            SessionEvent::AuthRequired(request) => {
                if request.disclaimer.is_some() && !accept_disclaimer {
                    return Err("DisclaimerRequired".to_string());
                }
                session
                    .commands
                    .send(SessionCommand::SubmitAuth(auth.take().ok_or_else(
                        || "host requested authentication twice".to_string(),
                    )?))
                    .map_err(|_| "session closed before authentication".to_string())?;
            }
            SessionEvent::Authenticated(_) => {
                session
                    .commands
                    .send(SessionCommand::AcceptAuthentication)
                    .map_err(|_| "session closed before authentication acceptance".to_string())?;
                authentication_accepted = true;
                last_health_ping = Instant::now();
            }
            SessionEvent::MediaReady => {
                let batch = session.media.take_batch();
                if require_multi_monitor && !input_probe_sent && input_probe_points.len() >= 2 {
                    send_multi_monitor_input_probe(
                        &session.commands,
                        &input_probe_points,
                        probe_pen,
                    )?;
                    input_probe_sent = true;
                }
                video_packets = batch.telemetry.video_received;
                // Audio has its own lane and queue (ADR 0014); the video
                // batch never carries it.
                let audio_items = session
                    .media
                    .audio_lane()
                    .recv_batch(Duration::ZERO)
                    .unwrap_or_default();
                audio_packets = session.media.audio_lane().counters().received;
                if let Some(error) = &batch.malformed_error {
                    eprintln!("media-smoke ignored malformed media packet: {error}");
                }
                // Socket-level diagnostic: confirm frames arrive at the client.
                // Separates "host never sends" from "client discards" in one run.
                if video_packets == 1 {
                    eprintln!(
                        "media-smoke socket: first video packet received (total_video={video_packets} total_audio={audio_packets})"
                    );
                }

                for item in audio_items {
                    let (header, payload) = match item {
                        arcen_deck::pipeline::audio_lane::AudioLaneItem::StreamResult(result) => {
                            audio.set_stream_result(&result);
                            continue;
                        }
                        arcen_deck::pipeline::audio_lane::AudioLaneItem::Frame {
                            header,
                            payload,
                            ..
                        } => (header, payload),
                    };
                    audio_packets_consumed += 1;
                    for sample in payload
                        .chunks_exact(2)
                        .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]))
                    {
                        audio_nonzero_samples += u64::from(sample != 0);
                        audio_peak = audio_peak.max(sample.unsigned_abs());
                    }
                    let status = (!discard_audio).then(|| audio.feed(header, &payload));
                    if audio_packets_consumed == 1 {
                        if let Some(status) = status {
                            println!(
                                "audio backend={} accepted={} queued={}ms note={}",
                                status.backend,
                                status.accepted_bytes,
                                status.queued_ms,
                                status.note.unwrap_or_default()
                            );
                        } else {
                            println!(
                                "audio backend=discard accepted={} queued=0ms note=same-machine feedback prevention",
                                payload.len()
                            );
                        }
                    }
                }

                if batch.video_discontinuity {
                    for monitor_id in &batch.video_discontinuity_monitor_ids {
                        if let Some(decoder) = decoders.get_mut(monitor_id) {
                            decoder.notify_discontinuity();
                        }
                    }
                }
                if batch.idr_needed {
                    full_frame_requests.request();
                    let _ = full_frame_requests.send_due(&session.commands);
                }

                if let Some(stats) = smoke_stats.as_mut() {
                    let rx = unix_ms_f64();
                    for (header, payload) in &batch.video {
                        stats.line(&serde_json::json!({
                            "k": "v", "rx": rx, "ts": header.timestamp_ms,
                            "b": payload.len(), "key": header.is_keyframe(),
                            "m": header.monitor_id,
                        }));
                    }
                }
                for (header, payload) in batch.video {
                    let monitor_id = header.monitor_id;
                    if received_monitor_ids.insert(monitor_id) {
                        eprintln!(
                            "media-smoke socket: first packet for monitor={} codec={:?} chroma={:?} keyframe={} payload={}",
                            monitor_id,
                            header.codec,
                            header.chroma,
                            header.is_keyframe(),
                            payload.len()
                        );
                    }
                    if require_multi_monitor && !expected_monitor_ids.contains(&monitor_id) {
                        return Err(format!(
                            "received video for unnegotiated monitor id {monitor_id}"
                        ));
                    }
                    if !validated_wire_profiles.contains(&monitor_id) {
                        let (expected_codec, expected_chroma) = expected_wire_profiles
                            .get(&monitor_id)
                            .copied()
                            .unwrap_or_else(|| {
                                let codec = server_active_codec.unwrap_or_else(|| {
                                    if requested_codec == "av1" && server_supports_av1 {
                                        arcen_deck::protocol::VideoCodec::Av1
                                    } else if requested_codec == "h265" && server_supports_h265 {
                                        arcen_deck::protocol::VideoCodec::H265
                                    } else {
                                        arcen_deck::protocol::VideoCodec::H264
                                    }
                                });
                                let chroma = server_active_chroma.unwrap_or_else(|| {
                                    if requested_chroma == "yuv444" && server_supports_yuv444 {
                                        arcen_deck::protocol::ChromaSubsampling::Yuv444
                                    } else {
                                        arcen_deck::protocol::ChromaSubsampling::Yuv420
                                    }
                                });
                                (codec, chroma)
                            });
                        if header.codec != expected_codec || header.chroma != expected_chroma {
                            return Err(format!(
                                "wire profile mismatch: requested={requested_codec}/{requested_chroma} expected={expected_codec:?}/{expected_chroma:?} actual={:?}/{:?}",
                                header.codec, header.chroma
                            ));
                        }
                        validated_wire_profiles.insert(monitor_id);
                    }
                    let is_keyframe = header.is_keyframe();
                    if dump_file.is_some() {
                        use std::io::Write as _;
                        dump_file
                            .as_mut()
                            .expect("dump file is present")
                            .write_all(&payload)
                            .map_err(|error| format!("dump write failed: {error}"))?;
                        if is_keyframe {
                            full_frame_requests.cancel_pending();
                        }
                        dumped_aus += 1;
                        if dumped_aus >= dump_target_aus {
                            dump_complete = true;
                            dump_file.take();
                            if !microphone_requested || microphone_active {
                                println!(
                                    "media-dump complete aus={} codec={:?} chroma={:?}",
                                    dumped_aus, header.codec, header.chroma
                                );
                                return Ok(());
                            }
                        }
                        continue;
                    }
                    if dump_complete {
                        continue;
                    }
                    let decoder = decoders
                        .entry(monitor_id)
                        .or_insert_with(NativeVideoDecoder::new);
                    let decoded = match decoder.decode(&header, &payload) {
                        Ok(decoded) => decoded,
                        Err(error) => {
                            eprintln!("media-smoke decode recovery: {error}");
                            decoder.notify_discontinuity();
                            full_frame_requests.request();
                            let _ = full_frame_requests.send_due(&session.commands);
                            continue;
                        }
                    };
                    if let Some(frame) = decoded {
                        if is_keyframe {
                            full_frame_requests.cancel_pending();
                        }
                        decoded_resolution = Some((frame.width, frame.height));
                        if last_decoded_size != Some((frame.width, frame.height)) {
                            if let Some((old_width, old_height)) = last_decoded_size {
                                println!(
                                    "decoded-size-change {old_width}x{old_height} -> {}x{} elapsed_ms={} video_packets={video_packets} audio_packets={audio_packets}",
                                    frame.width,
                                    frame.height,
                                    started.elapsed().as_millis()
                                );
                            }
                            last_decoded_size = Some((frame.width, frame.height));
                            if next_resize_at.is_none() && !resize_plan.is_empty() {
                                next_resize_at = Some(Instant::now() + resize_every);
                            }
                        }
                        let first_decoded_for_monitor = decoded_monitor_ids.insert(monitor_id);
                        decoded_summary = Some(format!(
                            "decoded frame monitor={} {}x{} backend={} pixfmt={} wire_codec={:?} wire_chroma={:?} ts={} video_packets={} audio_packets={}",
                            monitor_id,
                            frame.width,
                            frame.height,
                            frame.backend,
                            frame.pixel_format,
                            header.codec,
                            header.chroma,
                            frame.timestamp_ms,
                            video_packets,
                            audio_packets
                        ));
                        if first_decoded_for_monitor {
                            println!(
                                "media-smoke decoder: first frame monitor={} {}x{} backend={} pixfmt={}",
                                monitor_id,
                                frame.width,
                                frame.height,
                                frame.backend,
                                frame.pixel_format
                            );
                        }
                    } else if decoder.wants_keyframe() {
                        full_frame_requests.request();
                        let _ = full_frame_requests.send_due(&session.commands);
                    }
                }

                if let Some(summary) = &decoded_summary {
                    let monitors_complete = !require_multi_monitor
                        || (!expected_monitor_ids.is_empty()
                            && decoded_monitor_ids == expected_monitor_ids);
                    let duration_reached =
                        target_duration.is_none_or(|target| started.elapsed() >= target);
                    if video_packets >= target_video_packets
                        && duration_reached
                        && audio_requirement_satisfied(
                            require_multi_monitor,
                            verify_audio,
                            server_supports_audio,
                            audio_packets_consumed,
                            audio_refused,
                        )
                        && (!microphone_requested || microphone_active)
                        && monitors_complete
                        && input_probe_confirmed
                        && decoders.values().all(|decoder| !decoder.wants_keyframe())
                    {
                        println!("{summary}");
                        if let Some(health) = &last_health {
                            println!("health-stats {health}");
                        }
                        println!(
                            "media-smoke complete displays_mode={} active_pipeline={} monitors_decoded={} stream_resolution={}x{} video_packets={} audio_packets={} audio_nonzero_samples={} audio_peak={} elapsed_ms={} packet_fps={:.2} input_sent={input_sequence}",
                            requested_displays_mode,
                            host_active_pipeline,
                            decoded_monitor_ids.len().max(1),
                            decoded_resolution.map_or(0, |resolution| resolution.0),
                            decoded_resolution.map_or(0, |resolution| resolution.1),
                            video_packets,
                            audio_packets,
                            audio_nonzero_samples,
                            audio_peak,
                            started.elapsed().as_millis(),
                            video_packets as f64 / started.elapsed().as_secs_f64()
                        );
                        return Ok(());
                    }
                }
            }
            SessionEvent::Json(value) => {
                if let Some(stats) = smoke_stats.as_mut() {
                    match msg_type(&value) {
                        Some("health_stats") => stats.line(&serde_json::json!({
                            "k": "h", "rx": unix_ms_f64(), "stats": &value,
                        })),
                        Some("health_pong") => stats.line(&serde_json::json!({
                            "k": "pong", "rx": unix_ms_f64(),
                            "seq": value.get("sequence"),
                            "ping": value.get("ping_timestamp_ms"),
                            "server": value.get("server_timestamp_ms"),
                        })),
                        _ => {}
                    }
                }
                if msg_type(&value) == Some("health_stats") {
                    if require_multi_monitor {
                        let stats = serde_json::from_value::<HealthStatsMsg>(value.clone())
                            .map_err(|error| format!("invalid health_stats: {error}"))?;
                        // Latched: `input_events` counts one health window,
                        // and the next window must not un-confirm the probe.
                        let confirmed_now = stats.input_events >= 8
                            && (!require_last_pen_type
                                || matches!(
                                    stats.last_input_type.as_str(),
                                    "pen_event" | "region_pen_event"
                                ));
                        if confirmed_now && !input_probe_confirmed {
                            println!(
                                "input-probe complete events={} last_type={}",
                                stats.input_events, stats.last_input_type
                            );
                        }
                        input_probe_confirmed |= confirmed_now;
                    }
                    last_health = Some(value);
                } else if msg_type(&value) == Some(arcen_protocol::messages::SERVED_PIPELINE) {
                    // The host's authoritative served identity once its real
                    // encoder is known; it supersedes the hello's value.
                    if let Ok(served) = serde_json::from_value::<
                        arcen_protocol::messages::ServedPipelineMsg,
                    >(value.clone())
                    {
                        println!(
                            "served-pipeline-update served={} backend={} reason={} detail={:?}",
                            served.served.token(),
                            served.backend,
                            served.reason.as_deref().unwrap_or("-"),
                            served.detail.as_deref().unwrap_or("-")
                        );
                        host_active_pipeline = served.served.token().to_string();
                    }
                } else if msg_type(&value) == Some("display_update_result") {
                    println!(
                        "resize-result {} elapsed_ms={} audio_packets={audio_packets}",
                        value,
                        started.elapsed().as_millis()
                    );
                } else if msg_type(&value) == Some(AUDIO_STREAM_RESULT) {
                    let result = serde_json::from_value::<AudioStreamResultMsg>(value)
                        .map_err(|error| format!("invalid audio result: {error}"))?;
                    if !result.enabled {
                        audio_refused = true;
                        println!("audio refused by host reason={:?}", result.reason);
                    }
                } else if msg_type(&value) == Some(MICROPHONE_STREAM_RESULT) && microphone_requested
                {
                    let result = serde_json::from_value::<MicrophoneStreamResultMsg>(value)
                        .map_err(|error| format!("invalid microphone result: {error}"))?;
                    microphone_result_seen = true;
                    if !result.enabled {
                        return Err(format!(
                            "microphone negotiation was rejected: {:?}",
                            result.reason
                        ));
                    }
                }
            }
            SessionEvent::PresentationRecoveryRequested | SessionEvent::ConnectionStatusHint(_) => {
            }
            // ARCEN_MEDIA_SMOKE_CURSOR=1: report every host cursor record,
            // applied in lane order as the Deck applies them.
            SessionEvent::CursorUpdate(bytes) => {
                let inline = bytes.len() > arcen_protocol::cursor::CURSOR_UPDATE_HEADER_SIZE;
                match smoke_cursor.apply(&bytes) {
                    Ok(changed) if smoke_cursor_report => println!(
                        "cursor-update bytes={} inline_image={inline} changed={changed} cursor={} elapsed_ms={}",
                        bytes.len(),
                        describe_deck_cursor(&smoke_cursor.current()),
                        started.elapsed().as_millis()
                    ),
                    Ok(_) => {}
                    Err(error) => println!("cursor-update dropped: {error}"),
                }
            }
            SessionEvent::CursorPosition(bytes) => match smoke_pointer.apply(&bytes) {
                Ok(Some(position)) if smoke_cursor_report => println!(
                    "cursor-position seq={} monitor={} output={} x={} y={} space={}x{} on_stream={} elapsed_ms={}",
                    position.sequence,
                    position.monitor_id,
                    position.output,
                    position.x,
                    position.y,
                    position.space_width,
                    position.space_height,
                    position.on_stream,
                    started.elapsed().as_millis()
                ),
                Ok(_) => {}
                Err(error) => println!("cursor-position dropped: {error}"),
            },
            SessionEvent::CursorLaneReset => {
                smoke_cursor.reset_lane();
                smoke_pointer.reset_lane();
                if smoke_cursor_report {
                    println!("cursor-lane-reset");
                }
            }
            SessionEvent::MicrophoneActive(active) => {
                if microphone_requested {
                    if active {
                        microphone_active = true;
                        if dump_complete {
                            println!("media-dump complete aus={dumped_aus} microphone_active=true");
                            return Ok(());
                        }
                    } else if microphone_result_seen || microphone_active {
                        return Err(
                            "microphone capture stopped before smoke completion".to_string()
                        );
                    }
                }
            }
            SessionEvent::Ended(end) => return Err(end.message),
        }
    }
    }
    .await;
    close_smoke_session(&mut session).await;
    result
}

/// Whether the smoke has learned everything it needs to about audio.
///
/// `audio_refused` is the host having said, in an `audio_stream_result`, that
/// this session carries no audio. `server_hello` says what the host can do and
/// the result says what it is doing; only the second one settles this. Without
/// it the smoke spent the full media timeout rediscovering something it had
/// been told in the first second.
const fn audio_requirement_satisfied(
    require_multi_monitor: bool,
    verify_audio: bool,
    server_supports_audio: bool,
    audio_packets_consumed: u64,
    audio_refused: bool,
) -> bool {
    require_multi_monitor
        || !verify_audio
        || !server_supports_audio
        || audio_refused
        || audio_packets_consumed > 0
}

/// The timeout report's note on audio. A host that advertised audio and sent
/// video but never one audio packet usually has a silent source (a Windows
/// Pier's loopback capture delivers nothing while nothing plays), not a
/// stalled stream; say so instead of leaving the counters to be misread.
fn pending_audio_note(audio_satisfied: bool, video_packets: u64) -> &'static str {
    if audio_satisfied || video_packets == 0 {
        ""
    } else {
        "; video arrived but no audio packet did: the host's audio source is probably silent \
         (a Windows Pier sends no audio while nothing plays). Play sound on the host, or pass \
         --video-only to validate video alone"
    }
}

fn describe_deck_cursor(cursor: &arcen_input::cursor::DeckCursor) -> String {
    use arcen_input::cursor::DeckCursor;
    match cursor {
        DeckCursor::Default => "default".to_owned(),
        DeckCursor::Hidden => "hidden".to_owned(),
        DeckCursor::Named(shape) => format!("named:{shape:?}"),
        DeckCursor::Image {
            id,
            image,
            output,
            generation,
        } => format!(
            "image:{}:{}x{}@{},{}:output{output}:gen{generation}",
            id.get(),
            image.width(),
            image.height(),
            image.hotspot().0,
            image.hotspot().1
        ),
    }
}

fn rotate_secondary_for_smoke(
    topology: &arcen_media::RequestedMonitorTopology,
) -> Result<arcen_media::RequestedMonitorTopology, String> {
    let secondary_index = topology
        .monitors()
        .iter()
        .position(|monitor| !monitor.monitor().primary)
        .ok_or_else(|| "rotation smoke requires a secondary display".to_string())?;
    let monitors = topology
        .monitors()
        .iter()
        .enumerate()
        .map(|(index, requested)| {
            let mut monitor = requested.monitor().clone();
            if index == secondary_index {
                monitor.rotation = arcen_media::Rotation::Degrees180;
            }
            arcen_media::RequestedMonitor::new(
                monitor,
                requested.logical_width,
                requested.logical_height,
            )
            .map_err(|error| format!("rotation smoke topology is invalid: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    arcen_media::RequestedMonitorTopology::new(monitors)
        .map_err(|error| format!("rotation smoke topology is invalid: {error}"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MultiMonitorInputProbePoint {
    region_generation: u64,
    region_id: u32,
    logical_x: i64,
    logical_y: i64,
}

fn multi_monitor_input_probe_events(
    points: &[MultiMonitorInputProbePoint],
    pen: bool,
) -> Result<Vec<serde_json::Value>, String> {
    if points.len() < 2 {
        return Err("multi-monitor input probe requires two regions".to_string());
    }
    let first = points[0];
    let second = points[1];
    let events = vec![
        serde_json::json!({
            "type":"region_pointer_enter",
            "region_generation":first.region_generation,"region_id":first.region_id,
            "logical_x":first.logical_x,"logical_y":first.logical_y,
            "sequence":1,"timestamp_ns":0,"coalescable":true
        }),
        serde_json::json!({
            "type":"region_pointer_motion",
            "region_generation":first.region_generation,"region_id":first.region_id,
            "logical_x":first.logical_x,"logical_y":first.logical_y,
            "sequence":2,"timestamp_ns":0,"coalescable":true
        }),
        serde_json::json!({
            "type":"region_pointer_leave",
            "region_generation":first.region_generation,"region_id":first.region_id,
            "logical_x":first.logical_x,"logical_y":first.logical_y,
            "sequence":3,"timestamp_ns":0,"coalescable":false
        }),
        serde_json::json!({
            "type":"region_pointer_enter",
            "region_generation":second.region_generation,"region_id":second.region_id,
            "logical_x":second.logical_x,"logical_y":second.logical_y,
            "sequence":4,"timestamp_ns":0,"coalescable":true
        }),
        serde_json::json!({
            "type":"region_pointer_motion",
            "region_generation":second.region_generation,"region_id":second.region_id,
            "logical_x":second.logical_x,"logical_y":second.logical_y,
            "sequence":5,"timestamp_ns":0,"coalescable":true
        }),
        serde_json::json!({
            "type":"region_pointer_scroll",
            "region_generation":second.region_generation,"region_id":second.region_id,
            "logical_x":second.logical_x,"logical_y":second.logical_y,
            "delta_x":0,"delta_y":-120,
            "sequence":6,"timestamp_ns":0,"coalescable":false
        }),
    ];
    let mut events = events;
    if pen {
        events.extend([
            serde_json::json!({
                "type":"region_pen_event",
                "region_generation":second.region_generation,"region_id":second.region_id,
                "logical_x":second.logical_x,"logical_y":second.logical_y,
                "pressure":0.55,"tilt_x_degrees":8.0,"tilt_y_degrees":-5.0,
                "rotation_degrees":15.0,"tool":"tip","in_proximity":true,
                "touching":true,"buttons":0,"sequence":7,"timestamp_ns":0,
                "coalescable":true
            }),
            serde_json::json!({
                "type":"region_pen_event",
                "region_generation":second.region_generation,"region_id":second.region_id,
                "logical_x":second.logical_x,"logical_y":second.logical_y,
                "pressure":0.0,"tilt_x_degrees":0.0,"tilt_y_degrees":0.0,
                "rotation_degrees":0.0,"tool":"tip","in_proximity":false,
                "touching":false,"buttons":0,"sequence":8,"timestamp_ns":0,
                "coalescable":false
            }),
        ]);
    } else {
        // A right click: harmless on a desktop, and an edge the host must
        // land on the second region's output.
        for (sequence, pressed) in [(7, true), (8, false)] {
            events.push(serde_json::json!({
                "type":"region_pointer_button",
                "region_generation":second.region_generation,"region_id":second.region_id,
                "logical_x":second.logical_x,"logical_y":second.logical_y,
                "button":3,"pressed":pressed,"sequence":sequence,"timestamp_ns":0,
                "coalescable":false
            }));
        }
    }
    Ok(events)
}

fn send_multi_monitor_input_probe(
    commands: &SessionCommandSender,
    points: &[MultiMonitorInputProbePoint],
    pen: bool,
) -> Result<(), String> {
    for event in multi_monitor_input_probe_events(points, pen)? {
        commands
            .send(SessionCommand::Json(event))
            .map_err(|_| "session closed while sending multi-monitor input probe".to_string())?;
    }
    Ok(())
}

/// `--usb-emulate NAME`: emulate a USB device for the Hard USB bridge instead
/// of capturing one (lab builds with `usb-emulate-lab` only).
#[cfg(feature = "usb-emulate-lab")]
fn select_usb_emulation(args: &[String]) -> Result<(), String> {
    use arcen_deck::usb_emulate::{select, EmulatedDevice, Playback, StrokeScript};
    let Some(name) = flag_value(args, "--usb-emulate") else {
        return Ok(());
    };
    let device = EmulatedDevice::parse(&name)?;
    let mut playback = Playback::default();
    if let Some(strokes) = flag_value(args, "--usb-emulate-strokes") {
        playback.strokes = strokes
            .parse()
            .ok()
            .filter(|strokes| (1..=100).contains(strokes))
            .ok_or_else(|| format!("invalid --usb-emulate-strokes '{strokes}' (1-100)"))?;
    }
    if let Some(lead_in) = flag_value(args, "--usb-emulate-lead-in-ms") {
        playback.lead_in = lead_in
            .parse()
            .ok()
            .filter(|milliseconds| *milliseconds <= 600_000)
            .map(Duration::from_millis)
            .ok_or_else(|| format!("invalid --usb-emulate-lead-in-ms '{lead_in}'"))?;
    }
    if let Some(stroke) = flag_value(args, "--usb-emulate-stroke") {
        playback.stroke = StrokeScript::parse(&stroke)?;
    }
    select(device, playback)
}

#[cfg(not(feature = "usb-emulate-lab"))]
fn select_usb_emulation(args: &[String]) -> Result<(), String> {
    if flag_value(args, "--usb-emulate").is_some() {
        return Err(
            "--usb-emulate is a lab tool; this Deck was built without the usb-emulate-lab feature"
                .to_owned(),
        );
    }
    Ok(())
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find_map(|pair| (pair[0] == flag).then(|| pair[1].clone()))
}

/// Every value of a repeatable flag, in order.
fn flag_values(args: &[String], flag: &str) -> Vec<String> {
    args.windows(2)
        .filter(|pair| pair[0] == flag)
        .map(|pair| pair[1].clone())
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[cfg(feature = "usb-emulate-lab")]
    #[test]
    fn emulated_pen_smoke_prints_and_counts_every_host_pointer_position() {
        use arcen_protocol::cursor_position::{encode_cursor_position, CursorPosition};
        let at = |sequence, x, y| {
            encode_cursor_position(&CursorPosition {
                sequence,
                on_stream: true,
                output: 0,
                generation: 1,
                monitor_id: 0,
                x,
                y,
                space_width: 1800,
                space_height: 1130,
            })
            .expect("valid record")
        };
        let mut pointer = SmokeHostPointer::default();
        assert_eq!(
            pointer.record(&at(1, 450, 565)),
            "cursor-position seq=1 x=450 y=565 space=1800x1130 on_stream=true"
        );
        assert!(pointer
            .record(&at(2, 900, 113))
            .starts_with("cursor-position seq=2 "));
        assert_eq!(
            pointer.record(&at(2, 900, 113)),
            "cursor-position dropped: stale sequence"
        );
        assert!(pointer
            .record(&[0_u8; 3])
            .starts_with("cursor-position dropped: "));
        assert_eq!(
            pointer.summary(),
            "host_pointer_positions=2 first=0.250,0.500 last=0.500,0.100 dropped=2"
        );
    }

    fn base_smoke_args() -> Vec<String> {
        vec![
            "arcen-client".to_string(),
            "media-smoke".to_string(),
            "host.example".to_string(),
            "18444".to_string(),
            "--tls".to_string(),
        ]
    }

    #[test]
    fn pen_file_lines_are_all_checked_before_any_is_sent() {
        let samples = parse_pen_file(
            "# comment\n\n0.5 0.25 0.7 30 -20 90 tip 1 1 1 8\n1 0 0 0 0 0 eraser 0 0 0 300\n",
        )
        .expect("valid file");
        assert_eq!(samples.len(), 2);
        let (first, pause) = &samples[0];
        assert!(first.touching && first.in_proximity && !first.eraser);
        assert_eq!((first.buttons, *pause), (1, 8));
        assert!(samples[1].0.eraser && !samples[1].0.in_proximity);

        for bad in [
            "",
            "0.5 0.5 0 0 0 0 tip 1 1 0",
            "1.5 0.5 0 0 0 0 tip 1 1 0 8",
            "0.5 0.5 0 0 0 0 pencil 1 1 0 8",
            "0.5 0.5 0 0 0 0 tip 2 1 0 8",
            "0.5 0.5 0 0 0 0 tip 1 1 0 8\n0.5 0.5 0 0 0 0 tip 1 1 x 8",
        ] {
            assert!(parse_pen_file(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn pen_full_script_is_valid_and_covers_every_pen_state() {
        use arcen_deck::protocol::messages::{PenEventMsg, PenToolMsg};
        let script = pen_full_script(false);
        assert!(pen_full_script(true).last().unwrap().touching);
        let messages: Vec<PenEventMsg> = script
            .iter()
            .enumerate()
            .map(|(index, sample)| {
                let message: PenEventMsg =
                    serde_json::from_value(sample.json(index as u64 + 1)).unwrap();
                message.validate().unwrap();
                message
            })
            .collect();
        let hover_only = |m: &PenEventMsg| m.in_proximity && !m.touching;
        assert!(messages
            .iter()
            .any(|m| hover_only(m) && m.tool == PenToolMsg::Tip && m.tilt_x_degrees > 0.0));
        assert!(messages
            .iter()
            .any(|m| hover_only(m) && m.tool == PenToolMsg::Eraser));
        assert!(messages
            .iter()
            .any(|m| m.touching && m.tool == PenToolMsg::Eraser));
        assert!(messages.iter().any(|m| m.touching && m.buttons == 0b01));
        assert!(messages.iter().any(|m| m.touching && m.buttons == 0b10));
        assert!(messages
            .iter()
            .any(|m| m.touching && m.tilt_x_degrees < 0.0 && m.tilt_y_degrees < 0.0));
        assert!(messages
            .iter()
            .any(|m| m.touching && (m.pressure - 1.0).abs() < 1e-6));
        assert!(messages
            .iter()
            .any(|m| m.touching && m.x == 0.0 && m.y == 0.0));
        assert!(messages
            .iter()
            .any(|m| m.touching && m.x == 1.0 && m.y == 1.0));
        assert!(!messages.last().unwrap().in_proximity);
        assert!(messages
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence));
    }

    #[test]
    fn multi_monitor_smoke_uses_region_authoritative_input_messages() {
        let points = [
            MultiMonitorInputProbePoint {
                region_generation: 9,
                region_id: 1,
                logical_x: 90_000,
                logical_y: 54_000,
            },
            MultiMonitorInputProbePoint {
                region_generation: 9,
                region_id: 2,
                logical_x: 120_000,
                logical_y: 72_000,
            },
        ];
        let without_pen = multi_monitor_input_probe_events(&points, false).expect("probe");
        assert_eq!(without_pen.len(), 8);
        assert_eq!(without_pen[6]["type"], "region_pointer_button");
        assert_eq!(without_pen[6]["pressed"], true);
        assert_eq!(without_pen[7]["pressed"], false);
        assert_eq!(without_pen[7]["sequence"], 8);
        let events = multi_monitor_input_probe_events(&points, true).expect("two-region probe");

        let types = events
            .iter()
            .map(|event| event["type"].as_str().expect("message type"))
            .collect::<Vec<_>>();
        assert_eq!(
            types,
            [
                "region_pointer_enter",
                "region_pointer_motion",
                "region_pointer_leave",
                "region_pointer_enter",
                "region_pointer_motion",
                "region_pointer_scroll",
                "region_pen_event",
                "region_pen_event",
            ]
        );
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event["region_generation"], 9);
            assert_eq!(event["sequence"], u64::try_from(index + 1).unwrap());
            assert!(event.get("server_x").is_none());
            assert!(event.get("server_y").is_none());
        }

        assert_eq!(events[0]["region_id"], 1);
        assert_eq!(events[3]["region_id"], 2);
    }

    #[test]
    fn video_only_smoke_does_not_wait_for_an_idle_audio_source() {
        assert!(!audio_requirement_satisfied(false, true, true, 0, false));
        assert!(audio_requirement_satisfied(false, false, true, 0, false));
        assert!(audio_requirement_satisfied(false, true, false, 0, false));
        assert!(audio_requirement_satisfied(false, true, true, 1, false));
        assert!(audio_requirement_satisfied(true, true, true, 0, false));
        // A host that advertised audio and then said it is sending none has
        // answered the question; waiting the timeout out only delays a result
        // that is already known.
        assert!(audio_requirement_satisfied(false, true, true, 0, true));
    }

    #[test]
    fn a_timeout_waiting_only_for_audio_names_a_silent_source() {
        let silent = pending_audio_note(false, 13);
        assert!(silent.contains("no audio packet"));
        assert!(silent.contains("--video-only"));
        assert_eq!(pending_audio_note(true, 13), "");
        // Without video the stream itself is the problem, not the audio source.
        assert_eq!(pending_audio_note(false, 0), "");
    }

    fn test_monitor(id: u32, width_px: u32, height_px: u32, is_primary: bool) -> ClientMonitor {
        ClientMonitor {
            id,
            x: if is_primary { 0 } else { width_px as i32 },
            y: 0,
            width_px,
            height_px,
            scale: 2.0,
            refresh_hz: 60,
            is_primary,
            name: format!("Display {id}"),
            width_mm: 600.0,
            height_mm: 340.0,
            vendor: id,
            model: id,
            serial: id,
            edid: String::new(),
            color: None,
        }
    }

    fn parse_with_monitors(
        args: &[String],
        monitors: Vec<ClientMonitor>,
    ) -> Result<ConnectOptions, String> {
        connect_options_from_parts_with_monitors(args, "host.example".to_string(), 18_444, monitors)
    }

    #[test]
    fn gui_quick_connect_loads_password_file() {
        let directory = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/credential-test-fixtures");
        std::fs::create_dir_all(&directory).expect("create password fixture directory");
        let path = directory.join(format!("arcen-gui-password-file-{}", std::process::id()));
        std::fs::write(&path, b"dummy-password\n").expect("write password test file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("secure password test file");
        let args = vec![
            "arcen-client".to_string(),
            "--connect".to_string(),
            "host.example".to_string(),
            "18444".to_string(),
            "--username".to_string(),
            "automation".to_string(),
            "--tls".to_string(),
            "--password-file".to_string(),
            path.display().to_string(),
        ];

        let options = quick_connect_options_from_cli_args(&args)
            .expect("parse GUI quick-connect")
            .expect("create initial GUI session");
        assert_eq!(options.host, "host.example");
        assert_eq!(options.port, 18444);
        assert_eq!(options.password, "dummy-password");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn cli_connections_apply_the_automatic_point_stream_scale_contract() {
        let args = base_smoke_args();
        let options = parse_with_monitors(&args, vec![test_monitor(1, 3008, 1692, true)])
            .expect("parse monitor");
        let monitor = &options.monitors[0];
        let dpi_x = monitor.width_px as f32 * 25.4 / monitor.width_mm;
        let dpi_y = monitor.height_px as f32 * 25.4 / monitor.height_mm;

        assert_eq!(monitor.scale, 1.0);
        assert!((dpi_x - 96.0).abs() < 0.01);
        assert!((dpi_y - 96.0).abs() < 0.01);
    }

    #[test]
    fn every_smoke_command_loads_password_file() {
        let directory = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/credential-test-fixtures");
        std::fs::create_dir_all(&directory).expect("create password fixture directory");
        let path = directory.join(format!("arcen-smoke-password-file-{}", std::process::id()));
        std::fs::write(&path, b"dummy-smoke-password\n").expect("write password test file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("secure password test file");

        for subcommand in ["connect-smoke", "media-smoke", "input-smoke"] {
            let args = vec![
                "arcen-client".to_string(),
                subcommand.to_string(),
                "host.example".to_string(),
                "18444".to_string(),
                "--tls".to_string(),
                "--password-file".to_string(),
                path.display().to_string(),
            ];
            let options = connect_options_from_cli_args(&args).expect("parse smoke options");
            assert_eq!(options.password, "dummy-smoke-password");
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn input_smoke_accepts_typed_absolute_pointer_capability() {
        let hello: ServerHelloMsg = serde_json::from_value(serde_json::json!({
            "type": "server_hello",
            "input_protocol_version": 3,
            "input_capabilities": {
                "absolute_pointer": "available",
                "relative_pointer": "available"
            }
        }))
        .unwrap();

        assert!(server_reports_remote_input(&hello));
    }

    #[test]
    fn input_smoke_preserves_legacy_capability_fallback() {
        let hello: ServerHelloMsg = serde_json::from_value(serde_json::json!({
            "type": "server_hello",
            "device_capabilities": {
                "input": {
                    "available": true
                }
            }
        }))
        .unwrap();

        assert!(server_reports_remote_input(&hello));
    }

    #[test]
    fn input_smoke_rejects_unproven_input_capability() {
        let hello: ServerHelloMsg =
            serde_json::from_value(serde_json::json!({"type": "server_hello"})).unwrap();

        assert!(!server_reports_remote_input(&hello));
    }

    #[test]
    fn microphone_cli_consent_is_explicit_and_launch_scoped() {
        let mut args = vec![
            "arcen-client".to_string(),
            "media-smoke".to_string(),
            "host.example".to_string(),
            "18444".to_string(),
            "--tls".to_string(),
        ];
        assert!(
            !connect_options_from_cli_args(&args)
                .unwrap()
                .microphone_enabled
        );
        args.push("--microphone".to_string());
        assert!(
            connect_options_from_cli_args(&args)
                .unwrap()
                .microphone_enabled
        );

        args[1] = "connect-smoke".to_string();
        assert_eq!(
            connect_options_from_cli_args(&args).unwrap_err(),
            "--microphone requires media-smoke for verified capture"
        );
    }

    #[test]
    fn video_selection_cli_is_explicit_and_strict() {
        let mut args = base_smoke_args();
        args.extend([
            "--video-selection".to_string(),
            "adaptive-performance".to_string(),
        ]);
        assert_eq!(
            connect_options_from_cli_args(&args)
                .unwrap()
                .profile
                .video_selection,
            arcen_protocol::messages::VideoSelectionIntent::AdaptivePerformance
        );
        *args.last_mut().unwrap() = "fastest".to_string();
        assert!(connect_options_from_cli_args(&args).is_err());
    }

    #[test]
    fn pipeline_cli_uses_the_shared_contract() {
        for (token, expected_pipeline) in [
            ("auto", arcen_protocol::messages::StreamPipeline::Auto),
            ("speed", arcen_protocol::messages::StreamPipeline::Speed),
            ("grading", arcen_protocol::messages::StreamPipeline::Grading),
            ("hdr", arcen_protocol::messages::StreamPipeline::Hdr),
        ] {
            let mut args = base_smoke_args();
            args.extend(["--pipeline".to_string(), token.to_string()]);
            let options = connect_options_from_cli_args(&args).expect("pipeline parses");
            let id = parse_pipeline(token).expect("known pipeline");
            let contract = arcen_media::video::pipeline_contract(id);
            assert_eq!(options.profile.pipeline, Some(expected_pipeline));
            assert_eq!(options.profile.codec, contract.colour.codec.token());
            assert_eq!(options.profile.chroma, contract.colour.chroma.token());
            assert_eq!(options.profile.video_selection, contract.selection);
            assert_eq!(options.profile.max_fps, contract.max_fps);
            assert_eq!(options.profile.bit_depth, contract.colour.bit_depth.token());
            assert_eq!(options.profile.color_range, contract.colour.range.token());
            assert_eq!(options.profile.color_matrix, contract.colour.matrix.token());
            assert_eq!(options.profile.transfer, contract.colour.transfer.token());
            assert_eq!(
                options.profile.color_primaries,
                contract.colour.primaries.token()
            );
            assert_eq!(options.profile.encode_intent, contract.intent.token());
            assert_eq!(options.profile.motion_priority, contract.priority.token());
        }
    }

    #[test]
    fn explicit_video_axis_overrides_pipeline_and_makes_custom_exact() {
        let mut args = base_smoke_args();
        args.extend([
            "--pipeline".to_string(),
            "speed".to_string(),
            "--codec".to_string(),
            "h265".to_string(),
        ]);
        let options = connect_options_from_cli_args(&args).expect("custom pipeline override");
        assert_eq!(options.profile.codec, "h265");
        assert_eq!(
            options.profile.max_fps, 60,
            "other Speed fields remain the base"
        );
        assert_eq!(
            options.profile.video_selection,
            arcen_protocol::messages::VideoSelectionIntent::Exact
        );
        assert_eq!(options.profile.pipeline, None);

        *args.last_mut().unwrap() = "av1".to_string();
        args.extend([
            "--video-selection".to_string(),
            "adaptive-performance".to_string(),
        ]);
        let options = connect_options_from_cli_args(&args).expect("explicit selection");
        assert_eq!(
            options.profile.video_selection,
            arcen_protocol::messages::VideoSelectionIntent::AdaptivePerformance
        );
        assert_eq!(options.profile.pipeline, None);
    }

    #[test]
    fn pipeline_cli_rejects_unknown_tokens() {
        let mut args = base_smoke_args();
        args.extend(["--pipeline".to_string(), "cinema".to_string()]);
        assert_eq!(
            connect_options_from_cli_args(&args).unwrap_err(),
            "--pipeline must be auto, speed, grading, or hdr"
        );
    }

    #[test]
    fn password_authentication_always_uses_tls() {
        let args = vec![
            "arcen-client".to_string(),
            "connect-smoke".to_string(),
            "host.example".to_string(),
            "18444".to_string(),
            "--password".to_string(),
            "dummy-password".to_string(),
        ];
        let options = connect_options_from_cli_args(&args).expect("secure QUIC options");
        assert!(options.use_tls);
        assert!(options.quic_enabled);
    }

    #[test]
    fn accepts_custom_direct_udp_port() {
        let args = vec![
            "arcen-client".to_string(),
            "connect-smoke".to_string(),
            "host.example".to_string(),
            "8443".to_string(),
            "--tls".to_string(),
        ];
        let options = connect_options_from_cli_args(&args).expect("custom QUIC port");
        assert_eq!(options.port, 8443);
        assert!(options.use_tls);
        assert!(options.quic_enabled);
    }

    #[test]
    fn accepts_quic_on_the_product_udp_port() {
        let mut args = base_smoke_args();
        args[3] = "18444".to_string();
        args.push("--quic".to_string());
        let options = connect_options_from_cli_args(&args).unwrap();
        assert!(options.quic_enabled);
        assert!(options.use_tls);
        assert_eq!(options.port, 18_444);
    }

    #[test]
    fn quic_defaults_to_the_product_udp_port() {
        let args = vec![
            "arcen-client".to_string(),
            "connect-smoke".to_string(),
            "host.example".to_string(),
            "--quic".to_string(),
        ];
        let options = connect_options_from_cli_args(&args).unwrap();
        assert_eq!(options.port, 18_444);
        assert!(options.use_tls);
        assert!(options.quic_enabled);
    }

    #[test]
    fn quick_connect_quic_defaults_to_the_product_udp_port() {
        let args = vec![
            "arcen-client".to_string(),
            "--connect".to_string(),
            "host.example".to_string(),
            "--quic".to_string(),
        ];
        let options = quick_connect_options_from_cli_args(&args).unwrap().unwrap();
        assert_eq!(options.port, 18_444);
        assert!(options.use_tls);
        assert!(options.quic_enabled);
    }

    #[test]
    fn accepts_quic_on_a_custom_udp_port() {
        let mut args = base_smoke_args();
        args[3] = "19444".to_string();
        args.push("--quic".to_string());
        let options = connect_options_from_cli_args(&args).unwrap();
        assert_eq!(options.port, 19_444);
        assert!(options.quic_enabled);
        assert!(options.use_tls);
    }

    #[test]
    fn rejects_quic_on_udp_port_zero() {
        let mut args = base_smoke_args();
        args[3] = "0".to_string();
        args.push("--quic".to_string());
        assert_eq!(
            connect_options_from_cli_args(&args).unwrap_err(),
            "direct QUIC connections require a nonzero UDP port"
        );
    }

    #[test]
    fn smoke_cursor_mode_parses_host_and_defaults_local() {
        let options =
            parse_with_monitors(&base_smoke_args(), vec![test_monitor(1, 1920, 1080, true)])
                .expect("parse default cursor mode");
        assert_eq!(options.cursor_preference, CursorMode::Local);

        let mut host = base_smoke_args();
        host.extend(["--cursor-mode".to_string(), "host".to_string()]);
        let options = parse_with_monitors(&host, vec![test_monitor(1, 1920, 1080, true)])
            .expect("parse host cursor mode");
        assert_eq!(options.cursor_preference, CursorMode::Host);
    }

    #[test]
    fn smoke_cursor_mode_rejects_unknown_values() {
        let mut args = base_smoke_args();
        args.extend(["--cursor-mode".to_string(), "dynamic".to_string()]);
        assert_eq!(
            parse_with_monitors(&args, vec![test_monitor(1, 1920, 1080, true)]).unwrap_err(),
            "invalid --cursor-mode 'dynamic'; accepted values: local, host"
        );
    }

    #[test]
    fn smoke_displays_mode_parses_each_valid_mode() {
        for mode in ["match_layout", "single_primary", "windowed"] {
            let mut args = base_smoke_args();
            args.extend(["--displays-mode".to_string(), mode.to_string()]);

            let options = parse_with_monitors(&args, vec![test_monitor(1, 1920, 1080, true)])
                .expect("parse valid displays mode");

            assert_eq!(options.displays_mode, mode);
        }
    }

    #[test]
    fn smoke_displays_mode_rejects_invalid_mode_with_values() {
        let mut args = base_smoke_args();
        args.extend(["--displays-mode".to_string(), "mirror".to_string()]);

        let error = parse_with_monitors(&args, vec![test_monitor(1, 1920, 1080, true)])
            .expect_err("invalid mode must be rejected");

        assert_eq!(
            error,
            "invalid --displays-mode 'mirror'; accepted values: match_layout, single_primary, windowed"
        );
    }

    #[test]
    fn smoke_displays_mode_defaults_to_match_layout() {
        let options =
            parse_with_monitors(&base_smoke_args(), vec![test_monitor(1, 1920, 1080, true)])
                .expect("parse default displays mode");

        assert_eq!(options.displays_mode, "match_layout");
    }

    #[test]
    fn smoke_displays_mode_trims_pinned_modes_but_not_windowed() {
        let monitors = vec![
            test_monitor(10_001, 3024, 1890, true),
            test_monitor(10_002, 2560, 1440, false),
        ];

        for mode in ["match_layout", "single_primary"] {
            let mut args = base_smoke_args();
            args.extend(["--displays-mode".to_string(), mode.to_string()]);

            let options =
                parse_with_monitors(&args, monitors.clone()).expect("parse pinned displays mode");
            let mut expected = vec![monitors[0].clone()];
            apply_automatic_remote_ui_scale_to_monitors(&mut expected, 0);

            assert_eq!(options.displays_mode, mode);
            assert_eq!(options.monitors, expected);
        }

        let mut args = base_smoke_args();
        args.extend(["--displays-mode".to_string(), "windowed".to_string()]);

        let options = parse_with_monitors(&args, monitors.clone()).expect("parse windowed mode");
        let mut expected = monitors;
        apply_automatic_remote_ui_scale_to_monitors(&mut expected, 0);

        assert_eq!(options.displays_mode, "windowed");
        assert_eq!(options.monitors, expected);
    }
}
