//! `arcen-host` binary entry point.
//!
//! Parses the CLI, brings up the logging backbone, installs the rustls crypto
//! provider, then runs the TLS/WebSocket server (`net::serve`) on a Tokio
//! runtime until SIGINT/SIGTERM. PAM authentication is available in Stage 2;
//! native display control and input arrive in later stages.

use std::process::ExitCode;
use std::sync::Arc;

use arcen_pier_linux::cli;
use arcen_pier_linux::logging::{self, target};
use arcen_pier_linux::net;

#[cfg(feature = "wss-compat")]
const TLS_VERSION_USAGE: &str = "TLS1.2|TLS1.3";
#[cfg(not(feature = "wss-compat"))]
const TLS_VERSION_USAGE: &str = "TLS1.3";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if let Some(code) = dispatch_multicall(&args) {
        return code;
    }

    fn dispatch_multicall(args: &[String]) -> Option<ExitCode> {
        let subcommand = args.get(1)?.as_str();
        let helper_args = || {
            let mut rewritten = Vec::with_capacity(args.len() - 1);
            rewritten.push(format!("arcen-{subcommand}"));
            rewritten.extend(args.iter().skip(2).cloned());
            rewritten
        };
        match subcommand {
            "capenc" => {
                arcen_capenc::run_with_args(helper_args());
                Some(ExitCode::SUCCESS)
            }
            "audiocap" => {
                arcen_audiocap::run_with_args(&helper_args());
                Some(ExitCode::SUCCESS)
            }
            "input-helper" => Some(match arcen_input_helper::run_with_args(&helper_args()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("input-helper: {error}");
                    ExitCode::FAILURE
                }
            }),
            "session-agent" => {
                let args = args.iter().skip(2).cloned().collect::<Vec<_>>();
                Some(arcen_pier_linux::session::agent::agent_main(&args))
            }
            "session-launcher" => {
                let args = args.iter().skip(2).cloned().collect::<Vec<_>>();
                Some(arcen_pier_linux::session::launcher::launcher_main(&args))
            }
            "usb-bridge-helper" => {
                let args = args.iter().skip(2).cloned().collect::<Vec<_>>();
                Some(arcen_pier_linux::usb_bridge::helper_main(&args))
            }
            "usb-bridge-ipc-self-test" => Some(arcen_pier_linux::usb_bridge::ipc_self_test_main()),
            "new-host-cert" => Some(arcen_pier_linux::host_cert::main(&args[2..])),
            _ => None,
        }
    }

    if args
        .get(1)
        .is_some_and(|argument| argument == "support-bundle")
    {
        let options = match arcen_pier_linux::support_bundle::parse_options(&args[2..]) {
            Ok(options) => options,
            Err(error) => {
                eprintln!("support bundle arguments failed: {error}");
                return ExitCode::FAILURE;
            }
        };
        return match arcen_pier_linux::support_bundle::run(&options) {
            Ok(result) => {
                println!("{}", result.path.display());
                if result.omission_count != 0 {
                    eprintln!(
                        "support bundle completed with {} unavailable or omitted sources",
                        result.omission_count
                    );
                }
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("support bundle failed: {error}");
                ExitCode::FAILURE
            }
        };
    }

    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("arcen-host {}", arcen_pier_linux::VERSION);
        println!("{}", arcen_pier_linux::SOURCE_OFFER);
        println!("Source: {}", arcen_pier_linux::SOURCE_URL);
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return ExitCode::SUCCESS;
    }
    if args
        .get(1)
        .is_some_and(|argument| argument == "validate-config")
    {
        return match resolve_startup(&args[2..]) {
            Ok(_) => {
                println!("Pier configuration is valid.");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("invalid configuration: {error}");
                ExitCode::FAILURE
            }
        };
    }

    let (config, logging_options) = match resolve_startup(&args[1..]) {
        Ok(resolved) => resolved,
        Err(error) => {
            eprintln!("configuration failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    let log_controller = match logging::init(&logging_options) {
        Ok(controller) => Arc::new(controller),
        Err(error) => {
            eprintln!("logging setup failed: {error}");
            return ExitCode::FAILURE;
        }
    };

    let config = Arc::new(config);

    tracing::info!(
        target: target::NET,
        version = %arcen_pier_linux::VERSION,
        license = "AGPL-3.0-only",
        source = arcen_pier_linux::SOURCE_URL,
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        host = %config.host,
        port = config.port,
        audio = config.audio_enabled,
        audio_compressed = config.audio_compressed,
        microphone_input = config.microphone_input_enabled,
        "arcen-host starting"
    );
    if logging_options.retention_was_clamped {
        tracing::warn!(
            target: target::NET,
            retention_days = logging_options.policy.retention_days(),
            "configured log retention was clamped to the supported range"
        );
    }
    match log_controller.managed_log_path() {
        Some(path) => tracing::info!(
            target: target::NET,
            log = %path.display(),
            "managed logging backbone up"
        ),
        None => tracing::info!(
            target: target::NET,
            log_dir = %logging::log_dir().display(),
            "legacy logging backbone up (rolling file + stderr)"
        ),
    }

    #[cfg(not(target_os = "linux"))]
    tracing::warn!(
        target: target::NET,
        "running the LINUX host binary on {}: WS/TLS/handshake/relay are exercised, \
         but real capture/encode needs a Linux GPU host — use a stub via --capenc-bin",
        std::env::consts::OS
    );

    // Install the ring crypto provider ONCE before any rustls ServerConfig.
    net::tls::init_crypto();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(target: target::NET, error = %e, "failed to build Tokio runtime");
            flush_logging_before_exit(&log_controller);
            return ExitCode::FAILURE;
        }
    };

    if let Err(error) = runtime.block_on(arcen_pier_linux::deskside::recover_pending_display()) {
        tracing::error!(
            target: target::NET,
            %error,
            "deskside startup recovery failed; refusing service readiness"
        );
        flush_logging_before_exit(&log_controller);
        return ExitCode::FAILURE;
    }

    let admission_runtime = arcen_pier_linux::session_admission::SessionAdmissionRuntime::new();

    match runtime.block_on(net::serve(
        config,
        Arc::clone(&log_controller),
        admission_runtime,
    )) {
        Ok(()) => {
            tracing::info!(target: target::NET, "arcen-host stopped");
            flush_logging_before_exit(&log_controller);
            ExitCode::SUCCESS
        }
        Err(e) => {
            tracing::error!(target: target::NET, error = %e, "server exited with error");
            flush_logging_before_exit(&log_controller);
            ExitCode::FAILURE
        }
    }
}

