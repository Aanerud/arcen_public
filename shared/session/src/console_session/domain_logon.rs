// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Domain logon rights for Arcen's own sign-in PAM services.
//!
//! A host joined to an Active Directory domain through SSSD with GPO
//! access control (`access_provider = ad`, `ad_gpo_access_control`
//! `enforcing` or `permissive`) decides each PAM service's logon right by
//! name (`ad_gpo_map_*`), and denies a service it does not know
//! (`ad_gpo_default_right`, default `deny`). Arcen's remote-first sign-in
//! service ([`super::greeter_token::PAM_SERVICE`]) completes the same
//! interactive logon a person at the desk makes, so it must be weighed by
//! the same right — "Allow log on locally" — and by nothing broader.
//!
//! This module reads SSSD's configuration text (`sssd.conf` and the
//! `conf.d` snippets, in SSSD's own load order) and answers which domains
//! still need the service added to `ad_gpo_map_interactive`, and the
//! snippet that adds it while keeping whatever the administrator already
//! listed. The installer writes it; the Pier reads the same answer to
//! explain a refusal.

use std::fmt::Write as _;

/// The SSSD snippet Arcen writes (`/etc/sssd/conf.d/<this>`).
pub const DROP_IN_NAME: &str = "90-arcen-interactive-logon.conf";
/// First line of that snippet; a file without it is never changed.
pub const DROP_IN_MARKER: &str = "# arcen-interactive-logon v1";

/// SSSD's default set for `ad_gpo_map_interactive` (sssd-ad(5)).
const DEFAULT_INTERACTIVE: [&str; 12] = [
    "login",
    "su",
    "su-l",
    "gdm-fingerprint",
    "gdm-password",
    "gdm-smartcard",
    "kdm",
    "lightdm",
    "lxdm",
    "sddm",
    "unity",
    "xdm",
];

/// Whether SSSD loads a `conf.d` snippet: a regular file owned and
/// protected like `sssd.conf` (root, no group or other access). Every
/// reader of SSSD's configuration in Arcen uses this, so they all see what
/// SSSD sees.
#[must_use]
#[allow(
    clippy::verbose_bit_mask,
    reason = "the permission bits read as a mode, as sssd.conf(5) states them"
)]
pub const fn sssd_reads_snippet(regular: bool, owner: u32, mode: u32) -> bool {
    regular && owner == 0 && mode & 0o077 == 0
}

/// One configuration file's text, named for the load order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SssdFile {
    pub name: String,
    pub text: String,
}

/// The effective `[section] key = value` settings of SSSD's files, merged as
/// SSSD merges them: `sssd.conf` first, then `conf.d` snippets in name
/// order, later ones winning.
#[derive(Debug, Default)]
struct Settings {
    sections: Vec<(String, Vec<(String, String)>)>,
}

