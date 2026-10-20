//! GPU identity for the compositor and the encoder, as shared data.
//!
//! The encoder GPU is the CUDA device capenc opened (the Pier names it by PCI
//! address). The compositor GPU comes from the session adapter's hand-off.
//! Both are completed from sysfs where possible so that a PCI address and a
//! render node can be compared. Nothing is guessed: a missing fact stays
//! `None`, and [`arcen_media::video::GpuIdentity::compare`] treats an
//! identity with no common key as unknown.

use std::path::Path;

use arcen_media::video::{GpuIdentity, GpuMatch, PciAddress};

use super::handoff::{parse_pci_address, render_node_minor_from_name};

/// Formats a PCI address as sysfs names it (`0000:00:10.0`).
#[must_use]
pub fn sysfs_pci_name(pci: PciAddress) -> String {
    arcen_media::video::capture_handoff::format_pci_address(pci)
}

pub use arcen_media::video::capture_handoff::parse_pci_vendor as parse_vendor;

/// The render-node minor among a device's `drm/` directory entries.
pub fn render_minor_from_entries<'a>(entries: impl IntoIterator<Item = &'a str>) -> Option<u32> {
    let mut minors = entries.into_iter().filter_map(render_node_minor_from_name);
    let first = minors.next()?;
    // Two render nodes for one PCI function would be ambiguous.
    minors.next().is_none().then_some(first)
}

/// Completes an identity from sysfs below `sys_root` (normally `/sys`).
#[must_use]
pub fn identity_for_pci(sys_root: &Path, pci: PciAddress) -> GpuIdentity {
    let device = sys_root.join("bus/pci/devices").join(sysfs_pci_name(pci));
    let vendor = std::fs::read_to_string(device.join("vendor"))
        .ok()
        .as_deref()
        .and_then(parse_vendor);
    let render = std::fs::read_dir(device.join("drm")).ok().and_then(|dir| {
        let names: Vec<String> = dir
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        render_minor_from_entries(names.iter().map(String::as_str))
    });
    GpuIdentity {
        pci: Some(pci),
        render_node_minor: render,
        vendor_id: vendor,
    }
}

/// Completes an identity for a render node (`/dev/dri/renderD128`) from its
/// sysfs class entry below `sys_root`.
#[must_use]
pub fn identity_for_render_node(sys_root: &Path, node: &Path) -> GpuIdentity {
    let Some(name) = node.file_name().and_then(|name| name.to_str()) else {
        return GpuIdentity::default();
    };
    let minor = render_node_minor_from_name(name);
    let device = sys_root.join("class/drm").join(name).join("device");
    let pci = std::fs::canonicalize(&device)
        .ok()
        .and_then(|path| path.file_name()?.to_str().and_then(parse_pci_address));
    let vendor = std::fs::read_to_string(device.join("vendor"))
        .ok()
        .as_deref()
        .and_then(parse_vendor);
    GpuIdentity {
        pci,
        render_node_minor: minor,
        vendor_id: vendor,
    }
}

/// Merges two partial identities known to describe one device (they compare
/// [`GpuMatch::Same`] or share no key). Callers resolve conflicts first with
/// [`compositor_identity`]; this never decides between contradictory facts.
#[must_use]
pub fn merge(primary: GpuIdentity, secondary: GpuIdentity) -> GpuIdentity {
    GpuIdentity {
        pci: primary.pci.or(secondary.pci),
        render_node_minor: primary.render_node_minor.or(secondary.render_node_minor),
        vendor_id: primary.vendor_id.or(secondary.vendor_id),
    }
}

/// The compositor GPU from the hand-off's PCI address and render node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositorGpu {
    /// Neither fact was supplied.
    Unknown,
    /// A consistent identity.
    Known(GpuIdentity),
    /// The PCI address and the render node name different devices. No
    /// identity is derived, so no same-GPU route can be proven.
    Contradictory {
        /// Identity completed from the PCI address.
        from_pci: GpuIdentity,
        /// Identity completed from the render node.
        from_node: GpuIdentity,
    },
}

impl CompositorGpu {
    /// The identity to compare with the encoder; `None` unless consistent.
    #[must_use]
    pub const fn identity(self) -> Option<GpuIdentity> {
        match self {
            Self::Known(identity) => Some(identity),
            Self::Unknown | Self::Contradictory { .. } => None,
        }
    }
}

