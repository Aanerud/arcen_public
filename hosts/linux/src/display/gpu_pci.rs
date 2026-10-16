//! Reads the host's PCI functions from sysfs for the shared Xorg GPU decision.
//!
//! The decision itself — which GPU an NVIDIA `Device` section must name — is
//! `arcen_outputs::xorg_gpu`. This file only lists
//! `<sysfs>/bus/pci/devices/*/{vendor,class,driver}`. It uses nothing but `std`
//! and `arcen_outputs`, because the Linux installer compiles this same file to
//! make the same decision at install time.

use std::fs;
use std::io;
use std::path::Path;

use arcen_outputs::{GpuCandidate, XorgGpuDecision, XorgTemplateError};

/// Where the kernel mounts sysfs.
pub const SYSFS_ROOT: &str = "/sys";

/// Every PCI function under `sysfs_root` that reports a parsable address,
/// vendor and class, in directory order. Entries that cannot be read are
/// skipped: a function that vanished mid-scan cannot be the GPU Xorg needs.
///
/// # Errors
///
/// Only when the PCI device directory itself cannot be listed.
pub fn pci_candidates(sysfs_root: &Path) -> io::Result<Vec<GpuCandidate>> {
    let devices = sysfs_root.join("bus/pci/devices");
    let mut candidates = Vec::new();
    for entry in fs::read_dir(&devices)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let name = entry.file_name();
        let (Some(name), Ok(vendor), Ok(class)) = (
            name.to_str(),
            fs::read_to_string(path.join("vendor")),
            fs::read_to_string(path.join("class")),
        ) else {
            continue;
        };
        let driver = fs::read_link(path.join("driver")).ok();
        let driver = driver
            .as_deref()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str());
        if let Some(candidate) = GpuCandidate::from_sysfs(name, &vendor, &class, driver) {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

/// The usable-or-not NVIDIA display GPUs the host has, in `lspci -D` form,
/// for messages.
pub fn describe_nvidia_gpus(candidates: &[GpuCandidate]) -> String {
    let listed: Vec<String> = candidates
        .iter()
        .filter(|candidate| candidate.is_nvidia_display())
        .map(|candidate| match &candidate.driver {
            arcen_outputs::BoundDriver::Nvidia => format!("{} (nvidia)", candidate.address),
            arcen_outputs::BoundDriver::Unbound => format!("{} (no driver)", candidate.address),
            arcen_outputs::BoundDriver::Other(driver) => {
                format!("{} (bound to {driver}, not usable)", candidate.address)
            }
        })
        .collect();
    if listed.is_empty() {
        "none".to_owned()
    } else {
        listed.join(", ")
    }
}

/// One clause describing a decision about the template at `template`, for a
/// log line or a terminal. `action` is the verb a `Set` is reported with:
/// the installer says "setting", the Pier "using ... for this session".
pub fn describe_decision(template: &str, decision: &XorgGpuDecision, action: &str) -> String {
    match decision {
        XorgGpuDecision::Keep { bus_id, address } => {
            format!("{template} uses the NVIDIA GPU at PCI {address} (BusID {bus_id})")
        }
        XorgGpuDecision::Set {
            previous,
            bus_id,
            address,
            ..
        } => {
            let from = previous.as_deref().map_or_else(
                || "had no BusID".to_owned(),
                |previous| format!("named BusID \"{previous}\""),
            );
            format!(
                "{template} {from}, but the NVIDIA GPU is at {address}; {action} BusID \"{bus_id}\""
            )
        }
        XorgGpuDecision::NoNvidiaGpu { previous } => {
            let named = previous.as_deref().map_or_else(
                || "has no BusID".to_owned(),
                |previous| format!("names BusID \"{previous}\""),
            );
            format!(
                "{template} {named}, and this host has no usable NVIDIA GPU; headless sessions \
                 need an NVIDIA GPU with the nvidia driver loaded"
            )
        }
    }
}

/// When a `Set` chose among several NVIDIA GPUs: which others exist and how
/// to pick one of them instead.
pub fn describe_other_gpus(template: &str, decision: &XorgGpuDecision) -> Option<String> {
    let XorgGpuDecision::Set { others, .. } = decision else {
        return None;
    };
    if others.is_empty() {
        return None;
    }
    let others: Vec<String> = others
        .iter()
        .map(|other| format!("{other} (BusID \"{}\")", other.xorg_bus_id()))
        .collect();
    Some(format!(
        "this host has several NVIDIA GPUs; the others are {}. To use one of them, set its \
         BusID in {template}",
        others.join(", ")
    ))
}

/// Text for a template the shared decision refused.
pub fn describe_template_error(template: &str, error: &XorgTemplateError) -> String {
    format!("{template}: cannot choose a GPU for it: {error}")
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A throwaway sysfs tree, removed when dropped.
    pub(crate) struct FakeSysfs(PathBuf);

    impl FakeSysfs {
        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for FakeSysfs {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
            if let Some(parent) = self.0.parent() {
                let _ = fs::remove_dir(parent);
            }
        }
    }

    /// A sysfs tree under the crate directory: `(address, vendor, class,
    /// driver)` per PCI function.
    pub(crate) fn fake_sysfs(
        name: &str,
        devices: &[(&str, &str, &str, Option<&str>)],
    ) -> FakeSysfs {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(".arcen-gpu-pci-tests")
            .join(format!("{name}-{}-{unique}", std::process::id()));
        let pci = root.join("bus/pci/devices");
        fs::create_dir_all(&pci).expect("create fake sysfs");
        for (address, vendor, class, driver) in devices {
            let device = pci.join(address);
            fs::create_dir_all(&device).expect("create device");
            fs::write(device.join("vendor"), format!("{vendor}\n")).expect("vendor");
            fs::write(device.join("class"), format!("{class}\n")).expect("class");
            if let Some(driver) = driver {
                let target = root.join("bus/pci/drivers").join(driver);
                fs::create_dir_all(&target).expect("driver dir");
                std::os::unix::fs::symlink(&target, device.join("driver")).expect("driver link");
            }
        }
        FakeSysfs(root)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::fake_sysfs;
    use super::*;

    use arcen_outputs::{BoundDriver, PciAddress};

    #[test]
    fn sysfs_devices_become_candidates() {
        let root = fake_sysfs(
            "candidates",
            &[
                ("0000:00:10.0", "0x10de", "0x030000", Some("nvidia")),
                ("0000:00:02.0", "0x1b36", "0x030000", Some("qxl")),
                ("0000:00:1f.0", "0x8086", "0x060100", None),
                ("garbage", "0x10de", "0x030000", None),
            ],
        );
        let mut candidates = pci_candidates(root.path()).expect("list");
        candidates.sort_by_key(|candidate| candidate.address);
        assert_eq!(candidates.len(), 3);
        assert_eq!(
            candidates[1],
            GpuCandidate {
                address: PciAddress::new(0, 0, 16, 0).expect("address"),
                vendor: 0x10de,
                class: 0x03_00_00,
                driver: BoundDriver::Nvidia,
            }
        );
        assert_eq!(candidates[0].driver, BoundDriver::Other("qxl".to_owned()));
        assert_eq!(describe_nvidia_gpus(&candidates), "0000:00:10.0 (nvidia)");
    }

    #[test]
    fn a_missing_sysfs_is_an_error() {
        assert!(pci_candidates(Path::new("/nonexistent-arcen-sysfs")).is_err());
    }

    #[test]
    fn decisions_read_as_one_sentence() {
        let address = PciAddress::new(0, 0, 16, 0).expect("address");
        let other = PciAddress::new(0, 0x3b, 0, 0).expect("address");
        assert_eq!(
            describe_decision(
                "/etc/arcen/xorg.conf",
                &XorgGpuDecision::Set {
                    previous: Some("PCI:6:16:0".to_owned()),
                    bus_id: "PCI:0:16:0".to_owned(),
                    address,
                    others: Vec::new(),
                },
                "setting",
            ),
            "/etc/arcen/xorg.conf named BusID \"PCI:6:16:0\", but the NVIDIA GPU is at 0000:00:10.0; setting BusID \"PCI:0:16:0\""
        );
        let several = XorgGpuDecision::Set {
            previous: None,
            bus_id: "PCI:0:16:0".to_owned(),
            address,
            others: vec![other],
        };
        assert!(
            describe_decision("/etc/arcen/xorg.conf", &several, "setting")
                .starts_with("/etc/arcen/xorg.conf had no BusID, but")
        );
        let others = describe_other_gpus("/etc/arcen/xorg.conf", &several).expect("others");
        assert!(others.contains("the others are 0000:3b:00.0 (BusID \"PCI:59:0:0\")"));
        assert!(others.ends_with("set its BusID in /etc/arcen/xorg.conf"));
        assert_eq!(
            describe_decision(
                "/etc/arcen/xorg.conf",
                &XorgGpuDecision::Keep {
                    bus_id: "PCI:0:16:0".to_owned(),
                    address,
                },
                "setting",
            ),
            "/etc/arcen/xorg.conf uses the NVIDIA GPU at PCI 0000:00:10.0 (BusID PCI:0:16:0)"
        );
    }
}
