// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Which distribution package provides the in-tree USB/IP `vhci-hcd` for a
//! running kernel, and what to tell an administrator when it is missing.
//!
//! Rocky Linux and RHEL build `vhci-hcd` (`CONFIG_USBIP_VHCI_HCD=m`) but ship
//! it only in `kernel-modules-internal`, one package per kernel release, in
//! the distribution's `devel` repository (Red Hat labels it internal and
//! unsupported). Rocky moves a point release's packages to its vault when the
//! next point release appears. A kernel update therefore leaves the new kernel
//! without the module until the matching package is installed.
//!
//! Pure: the Pier and its installer gather the facts and act on the plan.

/// What a host knows about `vhci-hcd` for its running kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VhciHcdFacts {
    /// `uname -r`.
    pub running_kernel: String,
    /// Whether the controller is loaded (`/sys/devices/platform/vhci_hcd.0`).
    pub loaded: bool,
    /// Whether the running kernel has a `vhci-hcd` module file (or builds it in).
    pub module_for_running_kernel: bool,
    /// Other installed kernel releases that do have one.
    pub module_for_other_kernels: Vec<String>,
    /// Whether this is a Rocky/RHEL-family system.
    pub rhel_family: bool,
}

/// Whether `/etc/os-release` names a Rocky/RHEL-family distribution.
#[must_use]
pub fn os_release_is_rhel_family(text: &str) -> bool {
    text.lines().any(|line| {
        let Some((key, value)) = line.split_once('=') else {
            return false;
        };
        let value = value.trim().trim_matches('"').to_ascii_lowercase();
        (key == "ID" && matches!(value.as_str(), "rhel" | "rocky" | "almalinux" | "centos"))
            || (key == "ID_LIKE" && value.split_whitespace().any(|id| id == "rhel"))
    })
}

/// Whether `kernel` is a plausible kernel release string (safe to place in
/// a command line or a URL).
#[must_use]
pub fn valid_kernel_release(kernel: &str) -> bool {
    !kernel.is_empty()
        && kernel.len() <= 128
        && kernel
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_digit())
        && kernel
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-+".contains(&byte))
}

/// The package that carries `vhci-hcd` for `kernel` on Rocky/RHEL.
#[must_use]
pub fn kernel_modules_internal_package(kernel: &str) -> String {
    format!("kernel-modules-internal-{kernel}")
}

/// The exact install command for `kernel` (with the `devel` repository, which
/// is where Rocky Linux and Alma Linux publish the package).
#[must_use]
pub fn kernel_modules_internal_command(kernel: &str) -> String {
    format!(
        "dnf --enablerepo=devel install {}",
        kernel_modules_internal_package(kernel)
    )
}

/// The EL point release a Rocky kernel was built for (`5.14.0-611.55.1.el9_7`
/// → `9.7`, `6.12.0-211.66.1.el10_2` → `10.2`).
#[must_use]
pub fn el_point_release(kernel: &str) -> Option<String> {
    kernel.split('.').find_map(|part| {
        let rest = part.strip_prefix("el")?;
        let (major, minor) = rest.split_once('_')?;
        let minor: String = minor.chars().take_while(char::is_ascii_digit).collect();
        (!major.is_empty() && major.bytes().all(|byte| byte.is_ascii_digit()) && !minor.is_empty())
            .then(|| format!("{major}.{minor}"))
    })
}

/// The kernel's architecture suffix (`x86_64`, `aarch64`, …).
#[must_use]
pub fn kernel_arch(kernel: &str) -> Option<&str> {
    let arch = kernel.rsplit('.').next()?;
    matches!(arch, "x86_64" | "aarch64" | "ppc64le" | "s390x").then_some(arch)
}

/// Where Rocky Linux publishes the package for `kernel`: the current point
/// release's `devel` tree first, then the vault for an older point release.
/// Empty when the kernel string has no EL point release or architecture.
#[must_use]
pub fn rocky_package_urls(kernel: &str) -> Vec<String> {
    let (Some(release), Some(arch)) = (el_point_release(kernel), kernel_arch(kernel)) else {
        return Vec::new();
    };
    if !valid_kernel_release(kernel) {
        return Vec::new();
    }
    let file = format!("{}.rpm", kernel_modules_internal_package(kernel));
    ["pub", "vault"]
        .iter()
        .map(|tree| {
            format!(
                "https://dl.rockylinux.org/{tree}/rocky/{release}/devel/{arch}/os/Packages/k/{file}"
            )
        })
        .collect()
}

