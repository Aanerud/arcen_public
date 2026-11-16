// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Carries out the shared claim journal (`arcen_usb_bridge::winusb_package`):
//! trust, stage, distrust and bind at claim; restore, unstage and distrust at
//! release; every step recorded before it runs, and whatever a crash left
//! undone at service start.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use arcen_usb_bridge::winusb_package::{
    ClaimJournal, PriorDriver, ReleaseStep, TabletModel, is_arcen_package, is_plain_inf_name,
    is_published_oem_inf,
};

use super::{devices, trust};

/// A prepared, signed package for one tablet model, as `arcen-usb-prepare`
/// left it under the component's directory.
#[derive(Debug, Clone)]
pub(crate) struct Package {
    pub(crate) model: TabletModel,
    pub(crate) inf: PathBuf,
    pub(crate) certificate: Vec<u8>,
    pub(crate) thumbprint: String,
}

impl Package {
    /// Loads the package for `model` from `packages`.
    pub(crate) fn load(packages: &Path, model: TabletModel) -> Result<Self, String> {
        let name = model.package_name();
        let dir = packages.join(&name);
        let inf = dir.join(format!("{name}.inf"));
        let cat = dir.join(format!("{name}.cat"));
        if !inf.is_file() || !cat.is_file() {
            return Err(format!("{name} is not prepared"));
        }
        let cer = dir.join(format!("{name}.cer"));
        // Everything that leads to machine trust or a driver install must
        // come from an administrator (`arcen-usb-prepare`), not a user.
        for path in [
            packages,
            dir.as_path(),
            inf.as_path(),
            cat.as_path(),
            cer.as_path(),
        ] {
            super::common::require_trusted(path)?;
        }
        // The INF may only ever bind the in-box WinUSB to this model.
        let text = read_utf16(&inf)?;
        let expected = arcen_usb_bridge::winusb_package::winusb_inf(
            model,
            arcen_usb_bridge::winusb_package::InfDate {
                year: 2000,
                month: 1,
                day: 1,
            },
        );
        if without_driver_ver(&text) != without_driver_ver(&expected) {
            return Err(format!(
                "{name}.inf is not the Arcen WinUSB INF for this model"
            ));
        }
        let certificate =
            std::fs::read(&cer).map_err(|error| format!("{name} certificate: {error}"))?;
        let thumbprint = trust::thumbprint(&certificate)?;
        Ok(Self {
            model,
            inf,
            certificate,
            thumbprint,
        })
    }

    /// Every model with a prepared package.
    pub(crate) fn prepared_models(packages: &Path) -> Vec<TabletModel> {
        let Ok(entries) = std::fs::read_dir(packages) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                TabletModel::from_package_name(&entry.file_name().to_string_lossy())
            })
            .collect()
    }
}

fn read_utf16(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let body = bytes
        .strip_prefix(&[0xff, 0xfe])
        .ok_or("the INF is not UTF-16LE")?;
    let units: Vec<u16> = body
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16(&units).map_err(|_| "the INF is not valid UTF-16".to_owned())
}