impl Settings {
    fn parse(files: &[SssdFile]) -> Self {
        let mut ordered: Vec<&SssdFile> = files.iter().collect();
        // `sssd.conf` first, then the snippets by name.
        ordered.sort_by_key(|file| (file.name != "sssd.conf", file.name.clone()));
        let mut settings = Self::default();
        for file in ordered {
            let mut section: Option<String> = None;
            for line in file.text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                    continue;
                }
                if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                    section = Some(name.trim().to_owned());
                    continue;
                }
                let (Some(section), Some((key, value))) = (&section, line.split_once('=')) else {
                    continue;
                };
                settings.set(section, key.trim(), value.trim());
            }
        }
        settings
    }

    fn set(&mut self, section: &str, key: &str, value: &str) {
        let index = self
            .sections
            .iter()
            .position(|(name, _)| name == section)
            .unwrap_or_else(|| {
                self.sections.push((section.to_owned(), Vec::new()));
                self.sections.len() - 1
            });
        let entries = &mut self.sections[index].1;
        if let Some(entry) = entries.iter_mut().find(|(name, _)| name == key) {
            value.clone_into(&mut entry.1);
        } else {
            entries.push((key.to_owned(), value.to_owned()));
        }
    }

    fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.sections
            .iter()
            .find(|(name, _)| name == section)?
            .1
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    /// The active domains: `[sssd] domains`, else every `[domain/…]`.
    fn domains(&self) -> Vec<String> {
        let declared: Vec<String> = self
            .get("sssd", "domains")
            .map(|list| {
                list.split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        if !declared.is_empty() {
            return declared;
        }
        self.sections
            .iter()
            .filter_map(|(name, _)| name.strip_prefix("domain/").map(str::to_owned))
            .collect()
    }
}

/// The other `ad_gpo_map_*` options (sssd-ad(5)).
const OTHER_MAPS: [&str; 6] = [
    "ad_gpo_map_remote_interactive",
    "ad_gpo_map_network",
    "ad_gpo_map_batch",
    "ad_gpo_map_service",
    "ad_gpo_map_permit",
    "ad_gpo_map_deny",
];

fn list(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
}

/// Whether `service` is in the interactive set `value` describes (unset:
/// SSSD's default set).
fn maps_interactive(value: Option<&str>, service: &str) -> bool {
    let mut set: Vec<&str> = DEFAULT_INTERACTIVE.to_vec();
    for item in value.map(list).into_iter().flatten() {
        if let Some(removed) = item.strip_prefix('-') {
            set.retain(|name| *name != removed);
        } else {
            let added = item.strip_prefix('+').unwrap_or(item);
            if !set.contains(&added) {
                set.push(added);
            }
        }
    }
    set.contains(&service)
}

/// What a domain decides for one of Arcen's PAM services.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainLogon {
    /// GPO access control does not apply (not an AD access provider, or
    /// `ad_gpo_access_control = disabled`).
    NotGpo,
    /// The service is weighed by the interactive logon right.
    Interactive,
    /// The administrator removed the service from the interactive set
    /// explicitly; Arcen keeps that choice.
    RemovedByAdministrator,
    /// The administrator mapped the service to another logon right
    /// (`map`, e.g. `ad_gpo_map_deny` or `ad_gpo_map_permit`); SSSD refuses
    /// a service in two maps, so Arcen keeps that choice and adds nothing.
    MappedByAdministrator { map: String },
    /// The service is unknown to the domain's GPO maps: SSSD applies its
    /// default right (usually deny). `value` is what
    /// `ad_gpo_map_interactive` must become.
    Unmapped { value: String },
}

/// The decision for every active domain, for `service`.
#[must_use]
pub fn domain_logons(files: &[SssdFile], service: &str) -> Vec<(String, DomainLogon)> {
    let settings = Settings::parse(files);
    settings
        .domains()
        .into_iter()
        .map(|domain| {
            let section = format!("domain/{domain}");
            let id = settings.get(&section, "id_provider");
            let access = settings.get(&section, "access_provider").or(id);
            let gpo = settings
                .get(&section, "ad_gpo_access_control")
                .unwrap_or("enforcing");
            let decision = if access != Some("ad") || gpo.eq_ignore_ascii_case("disabled") {
                DomainLogon::NotGpo
            } else {
                let current = settings.get(&section, "ad_gpo_map_interactive");
                // An unmapped service gets the default right; only
                // `interactive` (or `permit`) already lets it in as a
                // desk logon would be let in.
                let default_admits =
                    settings
                        .get(&section, "ad_gpo_default_right")
                        .is_some_and(|right| {
                            right.eq_ignore_ascii_case("interactive")
                                || right.eq_ignore_ascii_case("permit")
                        });
                let elsewhere = OTHER_MAPS.iter().copied().find(|key| {
                    settings
                        .get(&section, key)
                        .map(list)
                        .into_iter()
                        .flatten()
                        .any(|item| item.strip_prefix('+').unwrap_or(item) == service)
                });
                if maps_interactive(current, service) {
                    DomainLogon::Interactive
                } else if current
                    .map(list)
                    .into_iter()
                    .flatten()
                    .any(|item| item.strip_prefix('-') == Some(service))
                {
                    DomainLogon::RemovedByAdministrator
                } else if let Some(map) = elsewhere {
                    // SSSD refuses a service in two maps: the
                    // administrator's map decides, Arcen adds nothing.
                    DomainLogon::MappedByAdministrator {
                        map: map.to_owned(),
                    }
                } else if default_admits {
                    DomainLogon::Interactive
                } else {
                    DomainLogon::Unmapped {
                        value: match current {
                            Some(current) if !current.trim().is_empty() => {
                                format!("{}, +{service}", current.trim())
                            }
                            _ => format!("+{service}"),
                        },
                    }
                }
            };
            (domain, decision)
        })
        .collect()
}