/// Why `vhci-hcd` is not usable, naming what to install.
#[must_use]
pub fn missing_vhci_hcd_reason(facts: &VhciHcdFacts) -> String {
    let kernel = &facts.running_kernel;
    if facts.module_for_running_kernel {
        return format!(
            "vhci-hcd is installed for kernel {kernel} but did not load (modprobe vhci-hcd failed)"
        );
    }
    if !facts.rhel_family {
        return format!(
            "vhci-hcd (USB/IP, CONFIG_USBIP_VHCI_HCD) is not available for kernel {kernel}: \
             install the distribution package that ships it (on Rocky Linux/RHEL: \
             kernel-modules-internal)"
        );
    }
    if facts.module_for_other_kernels.is_empty() {
        format!(
            "vhci-hcd is not installed: install kernel-modules-internal for the running kernel \
             ({})",
            kernel_modules_internal_command(kernel)
        )
    } else {
        format!(
            "vhci-hcd is installed only for kernel {} but kernel {kernel} is running: install \
             the matching kernel-modules-internal ({})",
            facts.module_for_other_kernels.join(", "),
            kernel_modules_internal_command(kernel)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(rhel: bool, running: bool, others: &[&str]) -> VhciHcdFacts {
        VhciHcdFacts {
            running_kernel: "6.12.0-211.72.1.el10_2.x86_64".to_owned(),
            loaded: false,
            module_for_running_kernel: running,
            module_for_other_kernels: others.iter().map(|value| (*value).to_owned()).collect(),
            rhel_family: rhel,
        }
    }

    #[test]
    fn a_kernel_update_names_the_exact_package_command() {
        let reason =
            missing_vhci_hcd_reason(&facts(true, false, &["6.12.0-211.66.1.el10_2.x86_64"]));
        assert!(reason.contains("installed only for kernel 6.12.0-211.66.1.el10_2.x86_64"));
        assert!(reason.contains(
            "dnf --enablerepo=devel install kernel-modules-internal-6.12.0-211.72.1.el10_2.x86_64"
        ));
    }

    #[test]
    fn a_missing_package_names_kernel_modules_internal_and_other_distributions_get_no_dnf() {
        let reason = missing_vhci_hcd_reason(&facts(true, false, &[]));
        assert!(reason.contains("not installed"));
        assert!(reason.contains("kernel-modules-internal-6.12.0-211.72.1.el10_2.x86_64"));
        let other = missing_vhci_hcd_reason(&facts(false, false, &[]));
        assert!(other.contains("CONFIG_USBIP_VHCI_HCD"));
        assert!(other.contains("kernel-modules-internal"));
        assert!(!other.contains("dnf"));
        assert!(missing_vhci_hcd_reason(&facts(true, true, &[])).contains("did not load"));
    }

    #[test]
    fn rhel_family_is_detected_from_id_or_id_like() {
        assert!(os_release_is_rhel_family(
            "NAME=\"Rocky Linux\"\nID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\n"
        ));
        assert!(os_release_is_rhel_family("ID=almalinux\n"));
        assert!(!os_release_is_rhel_family("ID=ubuntu\nID_LIKE=debian\n"));
        assert!(!os_release_is_rhel_family("ID=fedora\n"));
    }

    #[test]
    fn rocky_urls_follow_the_kernels_point_release_and_arch() {
        assert_eq!(
            rocky_package_urls("5.14.0-611.55.1.el9_7.x86_64"),
            vec![
                "https://dl.rockylinux.org/pub/rocky/9.7/devel/x86_64/os/Packages/k/\
                 kernel-modules-internal-5.14.0-611.55.1.el9_7.x86_64.rpm",
                "https://dl.rockylinux.org/vault/rocky/9.7/devel/x86_64/os/Packages/k/\
                 kernel-modules-internal-5.14.0-611.55.1.el9_7.x86_64.rpm",
            ]
        );
        assert_eq!(
            el_point_release("6.12.0-211.66.1.el10_2.x86_64").as_deref(),
            Some("10.2")
        );
        assert_eq!(el_point_release("6.12.0-55.el10.x86_64"), None);
        assert!(rocky_package_urls("6.8.0-45-generic").is_empty());
        assert!(rocky_package_urls("6.12.0-1.el10_2.x86_64;rm").is_empty());
    }

    #[test]
    fn kernel_releases_with_shell_or_url_metacharacters_are_refused() {
        assert!(valid_kernel_release("6.12.0-211.66.1.el10_2.x86_64"));
        assert!(valid_kernel_release("6.8.0-45-generic"));
        for bad in ["", "../x", "6.1 x", "6.1;rm", "6.1/x", "x6.1"] {
            assert!(!valid_kernel_release(bad), "{bad}");
        }
    }
}
