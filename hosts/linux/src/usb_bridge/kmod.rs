// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Whether the running kernel can provide the in-tree USB/IP `vhci-hcd`.
//!
//! The facts are read here; what they mean and what to install is shared
//! policy ([`arcen_usb_bridge::missing_vhci_hcd_reason`]). A kernel update
//! leaves the new kernel without the module until its own
//! `kernel-modules-internal` is installed: the Pier says so at start and in
//! `validate-config`, with the exact command.
//!
//! Compiled into the installer too (std and `arcen-usb-bridge` only), so both
//! reach the same answer.

use arcen_usb_bridge::VhciHcdFacts;
use std::path::Path;

/// Where the in-tree controller appears once `vhci-hcd` is loaded.
pub const VHCI_HCD_SYSFS: &str = "/sys/devices/platform/vhci_hcd.0";
const MAX_KERNELS: usize = 64;

/// `uname -r`, empty when unreadable or implausible.
#[must_use]
pub fn running_kernel() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|release| release.trim().to_owned())
        .ok()
        .filter(|release| arcen_usb_bridge::valid_kernel_release(release))
        .unwrap_or_default()
}

/// Reads the facts without changing anything.
#[must_use]
pub fn vhci_hcd_facts() -> VhciHcdFacts {
    let running_kernel = running_kernel();
    let modules = Path::new("/lib/modules");
    let mut module_for_other_kernels = Vec::new();
    if let Ok(entries) = std::fs::read_dir(modules) {
        for entry in entries.flatten().take(MAX_KERNELS) {
            let release = entry.file_name().to_string_lossy().into_owned();
            if release != running_kernel && kernel_has_vhci_hcd(&entry.path()) {
                module_for_other_kernels.push(release);
            }
        }
    }
    module_for_other_kernels.sort();
    VhciHcdFacts {
        module_for_running_kernel: !running_kernel.is_empty()
            && kernel_has_vhci_hcd(&modules.join(&running_kernel)),
        running_kernel,
        loaded: Path::new(VHCI_HCD_SYSFS).is_dir(),
        module_for_other_kernels,
        rhel_family: std::fs::read_to_string("/etc/os-release")
            .is_ok_and(|text| arcen_usb_bridge::os_release_is_rhel_family(&text)),
    }
}

/// Whether the kernel release directory holds `vhci-hcd` as a module file
/// in any of the trees distributions use, or builds it in.
#[must_use]
pub fn kernel_has_vhci_hcd(release_dir: &Path) -> bool {
    ["internal", "extra", "kernel", "updates"]
        .iter()
        .map(|tree| release_dir.join(tree).join("drivers/usb/usbip"))
        .any(|dir| {
            [
                "vhci-hcd.ko",
                "vhci-hcd.ko.xz",
                "vhci-hcd.ko.zst",
                "vhci-hcd.ko.gz",
            ]
            .iter()
            .any(|file| dir.join(file).is_file())
        })
        || std::fs::read_to_string(release_dir.join("modules.builtin"))
            .is_ok_and(|builtin| builtin.lines().any(|line| line.ends_with("/vhci-hcd.ko")))
}

/// The operator line for Pier start and `validate-config`: `None` when
/// `vhci-hcd` is loaded or installed for the running kernel.
#[must_use]
pub fn kernel_mismatch_warning() -> Option<String> {
    let facts = vhci_hcd_facts();
    (!facts.loaded && !facts.module_for_running_kernel)
        .then(|| arcen_usb_bridge::missing_vhci_hcd_reason(&facts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_module_in_the_internal_tree_or_builtin_list_counts() {
        let root = std::env::temp_dir().join(format!("arcen-kmod-test-{}", std::process::id()));
        let module = root.join("module");
        let builtin = root.join("builtin");
        std::fs::create_dir_all(&builtin).expect("test tree");
        assert!(!kernel_has_vhci_hcd(&module));
        let usbip = module.join("internal/drivers/usb/usbip");
        std::fs::create_dir_all(&usbip).expect("module tree");
        std::fs::write(usbip.join("vhci-hcd.ko.xz"), b"").expect("module file");
        assert!(kernel_has_vhci_hcd(&module));
        std::fs::write(
            builtin.join("modules.builtin"),
            "kernel/drivers/usb/usbip/vhci-hcd.ko\n",
        )
        .expect("builtin list");
        assert!(kernel_has_vhci_hcd(&builtin));
        let _ = std::fs::remove_dir_all(&root);
    }
}
