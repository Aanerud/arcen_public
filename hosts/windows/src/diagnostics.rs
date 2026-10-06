use std::net::SocketAddr;
use std::time::Duration;

const FIREWALL_RULE_NAME: &str = "Arcen Pier QUIC 18444";
const STARTUP_DIAGNOSTIC_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FirewallRuleSummary {
    pub(crate) exists: bool,
    pub(crate) enabled: Option<bool>,
    pub(crate) profiles: Option<String>,
}

impl FirewallRuleSummary {
    fn missing() -> Self {
        Self {
            exists: false,
            enabled: None,
            profiles: None,
        }
    }
}

pub(crate) fn spawn_startup_debug_diagnostics(listener_addr: SocketAddr) {
    if !tracing::enabled!(target: crate::logging::NET, tracing::Level::DEBUG) {
        return;
    }
    tokio::spawn(async move {
        match tokio::time::timeout(STARTUP_DIAGNOSTIC_TIMEOUT, startup_debug_diagnostics()).await {
            Ok(summary) => tracing::debug!(
                target: crate::logging::NET,
                listener_addr = %listener_addr,
                interface_addresses = %summary.interface_addresses.join(","),
                firewall_rule_exists = summary.firewall.exists,
                firewall_rule_enabled = summary.firewall.enabled,
                firewall_rule_profiles = summary.firewall.profiles.as_deref().unwrap_or("unknown"),
                "Windows Pier startup network diagnostics"
            ),
            Err(_) => tracing::debug!(
                target: crate::logging::NET,
                listener_addr = %listener_addr,
                "Windows Pier startup network diagnostics timed out"
            ),
        }
    });
}

struct StartupDiagnostics {
    interface_addresses: Vec<String>,
    firewall: FirewallRuleSummary,
}

async fn startup_debug_diagnostics() -> StartupDiagnostics {
    let addresses = crate::netinfo::local_interface_addresses();
    let firewall = query_firewall_rule().await.unwrap_or_else(|error| {
        tracing::debug!(target: crate::logging::NET, %error, "firewall diagnostic query failed");
        FirewallRuleSummary::missing()
    });
    StartupDiagnostics {
        interface_addresses: addresses,
        firewall,
    }
}

async fn query_firewall_rule() -> Result<FirewallRuleSummary, String> {
    let mut command = tokio::process::Command::new("netsh.exe");
    command.kill_on_drop(true);
    let output = command
        .args([
            "advfirewall",
            "firewall",
            "show",
            "rule",
            &format!("name={FIREWALL_RULE_NAME}"),
        ])
        .output()
        .await
        .map_err(|error| format!("run netsh: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Ok(parse_firewall_rule(&format!("{stdout}\n{stderr}")))
}

pub(crate) fn parse_firewall_rule(text: &str) -> FirewallRuleSummary {
    let lower = text.to_ascii_lowercase();
    if lower.contains("no rules match") || lower.contains("no rules matched") {
        return FirewallRuleSummary::missing();
    }
    let mut summary = FirewallRuleSummary {
        exists: lower.contains(&FIREWALL_RULE_NAME.to_ascii_lowercase())
            || lower.contains("rule name"),
        enabled: None,
        profiles: None,
    };
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        match key.as_str() {
            "enabled" => {
                summary.enabled = match value.to_ascii_lowercase().as_str() {
                    "yes" | "true" => Some(true),
                    "no" | "false" => Some(false),
                    _ => None,
                };
            }
            "profiles" | "profile" => summary.profiles = Some(value.to_owned()),
            _ => {}
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firewall_parser_extracts_enabled_and_profiles() {
        let parsed = parse_firewall_rule(
            "Rule Name:                            Arcen Pier QUIC 18444\n\
             Enabled:                              Yes\n\
             Profiles:                             Domain,Private\n",
        );
        assert!(parsed.exists);
        assert_eq!(parsed.enabled, Some(true));
        assert_eq!(parsed.profiles.as_deref(), Some("Domain,Private"));
    }

    #[test]
    fn firewall_parser_reports_missing_rule() {
        assert_eq!(
            parse_firewall_rule("No rules match the specified criteria."),
            FirewallRuleSummary::missing()
        );
    }
}