fn without_driver_ver(inf: &str) -> String {
    inf.lines()
        .filter(|line| !line.trim_start().starts_with("DriverVer"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The journal file and its write-ahead discipline.
#[derive(Debug, Clone)]
pub(crate) struct Journal {
    path: PathBuf,
}

impl Journal {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub(crate) fn load(&self) -> ClaimJournal {
        // A journal a user could have written is not acted on.
        if self.path.exists() && super::common::require_trusted(&self.path).is_err() {
            let _ = std::fs::rename(&self.path, self.path.with_extension("json.untrusted"));
            return ClaimJournal::default();
        }
        std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Writes the journal so that a crash never leaves a half-written file.
    pub(crate) fn save(&self, journal: &ClaimJournal) -> Result<(), String> {
        let text = serde_json::to_vec_pretty(journal).map_err(|error| error.to_string())?;
        let temporary = self.path.with_extension("json.new");
        std::fs::write(&temporary, text)
            .and_then(|()| std::fs::rename(&temporary, &self.path))
            .map_err(|error| format!("write claim journal {}: {error}", self.path.display()))
    }
}

/// How long Windows may take to restart the device on `WinUSB`.
const BIND_WAIT: Duration = Duration::from_secs(10);

/// Swaps one tablet instance to `WinUSB` and returns its `WinUSB` device
/// path. On any failure the claim is released again before returning.
pub(crate) fn claim(
    journal: &Journal,
    package: &Package,
    instance: &str,
) -> Result<String, String> {
    let prior = devices::driver_inf(instance)?;
    let prior_is_arcen = prior
        .as_deref()
        .and_then(devices::store_folder)
        .is_some_and(|folder| is_arcen_package(&folder));
    let hardware_ids = devices::hardware_ids(instance)?;
    let mut state = journal.load();
    state
        .begin(
            instance,
            &hardware_ids,
            package.model,
            prior.map(|inf| PriorDriver { inf }),
            prior_is_arcen,
            &package.thumbprint,
        )
        .map_err(|refusal| refusal.to_string())?;
    journal.save(&state)?;
    let outcome = bind(journal, &mut state, package, instance);
    if let Err(error) = outcome {
        let undo = release(journal, instance);
        return Err(match undo {
            Ok(()) => error,
            Err(undo) => format!("{error}; and putting the tablet back failed: {undo}"),
        });
    }
    let started = Instant::now();
    loop {
        if let Some(path) = devices::interface_path(&super::winusb::INTERFACE_GUID, instance) {
            return Ok(path);
        }
        if started.elapsed() > BIND_WAIT {
            let _ = release(journal, instance);
            return Err("the tablet did not come back on WinUSB".to_owned());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn bind(
    journal: &Journal,
    state: &mut ClaimJournal,
    package: &Package,
    instance: &str,
) -> Result<(), String> {
    let record =
        |state: &mut ClaimJournal,
         change: &dyn Fn(&mut arcen_usb_bridge::winusb_package::ClaimRecord)| {
            if let Some(claim) = state.record_mut(instance) {
                change(claim);
            }
            journal.save(state)
        };
    record(state, &|claim| claim.trust_added = true)?;
    trust::add(&package.certificate)?;
    record(state, &|claim| claim.staging = true)?;
    let staged = devices::stage(&package.inf);
    let removed = trust::remove(&package.thumbprint);
    let published = staged?;
    if !is_published_oem_inf(&published) {
        return Err(format!("staging returned an unexpected name {published}"));
    }
    record(state, &|claim| {
        claim.published_inf = Some(published.clone());
    })?;
    removed?;
    record(state, &|claim| claim.trust_added = false)?;
    record(state, &|claim| claim.bound = true)?;
    let inf = super::system_root()
        .ok_or("no Windows directory")?
        .join("INF")
        .join(&published);
    devices::install_on(instance, &inf)?;
    Ok(())
}

/// Puts one instance back as the journal says. Each step that succeeds is
/// recorded; the instance is forgotten only when nothing is left, so a
/// failed step is retried by the next recovery.
pub(crate) fn release(journal: &Journal, instance: &str) -> Result<(), String> {
    let mut state = journal.load();
    let plan = state.release_plan(instance);
    let errors = run(journal, &mut state, &plan);
    if errors.is_empty() {
        state.finish(instance);
    } else {
        state.prune();
    }
    journal.save(&state)?;
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Undoes whatever a previous run left: journalled claims first, then any
/// staged Arcen package and any Arcen certificate the journal does not know
/// about (a crash between a step and its record).
pub(crate) fn recover(journal: &Journal) -> Vec<String> {
    let mut state = journal.load();
    let mut log = Vec::new();
    let plan = state.recovery_plan();
    if !plan.is_empty() {
        log.push(format!(
            "recovering {} step(s) from the claim journal",
            plan.len()
        ));
        log.extend(run(journal, &mut state, &plan));
    }
    state.prune();
    if let Err(error) = journal.save(&state) {
        log.push(error);
    }
    let in_use: Vec<String> = state
        .claims
        .iter()
        .filter_map(|claim| claim.published_inf.clone())
        .collect();
    for published in
        devices::published_with_prefix(arcen_usb_bridge::winusb_package::PACKAGE_PREFIX)
    {
        if in_use
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&published))
        {
            continue;
        }
        log.push(match unstage(&published) {
            Ok(()) => format!("removed a leftover staged package {published}"),
            Err(error) => error,
        });
    }
    for thumbprint in trust::arcen_leftovers() {
        log.push(match trust::remove(&thumbprint) {
            Ok(()) => format!("removed a leftover Arcen certificate {thumbprint}"),
            Err(error) => format!("could not remove leftover certificate {thumbprint}: {error}"),
        });
    }
    log
}

/// Removes everything the component ever put on this PC — for the
/// installer's uninstall step, after the service has stopped: the journal's
/// recovery, every staged Arcen package (`/uninstall`, so instances get their
/// own driver back), every Arcen certificate in `Root` and
/// `TrustedPublisher`, and the component directory. Returns the log and the
/// leftovers found by re-checking each kind afterwards (empty = clean).
pub(crate) fn uninstall(journal: &Journal, root: &Path) -> (Vec<String>, Vec<String>) {
    let mut log = recover(journal);
    for published in
        devices::published_with_prefix(arcen_usb_bridge::winusb_package::PACKAGE_PREFIX)
    {
        log.push(match unstage(&published) {
            Ok(()) => format!("removed staged package {published}"),
            Err(error) => error,
        });
    }
    for thumbprint in trust::arcen_leftovers() {
        if let Err(error) = trust::remove(&thumbprint) {
            log.push(error);
        }
    }
    if root.exists() {
        log.push(match std::fs::remove_dir_all(root) {
            Ok(()) => format!("removed {}", root.display()),
            Err(error) => format!("remove {}: {error}", root.display()),
        });
    }
    let mut leftovers: Vec<String> =
        devices::published_with_prefix(arcen_usb_bridge::winusb_package::PACKAGE_PREFIX)
            .into_iter()
            .map(|published| format!("staged package {published}"))
            .collect();
    leftovers.extend(
        trust::arcen_leftovers()
            .into_iter()
            .map(|thumbprint| format!("trusted certificate {thumbprint}")),
    );
    if root.exists() {
        leftovers.push(format!("directory {}", root.display()));
    }
    (log, leftovers)
}

/// Runs release steps in order, recording each success in the journal and
/// continuing past failures so that the trust entries are always removed;
/// returns the failures.
fn run(journal: &Journal, state: &mut ClaimJournal, steps: &[ReleaseStep]) -> Vec<String> {
    let mut errors = Vec::new();
    for step in steps {
        let outcome = match step {
            ReleaseStep::RestoreDriver { instance_id, inf } => restore(instance_id, inf),
            ReleaseStep::UnstagePackage { published_inf } => unstage(published_inf),
            ReleaseStep::UnstageModel { model } => {
                let mut failures = Vec::new();
                for published in devices::published_with_prefix(&model.package_name()) {
                    if let Err(error) = unstage(&published) {
                        failures.push(error);
                    }
                }
                if failures.is_empty() {
                    Ok(())
                } else {
                    Err(failures.join("; "))
                }
            }
            ReleaseStep::Distrust { thumbprint } => trust::remove(thumbprint),
        };
        match outcome {
            Ok(()) => {
                state.mark_done(step);
                if let Err(error) = journal.save(state) {
                    errors.push(error);
                }
            }
            Err(error) => errors.push(error),
        }
    }
    errors
}

fn restore(instance: &str, inf: &str) -> Result<(), String> {
    if !is_plain_inf_name(inf) {
        return Err(format!("refusing to restore from {inf:?}"));
    }
    let path = super::system_root()
        .ok_or("no Windows directory")?
        .join("INF")
        .join(inf);
    match devices::install_on(instance, &path) {
        Ok(_) => Ok(()),
        // An unplugged tablet gets its own driver when it returns, because
        // the package is about to leave the store.
        Err(error) if error.contains("open device") => Ok(()),
        Err(error) => Err(error),
    }
}

/// `pnputil /delete-driver <oemN.inf> /uninstall`: removes the package and
/// reinstalls the next-best driver on any instance still using it.
fn unstage(published: &str) -> Result<(), String> {
    if !is_published_oem_inf(published) {
        return Err(format!("refusing to delete {published:?}"));
    }
    if !devices::store_folder(published).is_some_and(|folder| is_arcen_package(&folder)) {
        // Gone already, or not ours: never delete another vendor's package.
        return Ok(());
    }
    let pnputil = super::system_root()
        .ok_or("no Windows directory")?
        .join("System32")
        .join("pnputil.exe");
    let status = std::process::Command::new(pnputil)
        .args(["/delete-driver", published, "/uninstall"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|error| format!("run pnputil: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "pnputil /delete-driver {published} failed ({status})"
        ))
    }
}
