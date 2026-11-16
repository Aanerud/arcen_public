// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Windows Deck's per-tablet `WinUSB` package and its per-session claim,
//! as data and plans. The Windows adapter (`arcen-usb-prepare`,
//! `arcen-usb-service`) only carries the steps out.
//!
//! Proven on Windows 11 x64 with Secure Boot on and no test signing
//! (ADR 0024):
//!
//! - an INF that binds Microsoft's in-box `winusb.sys` to one hardware ID,
//!   with a catalog signed by a per-install self-signed certificate whose
//!   key never touches disk, stages without any dialog while that
//!   certificate is in `LocalMachine\Root` **and** `TrustedPublisher`;
//! - the trust entries are needed only while staging: a staged package binds
//!   after they are removed again;
//! - a staged hardware-ID package outranks the in-box HID driver, so it must
//!   never stay in the driver store between sessions, or a freshly plugged
//!   tablet would lose its own driver;
//! - `DiRollbackDriver` keeps no rollback data for this path: release
//!   reinstalls the recorded prior driver, then removes the package with
//!   `/uninstall`, which also reinstalls the next-best driver on any other
//!   instance that picked it up.
//!
//! So a claim is: trust, stage, distrust, bind; a release is: restore,
//! unstage, distrust. Every step is recorded in a [`ClaimJournal`] *before*
//! it runs, and [`ClaimJournal::recovery_plan`] undoes whatever a crash left.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

/// Device interface GUID the Arcen `WinUSB` INF registers, through which the
/// service finds a bound tablet.
pub const WINUSB_DEVICE_INTERFACE_GUID: &str = "{0BECB574-029E-4AB9-89DD-DF6903E0C352}";
/// Setup class of `WinUSB` devices (`USBDevice`).
pub const USB_DEVICE_CLASS_GUID: &str = "{88BAE032-5A81-49f0-BC3D-A4FF138216D6}";
/// File-name prefix of every Arcen package, so nothing else is ever touched.
pub const PACKAGE_PREFIX: &str = "arcen_usb_";
/// Subject prefix of every per-install certificate.
pub const CERTIFICATE_SUBJECT_PREFIX: &str = "CN=Arcen USB component ";
/// The component's Windows service (`LocalSystem`, automatic start).
pub const SERVICE_NAME: &str = "ArcenUsbComponent";
/// The named pipe the Deck reaches the service on.
pub const PIPE_NAME: &str = r"\\.\pipe\arcen-usb-component";

/// A tablet model: the device-level hardware ID a package binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TabletModel {
    /// `idVendor`.
    pub vendor_id: u16,
    /// `idProduct`.
    pub product_id: u16,
}

impl TabletModel {
    /// `USB\VID_056A&PID_0317`.
    #[must_use]
    pub fn hardware_id(self) -> String {
        format!(
            "USB\\VID_{:04X}&PID_{:04X}",
            self.vendor_id, self.product_id
        )
    }

    /// `arcen_usb_056a_0317`: the INF and catalog base name.
    #[must_use]
    pub fn package_name(self) -> String {
        format!(
            "{PACKAGE_PREFIX}{:04x}_{:04x}",
            self.vendor_id, self.product_id
        )
    }

    /// The model a prepared package directory is for: the inverse of
    /// [`Self::package_name`], and nothing else.
    #[must_use]
    pub fn from_package_name(name: &str) -> Option<Self> {
        let ids = name.strip_prefix(PACKAGE_PREFIX)?;
        let (vendor, product) = ids.split_once('_')?;
        let model = Self {
            vendor_id: hex4(vendor)?,
            product_id: hex4(product)?,
        };
        (model.package_name() == name).then_some(model)
    }

    /// The model a device-level hardware or instance ID names:
    /// `USB\VID_xxxx&PID_yyyy`, optionally followed by `&REV_zzzz` or by
    /// `\<instance>`. An interface ID (`&MI_nn`) or anything else is not a
    /// device the package binds and yields `None`.
    #[must_use]
    pub fn from_device_id(id: &str) -> Option<Self> {
        Self::split_device_id(id).map(|(model, _)| model)
    }