/// Combines the two compositor facts, refusing contradictory ones.
#[must_use]
pub fn compositor_identity(
    from_pci: Option<GpuIdentity>,
    from_node: Option<GpuIdentity>,
) -> CompositorGpu {
    match (from_pci, from_node) {
        (None, None) => CompositorGpu::Unknown,
        (Some(identity), None) | (None, Some(identity)) => CompositorGpu::Known(identity),
        (Some(from_pci), Some(from_node)) => match from_pci.compare(&from_node) {
            GpuMatch::Different => CompositorGpu::Contradictory {
                from_pci,
                from_node,
            },
            GpuMatch::Same | GpuMatch::Unknown => CompositorGpu::Known(merge(from_pci, from_node)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sysfs_names_round_trip() {
        let pci = PciAddress::new(0, 0, 0x10, 0).unwrap();
        assert_eq!(sysfs_pci_name(pci), "0000:00:10.0");
        assert_eq!(parse_pci_address(&sysfs_pci_name(pci)), Some(pci));
    }

    #[test]
    fn vendor_and_render_entries_parse_strictly() {
        assert_eq!(parse_vendor("0x10de\n"), Some(0x10de));
        assert_eq!(parse_vendor("10de"), None);
        assert_eq!(
            render_minor_from_entries(["card0", "renderD128", "controlD64"]),
            Some(128)
        );
        assert_eq!(render_minor_from_entries(["card0"]), None);
        assert_eq!(
            render_minor_from_entries(["renderD128", "renderD129"]),
            None
        );
    }

    #[test]
    fn missing_sysfs_leaves_only_the_named_key() {
        let pci = PciAddress::new(0, 0x41, 0, 0).unwrap();
        let identity = identity_for_pci(Path::new("/nonexistent-arcen-sysfs"), pci);
        assert_eq!(
            identity,
            GpuIdentity {
                pci: Some(pci),
                render_node_minor: None,
                vendor_id: None,
            }
        );
        let node = identity_for_render_node(
            Path::new("/nonexistent-arcen-sysfs"),
            Path::new("/dev/dri/renderD128"),
        );
        assert_eq!(node.render_node_minor, Some(128));
        assert_eq!(node.pci, None);
        // A PCI-only encoder and a render-node-only compositor share no key.
        assert_eq!(identity.compare(&node), GpuMatch::Unknown);
    }

    #[test]
    fn contradictory_compositor_facts_never_become_the_same_gpu() {
        let a = PciAddress::new(0, 0, 0x10, 0).unwrap();
        let b = PciAddress::new(0, 0x41, 0, 0).unwrap();
        let encoder = GpuIdentity {
            pci: Some(a),
            render_node_minor: Some(128),
            vendor_id: Some(0x10de),
        };
        // PCI names GPU A; the render node belongs to GPU B.
        let from_pci = GpuIdentity {
            pci: Some(a),
            ..GpuIdentity::default()
        };
        let from_node = GpuIdentity {
            pci: Some(b),
            render_node_minor: Some(129),
            vendor_id: Some(0x10de),
        };
        let combined = compositor_identity(Some(from_pci), Some(from_node));
        assert_eq!(
            combined,
            CompositorGpu::Contradictory {
                from_pci,
                from_node
            }
        );
        assert_eq!(combined.identity(), None);
        assert_eq!(
            arcen_media::video::compare_gpus(combined.identity().as_ref(), Some(&encoder)),
            GpuMatch::Unknown
        );
        // A blind merge keeps A's PCI and B's node, which an encoder known
        // only by A's PCI address would accept as the same GPU.
        let encoder_by_pci = GpuIdentity {
            pci: Some(a),
            ..GpuIdentity::default()
        };
        assert_eq!(
            merge(from_pci, from_node).compare(&encoder_by_pci),
            GpuMatch::Same
        );
        assert_eq!(
            arcen_media::video::compare_gpus(combined.identity().as_ref(), Some(&encoder_by_pci)),
            GpuMatch::Unknown
        );
    }

    #[test]
    fn consistent_or_single_compositor_facts_are_kept() {
        let a = PciAddress::new(0, 0, 0x10, 0).unwrap();
        let from_pci = GpuIdentity {
            pci: Some(a),
            vendor_id: Some(0x10de),
            ..GpuIdentity::default()
        };
        let from_node = GpuIdentity {
            pci: Some(a),
            render_node_minor: Some(128),
            vendor_id: Some(0x10de),
        };
        assert_eq!(
            compositor_identity(Some(from_pci), Some(from_node)).identity(),
            Some(from_node)
        );
        assert_eq!(compositor_identity(None, None), CompositorGpu::Unknown);
        assert_eq!(
            compositor_identity(None, Some(from_node)).identity(),
            Some(from_node)
        );
        // No common key: kept as one claim, compared later like any other.
        let node_only = GpuIdentity {
            render_node_minor: Some(128),
            ..GpuIdentity::default()
        };
        let pci_only = GpuIdentity {
            pci: Some(a),
            ..GpuIdentity::default()
        };
        assert_eq!(
            compositor_identity(Some(pci_only), Some(node_only)).identity(),
            Some(GpuIdentity {
                pci: Some(a),
                render_node_minor: Some(128),
                vendor_id: None,
            })
        );
    }

    #[test]
    fn merge_fills_gaps_without_overwriting() {
        let pci = PciAddress::new(0, 0, 0x10, 0).unwrap();
        let named = GpuIdentity {
            pci: Some(pci),
            ..GpuIdentity::default()
        };
        let node = GpuIdentity {
            render_node_minor: Some(128),
            vendor_id: Some(0x10de),
            ..GpuIdentity::default()
        };
        let merged = merge(named, node);
        assert_eq!(merged.pci, Some(pci));
        assert_eq!(merged.render_node_minor, Some(128));
        let encoder = GpuIdentity {
            pci: Some(pci),
            render_node_minor: Some(128),
            vendor_id: Some(0x10de),
        };
        assert_eq!(merged.compare(&encoder), GpuMatch::Same);
    }
}
