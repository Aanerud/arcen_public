//! Pure multi-head NVIDIA Xorg configuration generator for the
//! `multi_monitor_v1` Linux vertical slice.
//!
//! This replaces the single-token `DFP-N` substitution
//! (`session::launcher::render_xorg_config`) with a session-specific
//! generated configuration capable of driving 1-4 NVIDIA heads/MetaModes on
//! one dedicated Xorg screen (the same NVIDIA TwinView/BigDesktop-style
//! `ConnectedMonitor`/`MetaModes` technique the shipped template's header
//! comment already documents for commercial remote-desktop-style headless
//! GPU sessions). The existing single-head template path
//! (`render_xorg_config`) and its recovery/retry behavior in
//! `session::launcher::DedicatedXorg` are left completely unchanged for
//! sessions without a committed plan; this module adds a new, pure,
//! independently testable renderer that `DedicatedXorg::start` selects
//! instead whenever a [`LinuxTopologyPlan`] is present. That plan is carried
//! across the privileged `session-launcher` subprocess IPC boundary as part
//! of the `OpenRequest` (see `session::launcher::MultiHeadPlanMsg`) once
//! `SessionRegistry::acquire` has committed one; real multi-monitor session
//! establishment still only ever reaches this path behind the
//! capenc-supervisor carrier gate (see
//! `media::multi_capenc::MULTI_MONITOR_CARRIER_READY`) and the operator's
//! own explicit, default-off `--multi-monitor` configuration.
//!
//! This module performs no process/X-server I/O; it only transforms the
//! template text.

use crate::display::topology::{LinuxMonitorPlan, LinuxTopologyPlan};
use arcen_media::Rotation;
use thiserror::Error;

/// Typed rejection from the pure multi-head Xorg config renderer.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum XorgMultiHeadConfigError {
    #[error("topology plan contains no monitors")]
    EmptyPlan,
    #[error("xorg template must contain exactly one `ConnectedMonitor` option line, found {0}")]
    ConnectedMonitorLineCount(usize),
    #[error("xorg template must contain exactly one `MetaModes` option line, found {0}")]
    MetaModesLineCount(usize),
    #[error("xorg template must contain exactly one `Virtual` display line, found {0}")]
    VirtualLineCount(usize),
}