    /// The model and the instance part (after the second `\`), if any.
    fn split_device_id(id: &str) -> Option<(Self, Option<&str>)> {
        let rest = strip_prefix_ignore_case(id, "USB\\VID_")?;
        let (device, instance) = match rest.split_once('\\') {
            Some((device, instance)) => (device, Some(instance)),
            None => (rest, None),
        };
        let vendor_id = hex4(device.get(..4)?)?;
        let tail = strip_prefix_ignore_case(device.get(4..)?, "&PID_")?;
        let product_id = hex4(tail.get(..4)?)?;
        let tail = tail.get(4..)?;
        if !tail.is_empty() {
            hex4(strip_prefix_ignore_case(tail, "&REV_")?)?;
        }
        Some((
            Self {
                vendor_id,
                product_id,
            },
            instance,
        ))
    }
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

fn hex4(text: &str) -> Option<u16> {
    (text.len() == 4 && text.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| u16::from_str_radix(text, 16).ok())
        .flatten()
}

/// The INF `DriverVer` date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InfDate {
    /// Four-digit year.
    pub year: u16,
    /// 1–12.
    pub month: u8,
    /// 1–31.
    pub day: u8,
}

/// The INF that binds the in-box `WinUSB` to one tablet model. The adapter
/// writes it as UTF-16LE.
#[must_use]
pub fn winusb_inf(model: TabletModel, date: InfDate) -> String {
    let name = model.package_name();
    let hardware_id = model.hardware_id();
    let mut inf = String::new();
    let _ = write!(
        inf,
        "; Arcen USB component: binds Microsoft's in-box WinUSB to one tablet model on this PC.\r\n\
         ; Generated on this PC for this PC; signed with a certificate whose key never left memory.\r\n\
         [Version]\r\n\
         Signature   = \"$Windows NT$\"\r\n\
         Class       = USBDevice\r\n\
         ClassGuid   = {USB_DEVICE_CLASS_GUID}\r\n\
         Provider    = %Provider%\r\n\
         CatalogFile = {name}.cat\r\n\
         DriverVer   = {month:02}/{day:02}/{year:04},1.0.0.0\r\n\
         PnpLockdown = 1\r\n\
         \r\n\
         [Manufacturer]\r\n\
         %Provider% = Arcen,NTamd64\r\n\
         \r\n\
         [Arcen.NTamd64]\r\n\
         %DeviceName% = Arcen_Install, {hardware_id}\r\n\
         \r\n\
         [Arcen_Install]\r\n\
         Include = winusb.inf\r\n\
         Needs   = WINUSB.NT\r\n\
         \r\n\
         [Arcen_Install.Services]\r\n\
         Include = winusb.inf\r\n\
         Needs   = WINUSB.NT.Services\r\n\
         \r\n\
         [Arcen_Install.HW]\r\n\
         AddReg = Arcen_AddReg\r\n\
         \r\n\
         [Arcen_AddReg]\r\n\
         HKR,,DeviceInterfaceGUIDs,0x10000,\"{WINUSB_DEVICE_INTERFACE_GUID}\"\r\n\
         \r\n\
         [Strings]\r\n\
         Provider   = \"Arcen USB component (this PC)\"\r\n\
         DeviceName = \"Arcen Native tablet {vendor:04X}:{product:04X} (WinUSB)\"\r\n",
        month = date.month,
        day = date.day,
        year = date.year,
        vendor = model.vendor_id,
        product = model.product_id,
    );
    inf
}

/// The catalog definition (makecat CDF) for the package in `directory`:
/// SHA-256, `OSAttr` Windows 10+, one member — the INF.
#[must_use]
pub fn catalog_definition(model: TabletModel, directory: &str) -> String {
    let name = model.package_name();
    format!(
        "[CatalogHeader]\r\n\
         Name={name}.cat\r\n\
         ResultDir={directory}\r\n\
         PublicVersion=0x0000001\r\n\
         EncodingType=0x00010001\r\n\
         CatalogVersion=2\r\n\
         HashAlgorithms=SHA256\r\n\
         CATATTR1=0x10010001:OSAttr:2:10.0\r\n\
         [CatalogFiles]\r\n\
         <hash>{name}.inf={directory}\\{name}.inf\r\n\
         <hash>{name}.infATTR1=0x10010001:File:{name}.inf\r\n\
         <hash>{name}.infATTR2=0x10010001:OSAttr:2:10.0\r\n"
    )
}