/// The snippet that maps `services` to the interactive logon right in every
/// domain that does not yet, keeping each domain's own list; `None` when no
/// domain needs it. `files` must not include Arcen's own snippet.
#[must_use]
pub fn drop_in(files: &[SssdFile], services: &[&str]) -> Option<String> {
    let mut sections: Vec<(String, String)> = Vec::new();
    let mut files = files.to_vec();
    for service in services {
        for (domain, decision) in domain_logons(&files, service) {
            if let DomainLogon::Unmapped { value } = decision {
                let section = format!("domain/{domain}");
                // Later services build on the earlier ones' value.
                files.push(SssdFile {
                    name: DROP_IN_NAME.to_owned(),
                    text: format!("[{section}]\nad_gpo_map_interactive = {value}\n"),
                });
                match sections.iter_mut().find(|(name, _)| *name == section) {
                    Some(entry) => entry.1 = value,
                    None => sections.push((section, value)),
                }
            }
        }
    }
    if sections.is_empty() {
        return None;
    }
    let mut text = format!(
        "{DROP_IN_MARKER}\n\
         # Written by the Arcen Pier installer (ADR 0025): Arcen's sign-in\n\
         # services are the same interactive logon as a sign-in at the desk, so\n\
         # Group Policy weighs them by \"Allow log on locally\" (and its deny),\n\
         # nothing broader. Each domain keeps its own list. Removed by the\n\
         # uninstaller.\n"
    );
    for (section, value) in sections {
        let _ = write!(text, "\n[{section}]\nad_gpo_map_interactive = {value}\n");
    }
    Some(text)
}

/// Whether Arcen's snippet among `files` is what the rest of SSSD's
/// configuration needs now. `false` when it is missing but needed, or
/// stale: SSSD keeps one value per option, so the snippet carries a copy
/// of each domain's list, and an administrator's later change to that list
/// elsewhere is shadowed until the installer writes the snippet again.
#[must_use]
pub fn drop_in_is_current(files: &[SssdFile], services: &[&str]) -> bool {
    let ours = files
        .iter()
        .find(|file| file.name == DROP_IN_NAME && is_arcen_drop_in(&file.text))
        .map(|file| file.text.as_str());
    let others: Vec<SssdFile> = files
        .iter()
        .filter(|file| file.name != DROP_IN_NAME)
        .cloned()
        .collect();
    drop_in(&others, services).as_deref() == ours
}

