//! Linux adapter for NVIDIA head discovery.
//!
//! The pure parser/ranker lives in `arcen_outputs`; this module only starts a
//! short-lived Xorg probe and reads its log.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arcen_media::MAX_MULTI_MONITOR_COUNT;
use thiserror::Error;

use crate::cli::Config;

const PROBE_DISPLAY: &str = ":97";
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const DISCOVERY_CACHE_FILE: &str = "nvidia-heads.json";
const DISCOVERY_CACHE_VERSION: u8 = 1;

#[derive(Debug, Clone)]
pub struct NvidiaHeadDiscovery {
    pub heads: Vec<String>,
    pub source: DiscoverySource,
    pub log_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoverySource {
    Probe,
}

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("multi-monitor advertisement disabled by configuration")]
    Disabled,
    #[error("administrator configured explicit multi-monitor heads")]
    ExplicitOverride,
    #[error("Xorg binary is not executable: {0}")]
    MissingXorg(PathBuf),
    #[error("Xorg template is not readable: {0}")]
    MissingTemplate(PathBuf),
    #[error("create discovery directory {0}: {1}")]
    CreateDirectory(PathBuf, io::Error),
    #[error("write probe Xorg template {0}: {1}")]
    WriteTemplate(PathBuf, io::Error),
    #[error("start Xorg NVIDIA head probe: {0}")]
    Spawn(io::Error),
    #[error("Xorg NVIDIA head probe timed out")]
    Timeout,
    #[error("read Xorg NVIDIA head probe log {0}: {1}")]
    ReadLog(PathBuf, io::Error),
    #[error("NVIDIA head probe found no DFP outputs with pixel clocks")]
    NoHeads,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct DiscoveryCache {
    version: u8,
    heads: Vec<String>,
    log_path: String,
}

pub fn apply_startup_discovery(config: &mut Config) {
    if !config.multi_monitor.advertise_enabled || !config.multi_monitor.heads.is_empty() {
        return;
    }
    if config.encoder != crate::media::capenc::EncoderSelection::Auto
        && config.encoder != crate::media::capenc::EncoderSelection::NativeNvenc
    {
        tracing::warn!(
            target: crate::logging::target::SESSION,
            encoder = ?config.encoder,
            "Linux multi-monitor automatic discovery requires auto/nvenc; multi-monitor will not be advertised"
        );
        config.multi_monitor.heads.clear();
        return;
    }
    match discover_nvidia_heads(config) {
        Ok(discovery) => {
            config.multi_monitor.heads = discovery.heads;
            tracing::info!(
                target: crate::logging::target::SESSION,
                heads = ?config.multi_monitor.heads,
                source = ?discovery.source,
                log = %discovery.log_path.display(),
                configured_encoder = ?config.encoder,
                multi_monitor_encoder = ?config.resolved_multi_monitor_encoder(),
                "Linux multi-monitor NVIDIA head discovery succeeded"
            );
        }
        Err(error) => {
            config.multi_monitor.heads.clear();
            tracing::warn!(
                target: crate::logging::target::SESSION,
                %error,
                "Linux multi-monitor NVIDIA head discovery found no usable heads; multi-monitor will not be advertised"
            );
        }
    }
}

pub fn discover_nvidia_heads(config: &Config) -> Result<NvidiaHeadDiscovery, DiscoveryError> {
    if !config.multi_monitor.advertise_enabled {
        return Err(DiscoveryError::Disabled);
    }
    if !config.multi_monitor.heads.is_empty() {
        return Err(DiscoveryError::ExplicitOverride);
    }
    if !is_executable(&config.xorg_bin) {
        return Err(DiscoveryError::MissingXorg(config.xorg_bin.clone()));
    }
    if !config.xorg_config_template.is_file() {
        return Err(DiscoveryError::MissingTemplate(
            config.xorg_config_template.clone(),
        ));
    }
    let discovery_dir = discovery_dir(config);
    fs::create_dir_all(&discovery_dir)
        .map_err(|error| DiscoveryError::CreateDirectory(discovery_dir.clone(), error))?;
    run_xorg_probe(config, &discovery_dir)
}

fn discovery_dir(config: &Config) -> PathBuf {
    if config.session_runtime_root.is_absolute() {
        config.session_runtime_root.join("discovery")
    } else {
        PathBuf::from("/var/lib/arcen/discovery")
    }
}