/// Whether `name` is a bare INF file name (`input.inf`, `oem12.inf`): no
/// path, no traversal, nothing a step could be redirected with.
#[must_use]
pub fn is_plain_inf_name(name: &str) -> bool {
    let Some(stem) = name
        .len()
        .checked_sub(4)
        .and_then(|cut| name.get(..cut))
        .filter(|_| name[name.len() - 4..].eq_ignore_ascii_case(".inf"))
    else {
        return false;
    };
    !stem.is_empty()
        && stem.len() <= 64
        && stem
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// Whether a published INF (`oemNN.inf`) name is one the store assigns.
#[must_use]
pub fn is_published_oem_inf(name: &str) -> bool {
    is_plain_inf_name(name)
        && name.len() > 7
        && name[..3].eq_ignore_ascii_case("oem")
        && name[3..name.len() - 4]
            .bytes()
            .all(|byte| byte.is_ascii_digit())
}

/// Whether an INF's original name is an Arcen package.
#[must_use]
pub fn is_arcen_package(original_name: &str) -> bool {
    original_name
        .get(..PACKAGE_PREFIX.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(PACKAGE_PREFIX))
}

/// Whether `id` is a plausible device instance ID: `ENUMERATOR\DEVICE\INSTANCE`,
/// printable ASCII, no empty part and no `..`.
#[must_use]
pub fn is_device_instance_id(id: &str) -> bool {
    let parts: Vec<&str> = id.split('\\').collect();
    id.len() <= 200
        && parts.len() == 3
        && parts.iter().all(|part| !part.is_empty() && *part != "..")
        && id.bytes().all(|byte| byte.is_ascii_graphic())
}

/// A pen tablet the Deck sees attached (a USB device with a digitizer
/// collection): its device-level instance ID and model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedTablet {
    /// `USB\VID_…&PID_…\<instance>`.
    pub instance_id: String,
    /// The model its IDs name.
    pub model: TabletModel,
}

/// Whether the attached tablet is ready for a session, as the Deck reports
/// it beside the Native tablet option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabletPreparation {
    /// No tablet is attached; nothing to prepare.
    NoTablet,
    /// A tablet whose model is prepared is attached (the one the service
    /// claims).
    Prepared {
        /// Its instance ID.
        instance_id: String,
    },
    /// Tablets are attached, but none of their models is prepared.
    NeedsPreparing {
        /// The first one, which "Prepare this tablet…" prepares.
        instance_id: String,
    },
}

impl TabletPreparation {
    /// Decides from what is attached and which models are prepared. A
    /// prepared tablet wins over an unprepared one, as in the service's own
    /// choice of tablet to claim.
    #[must_use]
    pub fn assess(attached: &[AttachedTablet], prepared: &[TabletModel]) -> Self {
        if let Some(tablet) = attached
            .iter()
            .find(|tablet| prepared.contains(&tablet.model))
        {
            return Self::Prepared {
                instance_id: tablet.instance_id.clone(),
            };
        }
        attached
            .first()
            .map_or(Self::NoTablet, |tablet| Self::NeedsPreparing {
                instance_id: tablet.instance_id.clone(),
            })
    }

    /// The `device_prepared` fact of
    /// [`crate::native_component::NativeComponentFacts`].
    #[must_use]
    pub const fn device_prepared(&self) -> Option<bool> {
        match self {
            Self::NoTablet => None,
            Self::Prepared { .. } => Some(true),
            Self::NeedsPreparing { .. } => Some(false),
        }
    }
}

/// The driver a tablet instance had before Arcen claimed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriorDriver {
    /// What `DEVPKEY_Device_DriverInfPath` named (`input.inf`, `oem7.inf`).
    pub inf: String,
}

/// Why a tablet instance may not be claimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimRefusal {
    /// The instance ID is not a device-level USB ID of the prepared model.
    NotThisModel,
    /// The instance already runs an Arcen package: a previous session did not
    /// release it. Recover first.
    AlreadyClaimed,
    /// The recorded prior driver is not a bare INF name.
    UntrustedPriorDriver,
    /// A claim for this instance is already in the journal.
    InJournal,
}

impl std::fmt::Display for ClaimRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotThisModel => "the device is not the prepared tablet model",
            Self::AlreadyClaimed => {
                "the tablet is still bound to the Arcen USB component from an earlier session"
            }
            Self::UntrustedPriorDriver => "the tablet's current driver could not be recorded",
            Self::InJournal => "the tablet is already claimed",
        })
    }
}

