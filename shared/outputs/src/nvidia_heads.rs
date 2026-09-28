//! Pure NVIDIA Xorg output discovery parsing and ranking.
//!
//! Linux Piers collect the raw facts from an NVIDIA Xorg log; this module owns
//! the portable interpretation: find `DFP-N` outputs, remember connection
//! state and maximum pixel clock, then choose the highest-clocked heads for a
//! multi-display session.

use std::collections::BTreeMap;

use arcen_media::MAX_MULTI_MONITOR_COUNT;

/// Parsed facts for one NVIDIA `DFP-N` output from an Xorg log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvidiaXorgOutput {
    pub name: String,
    pub connected: Option<bool>,
    /// Pixel clock in tenths of MHz, preserving NVIDIA's common one-decimal
    /// log format without floating-point ordering.
    pub max_pixel_clock_mhz_tenths: Option<u32>,
}

impl NvidiaXorgOutput {
    #[must_use]
    pub fn max_pixel_clock_mhz(&self) -> Option<String> {
        self.max_pixel_clock_mhz_tenths
            .map(|tenths| format!("{}.{:01}", tenths / 10, tenths % 10))
    }
}

/// Returns true for an NVIDIA digital flat-panel output token such as `DFP-0`.
#[must_use]
pub fn is_nvidia_dfp_head_token(token: &str) -> bool {
    nvidia_dfp_head_index(token).is_some()
}

/// Parses the numeric suffix of a `DFP-N` token.
#[must_use]
pub fn nvidia_dfp_head_index(token: &str) -> Option<u16> {
    let digits = token.strip_prefix("DFP-")?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u16>().ok()
}

/// Parses every NVIDIA `DFP-N` output mentioned in an Xorg log.
///
/// Duplicate lines for the same output are merged; the last observed
/// connection state and pixel clock win, matching Xorg logs that repeat GPU
/// probing during server startup.
#[must_use]
pub fn parse_nvidia_xorg_outputs(log: &str) -> Vec<NvidiaXorgOutput> {
    let mut outputs: BTreeMap<String, NvidiaXorgOutput> = BTreeMap::new();
    for line in log.lines() {
        let Some((name, token_end)) = extract_dfp_token(line) else {
            continue;
        };
        let entry = outputs.entry(name.clone()).or_insert(NvidiaXorgOutput {
            name,
            connected: None,
            max_pixel_clock_mhz_tenths: None,
        });
        if let Some(connected) = parse_connection_state(line, token_end) {
            entry.connected = Some(connected);
        }
        if let Some(clock) = parse_pixel_clock_tenths(line) {
            entry.max_pixel_clock_mhz_tenths = Some(clock);
        }
    }
    outputs.into_values().collect()
}

/// Chooses up to `limit` NVIDIA heads, ranked by maximum pixel clock.
///
/// An output qualifies when the driver logged a maximum pixel clock for it or
/// logged it as connected. Physical boards list every output with its clock,
/// often as disconnected until a session forces it on, so the clock ranks
/// them. A vGPU logs no clocks at all, only the heads its profile provides as
/// connected (once the probe asks for them), so those are taken in index
/// order after any clocked output.
#[must_use]
pub fn rank_nvidia_xorg_heads(outputs: &[NvidiaXorgOutput], limit: usize) -> Vec<String> {
    let mut candidates = outputs
        .iter()
        .filter_map(|output| {
            let connected = output.connected == Some(true);
            let clock = match output.max_pixel_clock_mhz_tenths {
                Some(clock) => clock,
                None if connected => 0,
                None => return None,
            };
            let index = nvidia_dfp_head_index(&output.name)?;
            Some((
                output.name.clone(),
                clock,
                output.connected.unwrap_or(false),
                index,
            ))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| right.2.cmp(&left.2))
            .then_with(|| left.3.cmp(&right.3))
            .then_with(|| left.0.cmp(&right.0))
    });
    candidates
        .into_iter()
        .take(limit.min(MAX_MULTI_MONITOR_COUNT))
        .map(|(name, _, _, _)| name)
        .collect()
}

/// Parses and ranks NVIDIA Xorg outputs in one call.
#[must_use]
pub fn choose_nvidia_xorg_heads(log: &str, limit: usize) -> Vec<String> {
    rank_nvidia_xorg_heads(&parse_nvidia_xorg_outputs(log), limit)
}

fn extract_dfp_token(line: &str) -> Option<(String, usize)> {
    let start = line.find("DFP-")?;
    let digit_start = start + "DFP-".len();
    let digit_len = line[digit_start..]
        .bytes()
        .take_while(u8::is_ascii_digit)
        .count();
    if digit_len == 0 {
        return None;
    }
    let end = digit_start + digit_len;
    Some((line[start..end].to_string(), end))
}

fn parse_connection_state(line: &str, token_end: usize) -> Option<bool> {
    let after_token = line.get(token_end..)?;
    let (_, after_colon) = after_token.split_once(':')?;
    let state = after_colon.trim_start();
    if state.starts_with("connected") {
        Some(true)
    } else if state.starts_with("disconnected") {
        Some(false)
    } else {
        None
    }
}

fn parse_pixel_clock_tenths(line: &str) -> Option<u32> {
    let (before_marker, _) = line.split_once(" MHz maximum pixel clock")?;
    let token = before_marker.split_whitespace().next_back()?;
    parse_decimal_tenths(token)
}