fn run_xorg_probe(
    config: &Config,
    discovery_dir: &Path,
) -> Result<NvidiaHeadDiscovery, DiscoveryError> {
    let template = fs::read_to_string(&config.xorg_config_template)
        .map_err(|error| DiscoveryError::ReadLog(config.xorg_config_template.clone(), error))?;
    let probe_config_path = discovery_dir.join("nvidia-head-probe-xorg.conf");
    fs::write(&probe_config_path, probe_template(&template))
        .map_err(|error| DiscoveryError::WriteTemplate(probe_config_path.clone(), error))?;
    let _ = fs::set_permissions(&probe_config_path, fs::Permissions::from_mode(0o600));

    let log_path = discovery_dir.join(format!("nvidia-head-probe-{}.log", timestamp_secs()));
    let mut child = Command::new(&config.xorg_bin)
        .arg(PROBE_DISPLAY)
        .arg("-config")
        .arg(&probe_config_path)
        .arg("-logfile")
        .arg(&log_path)
        .arg("-noreset")
        .arg("-novtswitch")
        .arg("-nolisten")
        .arg("tcp")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(DiscoveryError::Spawn)?;

    let start = Instant::now();
    loop {
        if child.try_wait().map_err(DiscoveryError::Spawn)?.is_some() {
            break;
        }
        if start.elapsed() >= PROBE_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();

    let log = fs::read_to_string(&log_path)
        .map_err(|error| DiscoveryError::ReadLog(log_path.clone(), error))?;
    let heads = arcen_outputs::choose_nvidia_xorg_heads(&log, MAX_MULTI_MONITOR_COUNT);
    if heads.is_empty() {
        return Err(DiscoveryError::NoHeads);
    }
    store_cache(discovery_dir, &heads, &log_path);
    Ok(NvidiaHeadDiscovery {
        heads,
        source: DiscoverySource::Probe,
        log_path,
    })
}

/// Outputs the probe asks the driver for. A vGPU reports only the heads its
/// profile provides, and only when they are requested; a physical board
/// reports its outputs with their pixel clocks either way.
const PROBE_CONNECTED_MONITORS: &str = "DFP-0,DFP-1,DFP-2,DFP-3,DFP-4,DFP-5,DFP-6,DFP-7";

fn probe_template(template: &str) -> String {
    let mut out = String::with_capacity(template.len() + 128);
    let mut in_nvidia_device = false;
    let mut asked = false;
    for line in template.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Section") && trimmed.contains("\"Device\"") {
            in_nvidia_device = true;
            asked = false;
        }
        // The session's own head selection would hide every other output.
        if trimmed.starts_with("Option")
            && (trimmed.contains("\"ConnectedMonitor\"")
                || trimmed.contains("\"UseDisplayDevice\"")
                || trimmed.contains("\"MetaModes\""))
        {
            continue;
        }
        if in_nvidia_device && trimmed == "EndSection" {
            if !asked {
                out.push_str(&format!(
                    "    Option         \"ConnectedMonitor\" \"{PROBE_CONNECTED_MONITORS}\"\n"
                ));
                asked = true;
            }
            in_nvidia_device = false;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.replace(
        "\"AutoAddDevices\" \"false\"",
        "\"AutoAddDevices\" \"true\"",
    )
    .replace(
        "\"AutoEnableDevices\" \"false\"",
        "\"AutoEnableDevices\" \"true\"",
    )
}

fn store_cache(discovery_dir: &Path, heads: &[String], log_path: &Path) {
    let cache = DiscoveryCache {
        version: DISCOVERY_CACHE_VERSION,
        heads: heads.to_vec(),
        log_path: log_path.display().to_string(),
    };
    let Ok(bytes) = serde_json::to_vec_pretty(&cache) else {
        return;
    };
    let _ = fs::write(discovery_dir.join(DISCOVERY_CACHE_FILE), bytes);
}

fn is_executable(path: &Path) -> bool {
    path.is_file()
        && fs::metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

fn timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_probe_asks_for_every_head_instead_of_the_sessions_one() {
        let template = "Section \"Device\"\n    Identifier \"Device0\"\n    Driver \"nvidia\"\n    Option \"ConnectedMonitor\" \"DFP-0\"\n    Option \"MetaModes\" \"DFP-0: nvidia-auto-select +0+0\"\nEndSection\nSection \"ServerFlags\"\n    Option \"AutoAddDevices\" \"false\"\nEndSection\n";
        let probe = probe_template(template);
        assert!(!probe.contains("MetaModes"));
        assert!(!probe.contains("\"ConnectedMonitor\" \"DFP-0\"\n"));
        assert_eq!(probe.matches(PROBE_CONNECTED_MONITORS).count(), 1);
        assert!(probe.contains("\"AutoAddDevices\" \"true\""));
    }
}