/// One tablet instance's claim, written before each step it records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimRecord {
    /// The device instance (`USB\VID_056A&PID_0317\8&2a1b…`).
    pub instance_id: String,
    /// Its model.
    pub model: TabletModel,
    /// The driver to put back; `None` when the instance had none.
    pub prior_driver: Option<PriorDriver>,
    /// Thumbprint of the package's certificate (hex).
    pub certificate_thumbprint: String,
    /// The certificate may be in the machine's trust stores.
    pub trust_added: bool,
    /// The package may be staged, under this published name once known.
    pub staging: bool,
    /// The published name the store gave the package.
    pub published_inf: Option<String>,
    /// The instance may be bound to `WinUSB`.
    pub bound: bool,
}

/// One step of putting a machine back as it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseStep {
    /// Reinstall the recorded driver on one instance.
    RestoreDriver {
        /// The instance.
        instance_id: String,
        /// Its prior driver.
        inf: String,
    },
    /// `pnputil /delete-driver <inf> /uninstall`: every instance still on
    /// the package gets its next-best driver.
    UnstagePackage {
        /// The published name.
        published_inf: String,
    },
    /// Delete every staged Arcen package of this model (the published name
    /// was never learnt: a crash between staging and recording it).
    UnstageModel {
        /// The model whose `arcen_usb_*` packages to delete.
        model: TabletModel,
    },
    /// Remove the certificate from `Root` and `TrustedPublisher`.
    Distrust {
        /// Its thumbprint.
        thumbprint: String,
    },
}

/// Every claim the service has under way, persisted before each step.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimJournal {
    /// One record per claimed instance.
    pub claims: Vec<ClaimRecord>,
}

impl ClaimJournal {
    /// Opens a claim for `instance_id`, refusing an instance whose own
    /// hardware IDs do not name the model (that is what Windows matches the
    /// package's INF against), a malformed instance ID, an instance that is
    /// already Arcen's, or one already being claimed.
    ///
    /// # Errors
    ///
    /// The [`ClaimRefusal`] that applies.
    pub fn begin(
        &mut self,
        instance_id: &str,
        hardware_ids: &[String],
        model: TabletModel,
        prior_driver: Option<PriorDriver>,
        prior_driver_is_arcen: bool,
        certificate_thumbprint: &str,
    ) -> Result<&mut ClaimRecord, ClaimRefusal> {
        if !is_device_instance_id(instance_id)
            || !hardware_ids
                .iter()
                .any(|id| TabletModel::from_device_id(id) == Some(model))
        {
            return Err(ClaimRefusal::NotThisModel);
        }
        if prior_driver_is_arcen {
            return Err(ClaimRefusal::AlreadyClaimed);
        }
        if prior_driver
            .as_ref()
            .is_some_and(|prior| !is_plain_inf_name(&prior.inf))
        {
            return Err(ClaimRefusal::UntrustedPriorDriver);
        }
        if self
            .claims
            .iter()
            .any(|claim| claim.instance_id.eq_ignore_ascii_case(instance_id))
        {
            return Err(ClaimRefusal::InJournal);
        }
        self.claims.push(ClaimRecord {
            instance_id: instance_id.to_owned(),
            model,
            prior_driver,
            certificate_thumbprint: certificate_thumbprint.to_owned(),
            trust_added: false,
            staging: false,
            published_inf: None,
            bound: false,
        });
        let index = self.claims.len() - 1;
        Ok(&mut self.claims[index])
    }

    /// The record for one instance.
    pub fn record_mut(&mut self, instance_id: &str) -> Option<&mut ClaimRecord> {
        self.claims
            .iter_mut()
            .find(|claim| claim.instance_id.eq_ignore_ascii_case(instance_id))
    }

    /// The steps that put one instance back, in order: its driver, then the
    /// package, then the trust entries — each only if a step that needs
    /// undoing may have run, and only when no other claim still uses it.
    #[must_use]
    pub fn release_plan(&self, instance_id: &str) -> Vec<ReleaseStep> {
        let Some(claim) = self
            .claims
            .iter()
            .find(|claim| claim.instance_id.eq_ignore_ascii_case(instance_id))
        else {
            return Vec::new();
        };
        let others: Vec<&ClaimRecord> = self
            .claims
            .iter()
            .filter(|other| !other.instance_id.eq_ignore_ascii_case(instance_id))
            .collect();
        let mut steps = Vec::new();
        push_restore(claim, &mut steps);
        if claim.staging
            && !others
                .iter()
                .any(|other| other.staging && other.model == claim.model)
        {
            push_unstage(claim, &mut steps);
        }
        if claim.trust_added
            && !others.iter().any(|other| {
                other.trust_added && other.certificate_thumbprint == claim.certificate_thumbprint
            })
        {
            steps.push(ReleaseStep::Distrust {
                thumbprint: claim.certificate_thumbprint.clone(),
            });
        }
        steps
    }