/// Whether a file is Arcen's own snippet.
#[must_use]
pub fn is_arcen_drop_in(text: &str) -> bool {
    text.lines().next() == Some(DROP_IN_MARKER)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conf(text: &str) -> SssdFile {
        SssdFile {
            name: "sssd.conf".into(),
            text: text.into(),
        }
    }

    const AD: &str = "[sssd]\ndomains = corp.example.internal\nservices = nss, pam\n\
                      [domain/corp.example.internal]\nid_provider = ad\naccess_provider = ad\n";

    #[test]
    fn an_ad_gpo_domain_gets_the_service_as_an_interactive_logon_only() {
        let text = drop_in(&[conf(AD)], &["gdm-ovirtcred"]).expect("needed");
        assert!(is_arcen_drop_in(&text));
        assert!(
            text.contains(
                "[domain/corp.example.internal]\nad_gpo_map_interactive = +gdm-ovirtcred\n"
            )
        );
        for broader in [
            "ad_gpo_map_remote_interactive",
            "ad_gpo_default_right",
            "permit",
        ] {
            assert!(!text.contains(broader), "{broader}");
        }
        // With the snippet in place nothing more is needed.
        let with = [
            conf(AD),
            SssdFile {
                name: DROP_IN_NAME.into(),
                text,
            },
        ];
        assert_eq!(drop_in(&with, &["gdm-ovirtcred"]), None);
        assert_eq!(
            domain_logons(&with, "gdm-ovirtcred"),
            [("corp.example.internal".to_owned(), DomainLogon::Interactive)]
        );
    }

    #[test]
    fn the_administrators_own_list_is_kept_and_an_explicit_removal_respected() {
        let custom = format!("{AD}ad_gpo_map_interactive = +my-login, -su\n");
        let text = drop_in(&[conf(&custom)], &["gdm-ovirtcred", "arcen-login"]).expect("needed");
        assert!(
            text.contains(
                "ad_gpo_map_interactive = +my-login, -su, +gdm-ovirtcred, +arcen-login\n"
            )
        );
        let removed = format!("{AD}ad_gpo_map_interactive = -gdm-ovirtcred\n");
        assert_eq!(drop_in(&[conf(&removed)], &["gdm-ovirtcred"]), None);
        assert_eq!(
            domain_logons(&[conf(&removed)], "gdm-ovirtcred")[0].1,
            DomainLogon::RemovedByAdministrator
        );
    }

    #[test]
    fn hosts_without_ad_gpo_access_control_need_nothing() {
        for text in [
            "[sssd]\ndomains = l\n[domain/l]\nid_provider = ldap\n",
            &format!("{AD}ad_gpo_access_control = disabled\n"),
            "[sssd]\ndomains = c\n[domain/c]\nid_provider = ad\naccess_provider = simple\n",
            "",
        ] {
            assert_eq!(drop_in(&[conf(text)], &["gdm-ovirtcred"]), None, "{text}");
        }
        // The access provider defaults to the identity provider.
        let implicit = "[sssd]\ndomains = c\n[domain/c]\nid_provider = ad\n";
        assert!(drop_in(&[conf(implicit)], &["gdm-ovirtcred"]).is_some());
        // Permissive still evaluates (and logs) the policy.
        assert!(
            drop_in(
                &[conf(&format!("{AD}ad_gpo_access_control = permissive\n"))],
                &["x"]
            )
            .is_some()
        );
    }

    #[test]
    fn a_default_right_that_admits_unmapped_services_needs_nothing() {
        for right in ["interactive", "permit"] {
            let text = format!("{AD}ad_gpo_default_right = {right}\n");
            assert_eq!(drop_in(&[conf(&text)], &["gdm-ovirtcred"]), None, "{right}");
        }
        let deny = format!("{AD}ad_gpo_default_right = deny\n");
        assert!(drop_in(&[conf(&deny)], &["gdm-ovirtcred"]).is_some());
        // A service the administrator mapped to another right keeps it:
        // SSSD refuses a service in two maps.
        for map in [
            "ad_gpo_map_deny",
            "ad_gpo_map_permit",
            "ad_gpo_map_remote_interactive",
        ] {
            let elsewhere = format!("{AD}{map} = +gdm-ovirtcred\n");
            assert_eq!(
                drop_in(&[conf(&elsewhere)], &["gdm-ovirtcred"]),
                None,
                "{map}"
            );
            assert_eq!(
                domain_logons(&[conf(&elsewhere)], "gdm-ovirtcred")[0].1,
                DomainLogon::MappedByAdministrator {
                    map: map.to_owned()
                }
            );
        }
    }

    #[test]
    fn a_snippet_that_shadows_a_later_change_is_stale() {
        let ours = drop_in(&[conf(AD)], &["gdm-ovirtcred"]).expect("needed");
        let with = |base: &str| {
            vec![
                conf(base),
                SssdFile {
                    name: DROP_IN_NAME.into(),
                    text: ours.clone(),
                },
            ]
        };
        assert!(drop_in_is_current(&with(AD), &["gdm-ovirtcred"]));
        let changed = format!("{AD}ad_gpo_map_interactive = +kiosk-login\n");
        assert!(!drop_in_is_current(&with(&changed), &["gdm-ovirtcred"]));
        assert!(
            !drop_in_is_current(&[conf(AD)], &["gdm-ovirtcred"]),
            "missing"
        );
        assert!(
            drop_in_is_current(&[conf("")], &["gdm-ovirtcred"]),
            "not needed"
        );
    }

    #[test]
    fn only_snippets_protected_like_sssd_conf_are_read() {
        assert!(sssd_reads_snippet(true, 0, 0o100_600));
        for (regular, owner, mode) in [(true, 0, 0o644), (true, 1000, 0o600), (false, 0, 0o600)] {
            assert!(!sssd_reads_snippet(regular, owner, mode));
        }
    }

    #[test]
    fn snippets_load_after_sssd_conf_in_name_order_and_win() {
        let files = [
            SssdFile {
                name: "50-site.conf".into(),
                text: "[domain/corp.example.internal]\nad_gpo_map_interactive = +gdm-ovirtcred\n"
                    .into(),
            },
            conf(AD),
        ];
        assert_eq!(drop_in(&files, &["gdm-ovirtcred"]), None);
        // Default services are interactive already.
        assert_eq!(drop_in(&[conf(AD)], &["login"]), None);
        // Domains without `[sssd] domains` are found by their sections.
        let undeclared = "[domain/a]\nid_provider = ad\n[domain/b]\nid_provider = ad\n";
        let text = drop_in(&[conf(undeclared)], &["gdm-ovirtcred"]).expect("needed");
        assert!(text.contains("[domain/a]") && text.contains("[domain/b]"));
    }
}