/// Bounded best-effort drain of every registered observability sink before
/// process exit. The runtime is never installed via `install_global`, so
/// this explicit flush is the only way to guarantee queued canonical/dev
/// console/journald deliveries are attempted before the process exits.
fn flush_logging_before_exit(log_controller: &logging::LogController) {
    const SHUTDOWN_FLUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
    if let Err(error) = log_controller.shutdown(SHUTDOWN_FLUSH_TIMEOUT) {
        eprintln!("logging shutdown flush incomplete: {error}");
    }
}

fn resolve_startup(args: &[String]) -> Result<(cli::Config, logging::LoggingOptions), String> {
    let config = cli::parse(args)?;
    let logging = logging::LoggingOptions::from_config(&config, args)?;
    Ok((config, logging))
}

fn print_help() {
    println!(
        "arcen-host {ver} — native Linux host (control plane)\n\
         \n\
         USAGE:\n    arcen-host [FLAGS] [OPTIONS]\n\
         \x20  arcen-pier support-bundle [--out <DIR>]\n\
         \x20  arcen-pier validate-config [--config <PATH>] [overrides]\n\
         \x20  arcen-pier capenc|audiocap|input-helper|session-agent|session-launcher ...\n\
         \x20  arcen-pier new-host-cert [--directory <DIR>]\n\
         \n\
         FLAGS:\n\
         \x20   -V, --version    Print version and exit\n\
         \x20   -h, --help       Print this help and exit\n\
         \x20   -q, --quiet      Warnings and errors only\n\
         \x20   -v, --verbose    Per-event debug logging\n\
         \x20   -vv, --trace     Firehose logging\n\
         \x20       --verbosity <0..3>  Coarse logging tier\n\
         \x20       --no-auth    Disable authentication (Stage 1 default)\n\
         \x20       --unsafe-allow-remote-no-auth\n\
         \x20                      Expose no-auth mode beyond loopback (isolated dev only)\n\
         \n\
         OPTIONS:\n\
         \x20       --config <PATH>      Unified Pier JSON (default /etc/arcen/pier.json)\n\
         \x20       --no-config         Skip JSON for CLI-only diagnostics\n\
         \x20       --host <ADDR>        Bind address (default 127.0.0.1)\n\
         \x20       --port <PORT>        Direct QUIC UDP port (default 18444)\n\
         \x20       --quic-port <PORT>   Deprecated QUIC UDP port alias\n\
         \x20       --tls-cert <PATH>    Required QUIC TLS 1.3 certificate chain (PEM)\n\
         \x20       --tls-key <PATH>     Required TLS private key (PEM)\n\
         \x20       --tls-minimum-version <{tls_version_usage}>  Minimum TLS version\n\
         \x20       --tls-disabled-cipher-suite <IANA-NAME>  Repeatable ring suite blacklist\n\
         \x20       --tls-expiry-warning-days <0..3650>  Expiry warning window (default 30)\n\
         \x20       --tls-expected-san <DNS|IP>  Repeatable exact SAN requirement\n\
         \x20       --encoder <auto|nvenc|software-h264>  Encoder backend (default auto)\n\
         \x20       --codec <h264|h265>  Video codec (default h264)\n\
         \x20       --chroma <yuv420|yuv444>  Chroma; yuv444 requires h265 (default yuv420)\n\
         \x20       --bit-depth <8|10|12>  Coded component depth (default 8; 12 needs the software tier)\n\
         \x20       --color-range <limited|full>  Coded sample range (default limited)\n\
         \x20       --color-matrix <identity|bt709|bt601|bt2020ncl>  Matrix coefficients (default bt709)\n\
         \x20       --color-policy <always-on|always-off|default-on|default-off>\n\
         \x20                      Colour fidelity ceiling/default vs. client negotiation (default default-off)\n\
         \x20       --variant <id>       Probe-matrix variant id; overrides codec/chroma/bit-depth/range/matrix\n\
         \x20       --fps <N>            Target FPS (default 60)\n\
         \x20       --monitor <N>        1-based monitor index (default 1)\n\
         \x20       --display <:N>       X display to capture (default :0)\n\
         \x20       --xauthority <PATH>  XAUTHORITY for the capture child\n\
         \x20       --capenc-bin <PATH>  Ignored legacy external-helper path\n\
         \x20       --auth-mode <none|pam>  Authentication mode (default none)\n\
         \x20       --pam-service <NAME>    PAM service (default login)\n\
         \x20       --reconnect-window-secs <0..7200>  Direct resume window (default 1200)\n\
         \x20       --timezone-redirection  Redirect authenticated desktop TZ (default off)\n\
         \x20       --no-timezone-redirection  Disable authenticated desktop TZ redirection\n\
         \x20       --zoneinfo-root <PATH>  Zoneinfo database root (default /usr/share/zoneinfo)\n\
         \x20       --disclaimer            Require disclaimer acceptance before PAM\n\
         \x20       --disclaimer-dir <PATH> Directory containing locale text files\n\
         \x20       --disclaimer-locale <ID> Disclaimer locale (default en_US)\n\
         \x20       --session-agent-bin <PATH>  Explicit arcen-session-agent binary\n\
         \x20       --session-launcher-bin <PATH>  Explicit privileged PAM launcher binary\n\
         \x20       --desktop-session <gnome|gnome-classic>  Desktop (default gnome)\n\
         \x20       --session-display <:N>  Dedicated PAM display (default :10; never :0)\n\
         \x20       --session-gpu-head <DFP-N>  Dedicated NVIDIA head (default DFP-1)\n\
         \x20       --xorg-bin <PATH>  Xorg executable (default /usr/libexec/Xorg)\n\
         \x20       --xorg-config-template <PATH>  Single-head NVIDIA Xorg template\n\
         \x20       --session-runtime-root <PATH>  Root-owned session artifact directory\n\
         \x20       --input-mode <none|uinput>  Native input backend (default none)\n\
         \x20       --deskside          Require physical-console input/display privacy\n\
         \x20       --deskside-firmware-sha256 <HASH>  Pinned normalized DMI/chassis hash\n\
         \x20       --deskside-console-uid <UID>  Expected active local seat0 owner\n\
         \x20       --deskside-console-display <:N>  Physical console DISPLAY\n\
         \x20       --deskside-console-xauthority <PATH>  Physical console Xauthority\n\
         \x20       --deskside-input </dev/input/by-id/...>  Repeat for every keyboard/pointer\n\
         \x20       --deskside-output <NAME,DRM_SHA256,EDID_SHA256>  Repeat for every output\n\
         \x20       --multi-monitor     Advertise multi_monitor_v1 (still gated off by default)\n\
         \x20       --multi-monitor-head <DFP-N>  Repeat for every head this host may plan onto\n\
         \x20       --audio / --no-audio  Enable/disable host audio\n\
         \x20       --audio-compressed / --audio-uncompressed\n\
         \x20                           Force Opus 128 kbps or uncompressed PCM\n\
         \x20       --audiocap-bin <PATH> Ignored legacy external-helper path\n\
         \x20       --audio-user <session|host>  Audio runtime owner (default session)\n\
         \x20       --microphone-input / --no-microphone-input\n\
         \x20                           Allow/deny Deck microphone publication\n\
         \x20       --pactl-bin <PATH>   Session-user PipeWire-Pulse/PulseAudio control binary\n\
         \x20       --clipboard-direction <both|client_to_host|host_to_client|disabled>\n\
         \x20       --clipboard-content <all|text|image>\n\
         \x20       --clipboard-max-bytes <BYTES>  Encoded cap, 1 MiB through 20 MiB\n\
         \x20       --no-clipboard       Disable clipboard advertisement and X11 child\n\
         \x20       --managed-log <PATH>  Fixed file reopened on SIGHUP\n\
         \n\
         ENV:\n\
         \x20   ARCEN_LOG        EnvFilter override (e.g. arcen::capenc=debug)\n\
         \x20   ARCEN_LOG_DIR    Override the legacy rolling-log directory\n\
         \n\
         {offer}\n\
         Source: {source}",
        ver = arcen_pier_linux::VERSION,
        tls_version_usage = TLS_VERSION_USAGE,
        offer = arcen_pier_linux::SOURCE_OFFER,
        source = arcen_pier_linux::SOURCE_URL
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_args() -> Vec<String> {
        [
            "--no-config",
            "--tls-cert",
            "host.crt",
            "--tls-key",
            "host.key",
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    }

    #[test]
    fn validation_uses_the_complete_startup_resolver() {
        let mut invalid_verbosity = base_args();
        invalid_verbosity.extend(["--verbosity".to_string(), "9".to_string()]);
        assert!(resolve_startup(&invalid_verbosity).is_err());

        let mut missing_log_path = base_args();
        missing_log_path.push("--managed-log".to_string());
        assert!(resolve_startup(&missing_log_path).is_err());

        let mut unknown = base_args();
        unknown.push("--surprise".to_string());
        assert!(resolve_startup(&unknown).is_err());

        let mut positional = base_args();
        positional.push("unexpected".to_string());
        assert!(resolve_startup(&positional).is_err());
    }
}