    /// Everything a crash may have left, in order: every bound instance's
    /// driver, then every package, then every trust entry, each once.
    #[must_use]
    pub fn recovery_plan(&self) -> Vec<ReleaseStep> {
        let mut steps = Vec::new();
        for claim in &self.claims {
            push_restore(claim, &mut steps);
        }
        for claim in self.claims.iter().filter(|claim| claim.staging) {
            push_unstage(claim, &mut steps);
        }
        for claim in self.claims.iter().filter(|claim| claim.trust_added) {
            let step = ReleaseStep::Distrust {
                thumbprint: claim.certificate_thumbprint.clone(),
            };
            if !steps.contains(&step) {
                steps.push(step);
            }
        }
        steps
    }

    /// Records that one release step succeeded, so a failed step stays in
    /// the journal and the next recovery retries exactly what is left.
    ///
    /// Removing a package with `/uninstall` reinstalls the next-best driver
    /// on every instance still using it, so it also unbinds those instances.
    pub fn mark_done(&mut self, step: &ReleaseStep) {
        for claim in &mut self.claims {
            match step {
                ReleaseStep::RestoreDriver { instance_id, .. }
                    if claim.instance_id.eq_ignore_ascii_case(instance_id) =>
                {
                    claim.bound = false;
                }
                ReleaseStep::UnstagePackage { published_inf }
                    if claim
                        .published_inf
                        .as_deref()
                        .is_some_and(|name| name.eq_ignore_ascii_case(published_inf)) =>
                {
                    claim.staging = false;
                    claim.published_inf = None;
                    claim.bound = false;
                }
                ReleaseStep::UnstageModel { model } if claim.model == *model => {
                    claim.staging = false;
                    claim.published_inf = None;
                    claim.bound = false;
                }
                ReleaseStep::Distrust { thumbprint }
                    if claim
                        .certificate_thumbprint
                        .eq_ignore_ascii_case(thumbprint) =>
                {
                    claim.trust_added = false;
                }
                _ => {}
            }
        }
    }

    /// Forgets every claim with nothing left to undo.
    pub fn prune(&mut self) {
        self.claims
            .retain(|claim| claim.bound || claim.staging || claim.trust_added);
    }

    /// Forgets an instance once its release plan has run.
    pub fn finish(&mut self, instance_id: &str) {
        self.claims
            .retain(|claim| !claim.instance_id.eq_ignore_ascii_case(instance_id));
    }
}

fn push_restore(claim: &ClaimRecord, steps: &mut Vec<ReleaseStep>) {
    if let (true, Some(prior)) = (claim.bound, &claim.prior_driver) {
        steps.push(ReleaseStep::RestoreDriver {
            instance_id: claim.instance_id.clone(),
            inf: prior.inf.clone(),
        });
    }
}