/// Renders a session-specific multi-head Xorg configuration from `template`
/// (the same structural template asset as the single-head path,
/// `packaging/linux/arcen-xorg.conf`) and a validated [`LinuxTopologyPlan`].
///
/// Only the `ConnectedMonitor`/`MetaModes` `Option` lines and the `Virtual`
/// display line are regenerated (from the plan's assigned heads, exact
/// per-head mode/position/rotation, and virtual framebuffer size); every
/// other template line (server layout, monitor sync ranges, extensions,
/// input devices) is preserved verbatim, matching the single-head renderer's
/// `AutoAddDevices`/`AutoEnableDevices` recovery-mode flip.
///
/// # Errors
///
/// Returns an error when the plan has no monitors, or the template does not
/// contain exactly one of each of the three regenerated directive lines
/// (avoiding a silent partial/ambiguous rewrite).
pub fn render_multi_head_xorg_config(
    template: &str,
    plan: &LinuxTopologyPlan,
) -> Result<String, XorgMultiHeadConfigError> {
    if plan.monitors.is_empty() {
        return Err(XorgMultiHeadConfigError::EmptyPlan);
    }

    let connected_monitor_value = plan
        .monitors
        .iter()
        .map(|monitor| monitor.head.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let metamodes_value = plan
        .monitors
        .iter()
        .map(metamode_clause)
        .collect::<Vec<_>>()
        .join(", ");

    let mut connected_monitor_hits = 0usize;
    let mut metamodes_hits = 0usize;
    let mut virtual_hits = 0usize;
    let mut rendered_lines = Vec::with_capacity(template.lines().count());
    for line in template.lines() {
        let trimmed = line.trim_start();
        let indent = &line[..line.len() - trimmed.len()];
        if trimmed.starts_with("Option") && trimmed.contains("\"ConnectedMonitor\"") {
            connected_monitor_hits += 1;
            rendered_lines.push(format!(
                "{indent}Option         \"ConnectedMonitor\" \"{connected_monitor_value}\""
            ));
        } else if trimmed.starts_with("Option") && trimmed.contains("\"MetaModes\"") {
            metamodes_hits += 1;
            rendered_lines.push(format!(
                "{indent}Option         \"MetaModes\" \"{metamodes_value}\""
            ));
        } else if trimmed.starts_with("Virtual") {
            virtual_hits += 1;
            rendered_lines.push(format!(
                "{indent}Virtual    {} {}",
                plan.virtual_width, plan.virtual_height
            ));
        } else {
            rendered_lines.push(line.to_owned());
        }
    }

    if connected_monitor_hits != 1 {
        return Err(XorgMultiHeadConfigError::ConnectedMonitorLineCount(
            connected_monitor_hits,
        ));
    }
    if metamodes_hits != 1 {
        return Err(XorgMultiHeadConfigError::MetaModesLineCount(metamodes_hits));
    }
    if virtual_hits != 1 {
        return Err(XorgMultiHeadConfigError::VirtualLineCount(virtual_hits));
    }

    let mut rendered = rendered_lines.join("\n");
    if template.ends_with('\n') {
        rendered.push('\n');
    }
    Ok(rendered
        .replace(
            "\"AutoAddDevices\" \"false\"",
            "\"AutoAddDevices\" \"true\"",
        )
        .replace(
            "\"AutoEnableDevices\" \"false\"",
            "\"AutoEnableDevices\" \"true\"",
        ))
}

fn metamode_clause(monitor: &LinuxMonitorPlan) -> String {
    debug_assert!(
        monitor.x >= 0 && monitor.y >= 0,
        "LinuxTopologyPlan guarantees a non-negative applied origin"
    );
    // The requested stream dimensions are arbitrary client presentation
    // rasters, not necessarily named modes in the head's EDID. NVIDIA rejects
    // those raw `WIDTHxHEIGHT` tokens during X screen initialization. Keep the
    // connector on its validated `nvidia-auto-select` mode and use ViewPortIn
    // to expose the exact client raster in the combined X desktop; this is the
    // same mechanism the proven single-head MetaMode retarget path uses.
    //
    // `ViewPortIn` is the head's extent *in the X screen*, so under a
    // `Rotation=` clause it is stated in post-rotation screen space — the
    // plan's own applied footprint (`width`/`height`), not the native
    // pre-rotation raster (`mode_token`/`physical_size`). Driving a rotated
    // head from its pre-rotation raster makes NVIDIA compute a wider bounding
    // layout than the plan's `Virtual` size, which it then silently clamps and
    // re-centers: on the pier-linux.example.internal lab GPU a `+2560+0` portrait head landed at
    // `+1440+560` at the wrong extent. `session::randr_verify` fails that
    // session closed, so the practical effect was that any rotated monitor
    // could never verify; stating the footprint is what makes it apply
    // exactly.
    let position = format!(
        "{}: nvidia-auto-select +{}+{} {{ViewPortIn={}x{}}}",
        monitor.head, monitor.x, monitor.y, monitor.width, monitor.height
    );
    match rotation_token(monitor.rotation) {
        Some(token) => {
            let position = position
                .strip_suffix('}')
                .expect("viewport MetaMode clause ends with a property block");
            format!("{position}, Rotation={token}}}")
        }
        None => position,
    }
}

/// Maps this tranche's [`Rotation`] to the NVIDIA MetaModes rotation token.
///
/// Arcen's [`Rotation`] is documented as clockwise degrees. NVIDIA's own
/// MetaModes `Rotation` vocabulary (see NVIDIA's X driver README,
/// "Configuring TwinView") accepts both numeric degrees and directional
/// synonyms, and is explicit that its directional words are *not* clockwise
/// degrees: `"90"`/`"left"`/`"CCW"` are the same 90-degree-counter-clockwise
/// orientation, and `"270"`/`"right"`/`"CW"` are the same 270-degree-clockwise
/// (90-degree-counter-clockwise-from-`"90"`) orientation. So
/// [`Rotation::Degrees90`] (90 degrees *clockwise*) must render NVIDIA's
/// `right`/`CW` token, and [`Rotation::Degrees270`] (270 degrees clockwise,
/// i.e. 90 degrees counter-clockwise) must render NVIDIA's `left`/`CCW`
/// token — the reverse of what the directional word alone might suggest.
/// [`Rotation::Degrees180`] renders `inverted`. [`Rotation::Degrees0`] omits
/// the clause entirely, which NVIDIA treats as the implicit "normal"
/// orientation.
fn rotation_token(rotation: Rotation) -> Option<&'static str> {
    match rotation {
        Rotation::Degrees0 => None,
        Rotation::Degrees90 => Some("right"),
        Rotation::Degrees180 => Some("inverted"),
        Rotation::Degrees270 => Some("left"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::topology::{plan_topology, HeadInventory, VALID_HEAD_TOKENS};
    use arcen_media::{
        Monitor, MonitorIdentity, RequestedMonitor, RequestedMonitorTopology, TopologyGeneration,
    };

    const TEMPLATE: &str = include_str!("../../../../packaging/linux/arcen-xorg.conf");

    fn requested_monitor(
        id: &str,
        x: i32,
        y: i32,
        width_px: u32,
        height_px: u32,
        primary: bool,
        rotation: Rotation,
    ) -> RequestedMonitor {
        let monitor = Monitor {
            identity: MonitorIdentity {
                id: id.to_owned(),
                name: format!("Display {id}"),
                ..MonitorIdentity::default()
            },
            x,
            y,
            width_px,
            height_px,
            scale: 1.0,
            refresh_hz: 60,
            rotation,
            primary,
            width_mm: 0.0,
            height_mm: 0.0,
        };
        RequestedMonitor::new(monitor, width_px, height_px).expect("requested monitor")
    }

    fn plan_for(monitors: Vec<RequestedMonitor>, heads: usize) -> LinuxTopologyPlan {
        let requested = RequestedMonitorTopology::new(monitors).expect("requested topology");
        let generation = TopologyGeneration::new(1).expect("generation");
        let inventory =
            HeadInventory::uniform(VALID_HEAD_TOKENS.iter().take(heads).copied()).expect("heads");
        plan_topology(&requested, generation, &inventory).expect("plan")
    }

    #[test]
    fn one_head_config_retargets_connected_monitor_metamodes_and_virtual() {
        let plan = plan_for(
            vec![requested_monitor(
                "primary",
                0,
                0,
                1920,
                1080,
                true,
                Rotation::Degrees0,
            )],
            1,
        );
        let rendered = render_multi_head_xorg_config(TEMPLATE, &plan).expect("rendered");
        assert!(rendered.contains("Option         \"ConnectedMonitor\" \"DFP-0\""));
        assert!(rendered.contains(
            "Option         \"MetaModes\" \"DFP-0: nvidia-auto-select +0+0 {ViewPortIn=1920x1080}\""
        ));
        assert!(rendered.contains("Virtual    1920 1080"));
        assert!(rendered.contains("\"AutoAddDevices\" \"true\""));
        assert!(rendered.contains("\"AutoEnableDevices\" \"true\""));
        // Every other structural section survives untouched.
        assert!(rendered.contains("Section \"ServerLayout\""));
        assert!(rendered.contains("Option         \"PreferredMode\" \"1920x1200\""));
    }

    #[test]
    fn two_head_config_joins_connected_monitor_and_metamodes_clauses() {
        let plan = plan_for(
            vec![
                requested_monitor("primary", 0, 0, 1920, 1080, true, Rotation::Degrees0),
                requested_monitor("second", 1920, 0, 1280, 720, false, Rotation::Degrees0),
            ],
            2,
        );
        let rendered = render_multi_head_xorg_config(TEMPLATE, &plan).expect("rendered");
        assert!(rendered.contains("Option         \"ConnectedMonitor\" \"DFP-0, DFP-1\""));
        assert!(rendered.contains(
            "Option         \"MetaModes\" \"DFP-0: nvidia-auto-select +0+0 \
             {ViewPortIn=1920x1080}, DFP-1: nvidia-auto-select +1920+0 \
             {ViewPortIn=1280x720}\""
        ));
        assert!(rendered.contains("Virtual    3200 1080"));
    }

    #[test]
    fn four_head_config_emits_all_four_metamodes_clauses_with_rotation() {
        let plan = plan_for(
            vec![
                requested_monitor("a", 0, 0, 1920, 1080, true, Rotation::Degrees0),
                requested_monitor("b", 1920, 0, 1920, 1080, false, Rotation::Degrees90),
                requested_monitor("c", 3840, 0, 1920, 1080, false, Rotation::Degrees180),
                requested_monitor("d", 5760, 0, 1920, 1080, false, Rotation::Degrees270),
            ],
            4,
        );
        let rendered = render_multi_head_xorg_config(TEMPLATE, &plan).expect("rendered");
        assert!(
            rendered.contains("Option         \"ConnectedMonitor\" \"DFP-0, DFP-1, DFP-2, DFP-3\"")
        );
        // "b", "c", and "d" are each requested touching the *previous*
        // monitor's logical right edge (1920 logical units apart), but "b"
        // and "d" are rotated 90/270 degrees, so their true desktop footprint
        // is portrait (1080 wide x 1920 tall) even though their native
        // MetaMode token stays the landscape 1920x1080 mode. The edge-aware
        // placement (`display::topology::plan_monitor_offsets`) walks this
        // touching-edge chain using each anchor's own rotation-aware
        // footprint, so "c" lands flush against "b"'s true (portrait) right
        // edge at 1920 + 1080 = 3000, and "d" lands flush against "c"'s true
        // (landscape) right edge at 3000 + 1920 = 4920 -- not at the naive
        // 3840/5760 a rotation-blind conversion would produce, which would
        // leave a hidden 840px gap behind "b".
        //
        // Each rotated head's `ViewPortIn` states that same portrait
        // footprint, because `ViewPortIn` is the head's extent *in the X
        // screen* (post-rotation), not its native raster. Stating the
        // landscape raster instead made NVIDIA compute a wider bounding
        // layout than `Virtual`, silently clamp it, and re-center every head.
        assert!(rendered.contains("DFP-0: nvidia-auto-select +0+0 {ViewPortIn=1920x1080}"));
        assert!(rendered
            .contains("DFP-1: nvidia-auto-select +1920+0 {ViewPortIn=1080x1920, Rotation=right}"));
        assert!(rendered.contains(
            "DFP-2: nvidia-auto-select +3000+0 {ViewPortIn=1920x1080, Rotation=inverted}"
        ));
        assert!(rendered
            .contains("DFP-3: nvidia-auto-select +4920+0 {ViewPortIn=1080x1920, Rotation=left}"));
        // The overall virtual framebuffer is sized from that same
        // rotation-aware footprint chain, not the unrotated native
        // dimensions, and (since every head now lands flush against its
        // predecessor's true footprint) is exactly gap-free: 1920 (a) + 1080
        // (b, portrait) + 1920 (c) + 1080 (d, portrait) = 6000 wide.
        assert!(rendered.contains("Virtual    6000 1920"));
    }

    /// The exact three-display Mac layout the pier-linux.example.internal lab drove headlessly
    /// on the allowed GPU: built-in Retina primary, a landscape DELL, and a
    /// portrait DELL. A real dedicated Xorg started from this rendered
    /// configuration reported
    /// `DVI-D-0 3024x1964+0+0`, `DVI-D-1 2560x1440+3024+0`, and
    /// `DVI-D-2 1440x2560+5584+0` on a `7024x2560` screen — three independent
    /// RandR outputs from a GPU with zero physical display devices.
    #[test]
    fn three_head_config_provisions_the_exact_mac_three_display_layout() {
        let plan = plan_for(
            vec![
                requested_monitor("builtin", 0, 0, 3024, 1964, true, Rotation::Degrees0),
                requested_monitor(
                    "dell-landscape",
                    3024,
                    0,
                    2560,
                    1440,
                    false,
                    Rotation::Degrees0,
                ),
                requested_monitor(
                    "dell-portrait",
                    5584,
                    0,
                    1440,
                    2560,
                    false,
                    Rotation::Degrees0,
                ),
            ],
            3,
        );
        let rendered = render_multi_head_xorg_config(TEMPLATE, &plan).expect("rendered");
        assert!(rendered.contains("Option         \"ConnectedMonitor\" \"DFP-0, DFP-1, DFP-2\""));
        assert!(rendered.contains(
            "Option         \"MetaModes\" \"DFP-0: nvidia-auto-select +0+0 \
             {ViewPortIn=3024x1964}, DFP-1: nvidia-auto-select +3024+0 \
             {ViewPortIn=2560x1440}, DFP-2: nvidia-auto-select +5584+0 \
             {ViewPortIn=1440x2560}\""
        ));
        assert!(rendered.contains("Virtual    7024 2560"));
    }

    /// Regression for the live mixed-Retina/portrait layout that previously
    /// failed verification. A rotated head's `ViewPortIn` must use its
    /// post-rotation footprint, or NVIDIA expands the screen beyond the
    /// committed 6632x2820 bounds.
    #[test]
    fn live_three_head_layout_uses_the_rotated_viewport_footprint() {
        let mut plan = plan_for(
            vec![
                requested_monitor("builtin", 5120, 1870, 1512, 950, true, Rotation::Degrees0),
                requested_monitor(
                    "dell-portrait",
                    0,
                    0,
                    2560,
                    1440,
                    false,
                    Rotation::Degrees270,
                ),
                requested_monitor(
                    "dell-landscape",
                    2560,
                    1120,
                    2560,
                    1440,
                    false,
                    Rotation::Degrees0,
                ),
            ],
            3,
        );
        plan.virtual_width = 6632;
        plan.virtual_height = 2820;
        for (monitor, (x, y)) in plan
            .monitors
            .iter_mut()
            .zip([(5120, 1870), (0, 0), (2560, 1120)])
        {
            monitor.x = x;
            monitor.y = y;
        }
        let rendered = render_multi_head_xorg_config(TEMPLATE, &plan).expect("rendered");
        assert!(rendered.contains(
            "Option         \"MetaModes\" \"DFP-0: nvidia-auto-select +5120+1870 \
             {ViewPortIn=1512x950}, DFP-1: nvidia-auto-select +0+0 \
             {ViewPortIn=1440x2560, Rotation=left}, DFP-2: nvidia-auto-select +2560+1120 \
             {ViewPortIn=2560x1440}\""
        ));
        assert!(rendered.contains("Virtual    6632 2820"));
    }

    /// A head count is provisioned from the operator's allowlist ceiling, not
    /// from a fixed roster: the same renderer drives 1, 2, 3, and 4 heads off
    /// the full `DFP-0`..`DFP-3` capacity, always taking the first N.
    #[test]
    fn every_head_count_from_one_to_four_provisions_the_first_n_capacity_heads() {
        for count in 1..=VALID_HEAD_TOKENS.len() {
            let monitors = (0..count)
                .map(|index| {
                    let x = i32::try_from(index).expect("small index") * 1920;
                    requested_monitor(
                        &format!("display-{index}"),
                        x,
                        0,
                        1920,
                        1080,
                        index == 0,
                        Rotation::Degrees0,
                    )
                })
                .collect();
            let plan = plan_topology(
                &RequestedMonitorTopology::new(monitors).expect("requested topology"),
                TopologyGeneration::new(1).expect("generation"),
                &HeadInventory::uniform(VALID_HEAD_TOKENS.iter().copied())
                    .expect("four-head inventory"),
            )
            .expect("plan");
            let rendered = render_multi_head_xorg_config(TEMPLATE, &plan).expect("rendered");
            let expected_heads = VALID_HEAD_TOKENS[..count].join(", ");
            assert!(
                rendered.contains(&format!(
                    "Option         \"ConnectedMonitor\" \"{expected_heads}\""
                )),
                "{count}-head config must connect exactly {expected_heads}"
            );
            for (index, head) in VALID_HEAD_TOKENS[..count].iter().enumerate() {
                let x = index * 1920;
                assert!(
                    rendered.contains(&format!(
                        "{head}: nvidia-auto-select +{x}+0 {{ViewPortIn=1920x1080}}"
                    )),
                    "{count}-head config must place {head} at +{x}+0"
                );
            }
            assert!(
                !rendered.contains(VALID_HEAD_TOKENS[count - 1..].get(1).unwrap_or(&"DFP-4")),
                "{count}-head config must not reference an unprovisioned head"
            );
            assert!(rendered.contains(&format!("Virtual    {} 1080", count * 1920)));
        }
    }

    #[test]
    fn rejects_an_empty_plan() {
        let plan = LinuxTopologyPlan {
            generation: TopologyGeneration::new(1).expect("generation"),
            virtual_width: 0,
            virtual_height: 0,
            monitors: Vec::new(),
        };
        assert_eq!(
            render_multi_head_xorg_config(TEMPLATE, &plan),
            Err(XorgMultiHeadConfigError::EmptyPlan)
        );
    }

    #[test]
    fn rejects_a_template_missing_the_connected_monitor_line() {
        let plan = plan_for(
            vec![requested_monitor(
                "primary",
                0,
                0,
                1920,
                1080,
                true,
                Rotation::Degrees0,
            )],
            1,
        );
        let broken_template = TEMPLATE.replace("ConnectedMonitor", "NotAKnownOption");
        assert_eq!(
            render_multi_head_xorg_config(&broken_template, &plan),
            Err(XorgMultiHeadConfigError::ConnectedMonitorLineCount(0))
        );
    }

    #[test]
    fn rejects_a_template_missing_the_metamodes_line() {
        let plan = plan_for(
            vec![requested_monitor(
                "primary",
                0,
                0,
                1920,
                1080,
                true,
                Rotation::Degrees0,
            )],
            1,
        );
        let broken_template = TEMPLATE.replace("MetaModes", "NotAKnownOption");
        assert_eq!(
            render_multi_head_xorg_config(&broken_template, &plan),
            Err(XorgMultiHeadConfigError::MetaModesLineCount(0))
        );
    }

    #[test]
    fn rejects_a_template_missing_the_virtual_line() {
        let plan = plan_for(
            vec![requested_monitor(
                "primary",
                0,
                0,
                1920,
                1080,
                true,
                Rotation::Degrees0,
            )],
            1,
        );
        let broken_template = TEMPLATE.replace("Virtual", "NotAKnownDirective");
        assert_eq!(
            render_multi_head_xorg_config(&broken_template, &plan),
            Err(XorgMultiHeadConfigError::VirtualLineCount(0))
        );
    }

    #[test]
    fn rejects_a_template_with_duplicated_directive_lines() {
        let plan = plan_for(
            vec![requested_monitor(
                "primary",
                0,
                0,
                1920,
                1080,
                true,
                Rotation::Degrees0,
            )],
            1,
        );
        let mut duplicated = TEMPLATE.to_owned();
        duplicated
            .push_str("\n    Option         \"MetaModes\" \"DFP-0: nvidia-auto-select +0+0\"\n");
        assert_eq!(
            render_multi_head_xorg_config(&duplicated, &plan),
            Err(XorgMultiHeadConfigError::MetaModesLineCount(2))
        );
    }

    #[test]
    fn rotation_token_matches_nvidias_documented_ccw_cw_vocabulary_exactly() {
        // Golden values straight from NVIDIA's X driver README ("Configuring
        // TwinView"): "90"/"left"/"CCW" name the same orientation, and
        // "270"/"right"/"CW" name the same orientation. Arcen's `Rotation` is
        // clockwise degrees, so `Degrees90` (90 CW) must render NVIDIA's
        // `right`/CW synonym and `Degrees270` (270 CW, i.e. 90 CCW) must
        // render NVIDIA's `left`/CCW synonym.
        assert_eq!(rotation_token(Rotation::Degrees0), None);
        assert_eq!(rotation_token(Rotation::Degrees90), Some("right"));
        assert_eq!(rotation_token(Rotation::Degrees180), Some("inverted"));
        assert_eq!(rotation_token(Rotation::Degrees270), Some("left"));
    }
}