fn parse_decimal_tenths(token: &str) -> Option<u32> {
    let (whole, fraction) = token.split_once('.').unwrap_or((token, "0"));
    let whole = whole.parse::<u32>().ok()?;
    let tenth = fraction
        .bytes()
        .next()
        .filter(u8::is_ascii_digit)
        .map_or(0, |byte| u32::from(byte - b'0'));
    whole.checked_mul(10)?.checked_add(tenth)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic NVIDIA Xorg log: six outputs, alternating DisplayPort
    /// (high pixel clock) and TMDS (low), two of them connected.
    const QUADRO_XORG_EXCERPT: &str = r#"
        [    42.017] (--) NVIDIA(GPU-0): ACME Panel (DFP-0): connected
        [    42.017] (--) NVIDIA(GPU-0): ACME Panel (DFP-0): Internal DisplayPort
        [    42.017] (--) NVIDIA(GPU-0): ACME Panel (DFP-0): 1200.0 MHz maximum pixel clock
        [    42.017] (--) NVIDIA(GPU-0): ACME Panel (DFP-1): connected
        [    42.017] (--) NVIDIA(GPU-0): ACME Panel (DFP-1): Internal TMDS
        [    42.017] (--) NVIDIA(GPU-0): ACME Panel (DFP-1): 225.0 MHz maximum pixel clock
        [    42.017] (--) NVIDIA(GPU-0): DFP-2: disconnected
        [    42.017] (--) NVIDIA(GPU-0): DFP-2: Internal DisplayPort
        [    42.017] (--) NVIDIA(GPU-0): DFP-2: 1200.0 MHz maximum pixel clock
        [    42.017] (--) NVIDIA(GPU-0): DFP-3: disconnected
        [    42.017] (--) NVIDIA(GPU-0): DFP-3: Internal TMDS
        [    42.017] (--) NVIDIA(GPU-0): DFP-3: 225.0 MHz maximum pixel clock
        [    42.017] (--) NVIDIA(GPU-0): DFP-4: disconnected
        [    42.017] (--) NVIDIA(GPU-0): DFP-4: Internal DisplayPort
        [    42.017] (--) NVIDIA(GPU-0): DFP-4: 1200.0 MHz maximum pixel clock
        [    42.017] (--) NVIDIA(GPU-0): DFP-5: disconnected
        [    42.017] (--) NVIDIA(GPU-0): DFP-5: Internal TMDS
        [    42.017] (--) NVIDIA(GPU-0): DFP-5: 225.0 MHz maximum pixel clock
        [    42.017] (--) NVIDIA(GPU-0): DFP-6: disconnected
        [    42.017] (--) NVIDIA(GPU-0): DFP-6: Internal DisplayPort
        [    42.017] (--) NVIDIA(GPU-0): DFP-6: 1200.0 MHz maximum pixel clock
    "#;

    #[test]
    fn parses_nvidia_xorg_output_facts() {
        let outputs = parse_nvidia_xorg_outputs(QUADRO_XORG_EXCERPT);
        let dfp4 = outputs
            .iter()
            .find(|output| output.name == "DFP-4")
            .expect("DFP-4");
        assert_eq!(dfp4.connected, Some(false));
        assert_eq!(dfp4.max_pixel_clock_mhz_tenths, Some(12_000));
        assert_eq!(dfp4.max_pixel_clock_mhz().as_deref(), Some("1200.0"));
    }

    #[test]
    fn chooses_highest_pixel_clock_heads_in_stable_order() {
        assert_eq!(
            choose_nvidia_xorg_heads(QUADRO_XORG_EXCERPT, 4),
            ["DFP-0", "DFP-2", "DFP-4", "DFP-6"]
        );
    }

    #[test]
    fn keeps_k_series_style_fewer_than_four_heads() {
        let excerpt = r#"
            [ 11.100] (--) NVIDIA(GPU-0): Quadro K2200 (DFP-0): connected
            [ 11.100] (--) NVIDIA(GPU-0): Quadro K2200 (DFP-0): 540.0 MHz maximum pixel clock
            [ 11.100] (--) NVIDIA(GPU-0): DFP-1: disconnected
            [ 11.100] (--) NVIDIA(GPU-0): DFP-1: 330.0 MHz maximum pixel clock
        "#;
        assert_eq!(choose_nvidia_xorg_heads(excerpt, 4), ["DFP-0", "DFP-1"]);
    }

    #[test]
    fn ignores_disconnected_outputs_without_pixel_clock() {
        let excerpt = r#"
            [ 1.0] (--) NVIDIA(GPU-0): DFP-0: disconnected
            [ 1.0] (--) NVIDIA(GPU-0): DFP-1: connected
            [ 1.0] (--) NVIDIA(GPU-0): DFP-1: 165.0 MHz maximum pixel clock
        "#;
        assert_eq!(choose_nvidia_xorg_heads(excerpt, 4), ["DFP-1"]);
    }

    #[test]
    fn a_vgpu_log_without_pixel_clocks_keeps_its_connected_heads_in_order() {
        // Shape of a GRID vGPU probe that asked for DFP-0..7: the profile
        // provides four heads and the driver logs no pixel clocks.
        let excerpt = r#"
            [ 9.1] (--) NVIDIA(GPU-0): NVIDIA VGX (DFP-2): connected
            [ 9.1] (--) NVIDIA(GPU-0): NVIDIA VGX (DFP-2): External TMDS
            [ 9.1] (--) NVIDIA(GPU-0): NVIDIA VGX (DFP-0): connected
            [ 9.1] (--) NVIDIA(GPU-0): NVIDIA VGX (DFP-1): connected
            [ 9.1] (--) NVIDIA(GPU-0): NVIDIA VGX (DFP-3): connected
            [ 9.1] (**) NVIDIA(GPU-0): Mode Validation Overrides for NVIDIA VGX (DFP-4):
        "#;
        assert_eq!(
            choose_nvidia_xorg_heads(excerpt, 4),
            ["DFP-0", "DFP-1", "DFP-2", "DFP-3"]
        );
    }
}