fn push_unstage(claim: &ClaimRecord, steps: &mut Vec<ReleaseStep>) {
    let step = match &claim.published_inf {
        Some(published) if is_published_oem_inf(published) => ReleaseStep::UnstagePackage {
            published_inf: published.clone(),
        },
        _ => ReleaseStep::UnstageModel { model: claim.model },
    };
    if !steps.contains(&step) {
        steps.push(step);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTUOS5: TabletModel = TabletModel {
        vendor_id: 0x056a,
        product_id: 0x0317,
    };
    const INSTANCE: &str = "USB\\VID_056A&PID_0317\\8&2A1B3C4D&0&1";
    const THUMB: &str = "4F6E81D0023137C2768BECA2BFC32D300F47A162";

    #[test]
    fn a_model_names_its_hardware_id_and_package() {
        assert_eq!(INTUOS5.hardware_id(), "USB\\VID_056A&PID_0317");
        assert_eq!(INTUOS5.package_name(), "arcen_usb_056a_0317");
    }

    #[test]
    fn only_an_exact_package_name_names_a_prepared_model() {
        assert_eq!(
            TabletModel::from_package_name("arcen_usb_056a_0317"),
            Some(INTUOS5)
        );
        for name in [
            "arcen_usb_056A_0317",
            "arcen_usb_056a_0317.old",
            "arcen_usb_56a_317",
            "arcen_usb_056a",
            "other_056a_0317",
            "arcen_usb_056a_0317_x",
        ] {
            assert_eq!(TabletModel::from_package_name(name), None, "{name}");
        }
    }

    #[test]
    fn a_prepared_attached_tablet_wins_and_otherwise_the_first_needs_preparing() {
        let other = TabletModel {
            vendor_id: 0x256c,
            product_id: 0x006d,
        };
        let huion = AttachedTablet {
            instance_id: "USB\\VID_256C&PID_006D\\1".to_owned(),
            model: other,
        };
        let intuos = AttachedTablet {
            instance_id: INSTANCE.to_owned(),
            model: INTUOS5,
        };
        let none = TabletPreparation::assess(&[], &[INTUOS5]);
        assert_eq!(none, TabletPreparation::NoTablet);
        assert_eq!(none.device_prepared(), None);
        let unprepared = TabletPreparation::assess(&[huion.clone(), intuos.clone()], &[]);
        assert_eq!(
            unprepared,
            TabletPreparation::NeedsPreparing {
                instance_id: huion.instance_id.clone()
            }
        );
        assert_eq!(unprepared.device_prepared(), Some(false));
        let prepared = TabletPreparation::assess(&[huion, intuos], &[INTUOS5]);
        assert_eq!(
            prepared,
            TabletPreparation::Prepared {
                instance_id: INSTANCE.to_owned()
            }
        );
        assert_eq!(prepared.device_prepared(), Some(true));
    }

    #[test]
    fn only_device_level_usb_ids_name_a_model() {
        for id in [
            "USB\\VID_056A&PID_0317",
            "usb\\vid_056a&pid_0317",
            "USB\\VID_056A&PID_0317&REV_0100",
            INSTANCE,
            "USB\\VID_056A&PID_0317\\28754-0000:00:1D.7-1",
        ] {
            assert_eq!(TabletModel::from_device_id(id), Some(INTUOS5), "{id}");
        }
        for id in [
            "USB\\VID_056A&PID_0317&MI_00",
            "USB\\VID_056A&PID_0317&MI_00\\7&1",
            "USB\\VID_056A&PID_031",
            "USB\\VID_056A&PID_0317&REV_01",
            "USB\\VID_056A&PID_0317X",
            "HID\\VID_056A&PID_0317",
            "USB\\VID_G56A&PID_0317",
            "USB\\ROOT_HUB20\\4&2146AE0&0",
            "",
            "USB",
            "USB\\VID_056A&PID_0317&REV_+100",
        ] {
            assert_eq!(TabletModel::from_device_id(id), None, "{id}");
        }
    }

    #[test]
    fn the_inf_is_the_one_proven_on_secure_boot() {
        let inf = winusb_inf(
            INTUOS5,
            InfDate {
                year: 2026,
                month: 11,
                day: 14,
            },
        );
        for line in [
            "Class       = USBDevice\r\n",
            "CatalogFile = arcen_usb_056a_0317.cat\r\n",
            "DriverVer   = 11/14/2026,1.0.0.0\r\n",
            "PnpLockdown = 1\r\n",
            "%DeviceName% = Arcen_Install, USB\\VID_056A&PID_0317\r\n",
            "Include = winusb.inf\r\nNeeds   = WINUSB.NT\r\n",
            "Needs   = WINUSB.NT.Services\r\n",
            "HKR,,DeviceInterfaceGUIDs,0x10000,\"{0BECB574-029E-4AB9-89DD-DF6903E0C352}\"\r\n",
            "Signature   = \"$Windows NT$\"\r\n",
        ] {
            assert!(inf.contains(line), "missing {line:?}");
        }
        assert!(!inf.contains('\t'));
    }

    #[test]
    fn the_catalog_definition_hashes_only_the_inf() {
        let cdf = catalog_definition(INTUOS5, "C:\\ProgramData\\Arcen\\USB\\arcen_usb_056a_0317");
        assert!(cdf.contains("Name=arcen_usb_056a_0317.cat\r\n"));
        assert!(cdf.contains("HashAlgorithms=SHA256\r\n"));
        assert!(cdf.contains(
            "<hash>arcen_usb_056a_0317.inf=C:\\ProgramData\\Arcen\\USB\\arcen_usb_056a_0317\\arcen_usb_056a_0317.inf\r\n"
        ));
        assert_eq!(cdf.matches("<hash>").count(), 3);
    }

    #[test]
    fn inf_names_are_bare_and_published_names_are_the_stores() {
        for good in ["input.inf", "oem12.inf", "wacom_usb-x.INF"] {
            assert!(is_plain_inf_name(good), "{good}");
        }
        for bad in [
            "",
            ".inf",
            "input",
            "..\\input.inf",
            "C:\\Windows\\INF\\input.inf",
            "a b.inf",
            "x.inf\0",
            "inf",
        ] {
            assert!(!is_plain_inf_name(bad), "{bad:?}");
        }
        assert!(is_published_oem_inf("oem9.inf"));
        assert!(is_published_oem_inf("OEM123.INF"));
        assert!(!is_published_oem_inf("oem.inf"));
        assert!(!is_published_oem_inf("oemx.inf"));
        assert!(!is_published_oem_inf("input.inf"));
        assert!(is_arcen_package("arcen_usb_056a_0317.inf"));
        assert!(!is_arcen_package("input.inf"));
    }

    fn hardware() -> Vec<String> {
        vec![
            "USB\\VID_056A&PID_0317&REV_0100".to_owned(),
            "USB\\VID_056A&PID_0317".to_owned(),
        ]
    }

    fn claimed(journal: &mut ClaimJournal, instance: &str) {
        let record = journal
            .begin(
                instance,
                &hardware(),
                INTUOS5,
                Some(PriorDriver {
                    inf: "input.inf".to_owned(),
                }),
                false,
                THUMB,
            )
            .expect("claim opens");
        record.trust_added = true;
        record.staging = true;
        record.published_inf = Some("oem9.inf".to_owned());
        record.bound = true;
    }

    #[test]
    fn a_whole_claim_releases_driver_then_package_then_trust() {
        let mut journal = ClaimJournal::default();
        claimed(&mut journal, INSTANCE);
        assert_eq!(
            journal.release_plan(INSTANCE),
            vec![
                ReleaseStep::RestoreDriver {
                    instance_id: INSTANCE.to_owned(),
                    inf: "input.inf".to_owned()
                },
                ReleaseStep::UnstagePackage {
                    published_inf: "oem9.inf".to_owned()
                },
                ReleaseStep::Distrust {
                    thumbprint: THUMB.to_owned()
                },
            ]
        );
        assert_eq!(journal.recovery_plan(), journal.release_plan(INSTANCE));
        journal.finish(INSTANCE);
        assert!(journal.claims.is_empty());
        assert!(journal.recovery_plan().is_empty());
    }

    #[test]
    fn a_failed_step_stays_in_the_journal_and_is_retried_alone() {
        let mut journal = ClaimJournal::default();
        claimed(&mut journal, INSTANCE);
        let plan = journal.release_plan(INSTANCE);
        // The driver came back and the trust went, but pnputil failed.
        journal.mark_done(&plan[0]);
        journal.mark_done(&plan[2]);
        journal.prune();
        assert_eq!(journal.claims.len(), 1, "the package is still staged");
        assert_eq!(
            journal.recovery_plan(),
            vec![ReleaseStep::UnstagePackage {
                published_inf: "oem9.inf".to_owned()
            }]
        );
        journal.mark_done(&ReleaseStep::UnstagePackage {
            published_inf: "OEM9.INF".to_owned(),
        });
        journal.prune();
        assert!(journal.claims.is_empty());
    }

    #[test]
    fn removing_the_package_also_unbinds_an_instance_whose_restore_failed() {
        let mut journal = ClaimJournal::default();
        claimed(&mut journal, INSTANCE);
        journal.mark_done(&ReleaseStep::UnstageModel { model: INTUOS5 });
        journal.mark_done(&ReleaseStep::Distrust {
            thumbprint: THUMB.to_owned(),
        });
        journal.prune();
        assert!(journal.claims.is_empty());
    }

    #[test]
    fn a_crash_after_staging_but_before_the_name_was_learnt_unstages_the_model() {
        let mut journal = ClaimJournal::default();
        let record = journal
            .begin(INSTANCE, &hardware(), INTUOS5, None, false, THUMB)
            .expect("claim opens");
        record.trust_added = true;
        record.staging = true;
        assert_eq!(
            journal.recovery_plan(),
            vec![
                ReleaseStep::UnstageModel { model: INTUOS5 },
                ReleaseStep::Distrust {
                    thumbprint: THUMB.to_owned()
                },
            ]
        );
    }

    #[test]
    fn a_device_without_a_prior_driver_is_released_by_unstaging_alone() {
        let mut journal = ClaimJournal::default();
        let record = journal
            .begin(INSTANCE, &hardware(), INTUOS5, None, false, THUMB)
            .expect("claim opens");
        record.staging = true;
        record.published_inf = Some("oem9.inf".to_owned());
        record.bound = true;
        assert_eq!(
            journal.release_plan(INSTANCE),
            vec![ReleaseStep::UnstagePackage {
                published_inf: "oem9.inf".to_owned()
            }]
        );
    }

    #[test]
    fn a_second_identical_tablet_keeps_the_shared_package_until_the_last_release() {
        let second = "USB\\VID_056A&PID_0317\\8&FFFF&0&2";
        let mut journal = ClaimJournal::default();
        claimed(&mut journal, INSTANCE);
        claimed(&mut journal, second);
        assert_eq!(
            journal.release_plan(INSTANCE),
            vec![ReleaseStep::RestoreDriver {
                instance_id: INSTANCE.to_owned(),
                inf: "input.inf".to_owned()
            }]
        );
        journal.finish(INSTANCE);
        assert_eq!(journal.release_plan(second).len(), 3);
        claimed(&mut journal, INSTANCE);
        let recovery = journal.recovery_plan();
        assert_eq!(recovery.len(), 4, "two restores, one unstage, one distrust");
    }

    #[test]
    fn claims_that_cannot_be_undone_safely_are_refused() {
        let mut journal = ClaimJournal::default();
        let other = vec!["USB\\VID_056A&PID_0318".to_owned()];
        assert_eq!(
            journal
                .begin(INSTANCE, &other, INTUOS5, None, false, THUMB)
                .err(),
            Some(ClaimRefusal::NotThisModel),
            "the device's own hardware IDs decide the model"
        );
        let interface_only = vec!["USB\\VID_056A&PID_0317&MI_00".to_owned()];
        assert_eq!(
            journal
                .begin(INSTANCE, &interface_only, INTUOS5, None, false, THUMB)
                .err(),
            Some(ClaimRefusal::NotThisModel)
        );
        for malformed in ["USB\\VID_056A&PID_0317", "USB\\..\\x", "USB\\a\\b c", ""] {
            assert_eq!(
                journal
                    .begin(malformed, &hardware(), INTUOS5, None, false, THUMB)
                    .err(),
                Some(ClaimRefusal::NotThisModel),
                "{malformed:?}"
            );
        }
        assert_eq!(
            journal
                .begin(INSTANCE, &hardware(), INTUOS5, None, true, THUMB)
                .err(),
            Some(ClaimRefusal::AlreadyClaimed)
        );
        assert_eq!(
            journal
                .begin(
                    INSTANCE,
                    &hardware(),
                    INTUOS5,
                    Some(PriorDriver {
                        inf: "..\\..\\evil.inf".to_owned()
                    }),
                    false,
                    THUMB
                )
                .err(),
            Some(ClaimRefusal::UntrustedPriorDriver)
        );
        claimed(&mut journal, INSTANCE);
        assert_eq!(
            journal
                .begin(
                    &INSTANCE.to_lowercase(),
                    &hardware(),
                    INTUOS5,
                    None,
                    false,
                    THUMB
                )
                .err(),
            Some(ClaimRefusal::InJournal)
        );
        let mut journal = ClaimJournal::default();
        assert!(
            journal
                .begin("ROOT\\USB\\0000", &hardware(), INTUOS5, None, false, THUMB)
                .is_ok(),
            "a stand-in that carries the model's hardware IDs is claimable"
        );
    }

    #[test]
    fn an_untrusted_published_name_falls_back_to_unstaging_the_model() {
        let mut journal = ClaimJournal::default();
        claimed(&mut journal, INSTANCE);
        journal.claims[0].published_inf = Some("..\\input.inf".to_owned());
        assert!(
            journal
                .recovery_plan()
                .contains(&ReleaseStep::UnstageModel { model: INTUOS5 })
        );
    }
}
