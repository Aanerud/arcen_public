use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use arcen_deck::credentials::credentials_from_args;
use arcen_deck::display::metrics::{DisplayMetrics, LogicalRect, SafeAreaInsets};
use arcen_deck::pipeline::audio::PcmAudioPlayer;
use arcen_deck::pipeline::video_decoder::NativeVideoDecoder;
use arcen_deck::protocol::messages::{
    msg_type, ClientMonitor, CursorMode, HealthStatsMsg, InputCapabilityAvailability,
    MicrophoneStreamResultMsg, SafeAreaPolicyMsg, ServerHelloMsg, TabletModeMsg,
    TabletModeResultMsg, MICROPHONE_STREAM_RESULT, TABLET_MODE_RESULT,
};
use arcen_deck::transport::tls::{parse_fingerprint, TlsTrustConfig};
use arcen_deck::transport::websocket::{
    connect_smoke, spawn_session, AuthSubmission, ConnectOptions, FullFrameRequestGate,
    SessionCommand, SessionCommandSender, SessionEvent, StreamProfile,
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
#[cfg(feature = "dev-tools")]
const DEV_TOOLS_USAGE: &str = "arcen-client virtual-monitor-lab [2|4] [--timeout-secs N]   (developer-only; requires ARCEN_ENABLE_VIRTUAL_MONITOR_LAB=1)\narcen-client probe-matrix [--parameter-sets <dir>] [--output <path>]   (developer-only; see docs/testing/color-matrix-results.json)\n";

#[tokio::main]
async fn main() {
    // Logging is infrastructure: bring it up before anything else so every
    // subsystem has a sink from the first line. The GUI later re-points the
    // level at the user's saved "Log level" preference; CLI paths keep Info.
    arcen_deck::logging::init(arcen_deck::logging::startup_profile());
    arcen_deck::logging::diagnostics::log_startup_banner();

    let args: Vec<String> = std::env::args().collect();
    let subcommand = args.get(1).map(String::as_str);
    if subcommand == Some("multi-monitor-harness") {
        run_multi_monitor_harness_subcommand(&args);
        arcen_deck::logging::shutdown();
        return;
    }
    #[cfg(feature = "usb-hard-lab")]
    if subcommand == Some("usb-helper-status") {
        use arcen_deck::usb_helper_install::{install_state, HelperInstallState};
        let state = install_state();
        let label = match state {
            HelperInstallState::Enabled => "enabled",
            HelperInstallState::RequiresApproval => "requires_approval",
            HelperInstallState::NotRegistered => "not_registered",
            HelperInstallState::NotFound => "not_found",
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
    #[cfg(feature = "usb-hard-lab")]
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
    #[cfg(feature = "usb-hard-lab")]
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
    #[cfg(feature = "usb-hard-lab")]
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
    #[cfg(feature = "usb-hard-lab")]
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

    #[cfg(feature = "usb-hard-lab")]
    const WACOM_VENDOR_ID: u16 = 0x056a;

    #[cfg(feature = "usb-hard-lab")]
    fn usb_claim_probe() -> Result<(), String> {
        let device = nusb::list_devices()
            .map_err(|error| error.to_string())?
            .find(|device| device.vendor_id() == WACOM_VENDOR_ID)
            .ok_or_else(|| "no Wacom tablet is attached".to_owned())?;
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
            return Err("no Wacom USB interface could be claimed".to_owned());
        }
        println!("usb-claim-probe claimed_interfaces={claimed}");
        Ok(())
    }

    #[cfg(feature = "usb-hard-lab")]
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
                device
                    .device_descriptor()
                    .is_ok_and(|descriptor| descriptor.vendor_id() == WACOM_VENDOR_ID)
            })
            .ok_or_else(|| "no Wacom tablet is attached".to_owned())?;
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
                  arcen-client --connect <host> [port] [--ca-bundle PATH] [--pin-sha256 FP] [--insecure-skip-verify] [--username USER] [--password PASS | --password-file PATH] [--credentials-stdin] [--codec h264|h265|av1] [--chroma yuv420|yuv444] [--video-selection exact|adaptive-performance|color-fidelity] [--bit-depth 8|10|12] [--color-range limited|full] [--color-matrix bt709|identity|bt601|bt2020ncl] [--transfer bt709|srgb|pq|hlg] [--color-primaries bt709|bt2020|display_p3] [--encode-intent interactive|quality] [--max-fps N] [--microphone]\n\
                  arcen-client connect-smoke <host> [port] [--ca-bundle PATH] [--pin-sha256 FP] [--insecure-skip-verify] [--username USER] [--password PASS | --password-file PATH] [--credentials-stdin] [--video-selection exact|adaptive-performance|color-fidelity] [--cursor-mode local|host] [--displays-mode match_layout|single_primary|windowed]\n\
                  arcen-client media-smoke <host> [port] [--ca-bundle PATH] [--pin-sha256 FP] [--insecure-skip-verify] [--username USER] [--password PASS | --password-file PATH] [--credentials-stdin] [--video-selection exact|adaptive-performance|color-fidelity] [--video-only] [--microphone] [--cursor-mode local|host] [--displays-mode match_layout|single_primary|windowed]\n\
                  arcen-client multi-monitor-smoke <host> [port] [--ca-bundle PATH] [--pin-sha256 FP] [--insecure-skip-verify] [--username USER] [--password PASS | --password-file PATH] [--credentials-stdin] [--accept-disclaimer] [--monitor-fixture PATH] [--full-color-display ID]\n\
                  arcen-client input-smoke <host> [port] [--ca-bundle PATH] [--pin-sha256 FP] [--insecure-skip-verify] [--username USER] [--password PASS | --password-file PATH] [--credentials-stdin] [--cursor-mode local|host] [--displays-mode match_layout|single_primary|windowed] [--tablet-mode light|hard|off]\n\
                  arcen-client multi-monitor-harness [1|2|4] [--frames N]\n\
                  arcen-client multi-monitor-window-diagnostic [1|2|4] [--timeout-secs N]\n\
                {}transport: QUIC/TLS 1.3 over UDP; default port 18444\n\
                \n\
                {}\n\
                Source: {}",
                arcen_deck::protocol::PROTOCOL_VERSION,
                DEV_TOOLS_USAGE,
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
        if let Err(error) = input_smoke(options).await {
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
                     matrix={} pixel_format={} encoder={} encoder_class={} state={}",
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
                    result.fsm_state
                );
            } else if let Some(hello) = result.broker_hello {
                println!(
                    "connected uri={} broker_hello machines={} state={}",
                    result.uri,
                    hello
                        .get("machines")
                        .and_then(|machines| machines.as_array())
                        .map_or(0, Vec::len),
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
    let profile = StreamProfile {
        codec: flag_value(args, "--codec").unwrap_or(default_profile.codec),
        chroma: flag_value(args, "--chroma").unwrap_or(default_profile.chroma),
        video_selection: flag_value(args, "--video-selection")
            .map(|value| parse_video_selection(&value))
            .transpose()?
            .unwrap_or(arcen_protocol::messages::VideoSelectionIntent::Exact),
        max_fps: flag_value(args, "--max-fps")
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(default_profile.max_fps),
        bit_depth: flag_value(args, "--bit-depth").unwrap_or(default_profile.bit_depth),
        color_range: flag_value(args, "--color-range").unwrap_or(default_profile.color_range),
        color_matrix: flag_value(args, "--color-matrix").unwrap_or(default_profile.color_matrix),
        transfer: flag_value(args, "--transfer").unwrap_or(default_profile.transfer),
        color_primaries: flag_value(args, "--color-primaries")
            .unwrap_or(default_profile.color_primaries),
        encode_intent: flag_value(args, "--encode-intent").unwrap_or(default_profile.encode_intent),
    };
    let fixture_displays = smoke_monitor_fixture(args)?;
    let monitors = fixture_displays.as_ref().map_or(live_monitors, |displays| {
        displays
            .iter()
            .map(|(monitor, _)| monitor.clone())
            .collect()
    });
    let displays_mode = displays_mode_from_args(args)?;
    // Smoke/quick-connect reproduces the shipped defaults: the notch-area
    // opt-in and HiDPI streaming are GUI settings and are not exposed as flags,
    // while Automatic UI scale normalizes a point-sized stream to 1x/96 DPI.
    let presentation = primary_presentation_size(&monitors, false, false);
    let mut selected_monitors = monitors_for_displays_mode(displays_mode, &monitors, presentation);
    apply_automatic_remote_ui_scale_to_monitors(&mut selected_monitors, 0);
    let multi_monitor_topology = (args.get(1).map(String::as_str) == Some("multi-monitor-smoke"))
        .then(|| {
            let mut topology = match fixture_displays.as_ref() {
                Some(displays) => {
                    arcen_deck::display::topology::build_requested_topology_from(displays, false, 0)
                }
                None => arcen_deck::display::topology::build_requested_topology(false, 0),
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
            topology = apply_automatic_remote_ui_scale_to_topology(&topology, 0);
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

    let tablet_mode_requested = match flag_value(args, "--tablet-mode").as_deref() {
        Some("light") | None if !args.iter().any(|arg| arg == "--no-tablet-input") => {
            TabletModeMsg::LocalTermination
        }
        Some("hard") => TabletModeMsg::WacomUsbBridge,
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

async fn input_smoke(mut options: ConnectOptions) -> Result<(), String> {
    let hard_mode = options.tablet_mode_requested == TabletModeMsg::WacomUsbBridge;
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
                if hard_mode && msg_type(&value) == Some(TABLET_MODE_RESULT) {
                    let result = serde_json::from_value::<TabletModeResultMsg>(value)
                        .map_err(|error| error.to_string())?;
                    if !result.accepted || result.active != TabletModeMsg::WacomUsbBridge {
                        return Err(format!(
                            "Hard USB mode rejected: {}",
                            result.reason.as_str()
                        ));
                    }
                    hard_mode_ready = true;
                }
            }
            SessionEvent::BrokerHello(_) | SessionEvent::MicrophoneActive(_) => {}
        }
    }

    tokio::time::sleep(Duration::from_millis(500)).await;
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
        session
            .commands
            .send(SessionCommand::Close)
            .map_err(|_| "session closed before Hard USB teardown".to_string())?;
        println!("input-smoke complete usb_hard=true pen_states=750");
        return Ok(());
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
    session
        .commands
        .send(SessionCommand::Close)
        .map_err(|_| "session closed before input teardown".to_string())?;
    println!("input-smoke complete input_events=8 reset_events=1 keyboard=3 mouse=3 scroll=2");
    Ok(())
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
    let mut auth = Some(AuthSubmission {
        username: std::mem::take(&mut options.username),
        password: std::mem::take(&mut options.password),
    });
    let mut session = spawn_session(options);
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
    let mut audio = PcmAudioPlayer::new();
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
    let mut last_health = None;
    let mut microphone_result_seen = false;
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
    let mut dump_complete = false;

    loop {
        if full_frame_requests
            .retry_after()
            .is_some_and(|delay| delay.is_zero())
        {
            let _ = full_frame_requests.send_due(&session.commands);
        }
        let remaining = smoke_timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(format!(
                "media smoke timed out after {}s (video_packets={video_packets}, audio_packets={audio_packets}, received_monitors={:?}, decoded_monitors={:?}, microphone_active={microphone_active})",
                smoke_timeout.as_secs(),
                received_monitor_ids,
                decoded_monitor_ids
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
            Err(_) => return Err("timed out waiting for media".to_string()),
        }
        .ok_or_else(|| "session ended before media arrived".to_string())?;
        if last_health_ping.elapsed() >= Duration::from_secs(5) {
            health_sequence += 1;
            let _ = session
                .commands
                .send(SessionCommand::Json(serde_json::json!({
                    "type": "health_ping",
                    "timestamp_ms": started.elapsed().as_millis() as u64,
                    "sequence": health_sequence
                })));
            last_health_ping = Instant::now();
        }
        match event {
            SessionEvent::CertificateUntrusted(_) => {
                return Err("server certificate requires interactive trust".to_string());
            }
            SessionEvent::ServerHello(hello) => {
                println!(
                    "server={} codec={} bit_depth={} range={} matrix={} primaries={} transfer={} \
                     pixel_format={} encoder={} encoder_class={} size={}x{} supports_audio={} \
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
                server_active_codec = match hello.codec.as_str() {
                    "h264" => Some(arcen_deck::protocol::VideoCodec::H264),
                    "h265" => Some(arcen_deck::protocol::VideoCodec::H265),
                    "av1" => Some(arcen_deck::protocol::VideoCodec::Av1),
                    _ => None,
                };
                server_active_chroma =
                    active_chroma_from_pixel_format(&hello.color_caps.advertised_pix_fmt);
                if require_multi_monitor {
                    if hello.input_capabilities.pen != InputCapabilityAvailability::Available {
                        return Err(
                            "host did not advertise native pen input for the multi-monitor session"
                                .to_string(),
                        );
                    }
                    require_last_pen_type = hello.server_name.contains("Windows");
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
            SessionEvent::BrokerHello(_) => {
                return Err("media-smoke expects a direct host, not a gateway".to_string());
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
            }
            SessionEvent::MediaReady => {
                let batch = session.media.take_batch();
                if require_multi_monitor && !input_probe_sent && input_probe_points.len() >= 2 {
                    send_multi_monitor_input_probe(&session.commands, &input_probe_points)?;
                    input_probe_sent = true;
                }
                video_packets = batch.telemetry.video_received;
                audio_packets = batch.telemetry.audio_received;
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

                for (header, payload) in batch.audio {
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
                            "media-smoke complete displays_mode={} monitors_decoded={} stream_resolution={}x{} video_packets={} audio_packets={} audio_nonzero_samples={} audio_peak={} elapsed_ms={} packet_fps={:.2}",
                            requested_displays_mode,
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
                if msg_type(&value) == Some("health_stats") {
                    if require_multi_monitor {
                        let stats = serde_json::from_value::<HealthStatsMsg>(value.clone())
                            .map_err(|error| format!("invalid health_stats: {error}"))?;
                        input_probe_confirmed = stats.input_events >= 8
                            && (!require_last_pen_type
                                || matches!(
                                    stats.last_input_type.as_str(),
                                    "pen_event" | "region_pen_event"
                                ));
                        if input_probe_confirmed {
                            println!(
                                "input-probe complete events={} last_type={}",
                                stats.input_events, stats.last_input_type
                            );
                        }
                    }
                    last_health = Some(value);
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

const fn audio_requirement_satisfied(
    require_multi_monitor: bool,
    verify_audio: bool,
    server_supports_audio: bool,
    audio_packets_consumed: u64,
) -> bool {
    require_multi_monitor || !verify_audio || !server_supports_audio || audio_packets_consumed > 0
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
    ];
    Ok(events)
}

fn send_multi_monitor_input_probe(
    commands: &SessionCommandSender,
    points: &[MultiMonitorInputProbePoint],
) -> Result<(), String> {
    for event in multi_monitor_input_probe_events(points)? {
        commands
            .send(SessionCommand::Json(event))
            .map_err(|_| "session closed while sending multi-monitor input probe".to_string())?;
    }
    Ok(())
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find_map(|pair| (pair[0] == flag).then(|| pair[1].clone()))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

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
    fn multi_monitor_smoke_uses_region_authoritative_input_messages() {
        let events = multi_monitor_input_probe_events(&[
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
        ])
        .expect("two-region probe");

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
        assert!(!audio_requirement_satisfied(false, true, true, 0));
        assert!(audio_requirement_satisfied(false, false, true, 0));
        assert!(audio_requirement_satisfied(false, true, false, 0));
        assert!(audio_requirement_satisfied(false, true, true, 1));
        assert!(audio_requirement_satisfied(true, true, true, 0));
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
