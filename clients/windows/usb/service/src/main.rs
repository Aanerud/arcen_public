// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! `arcen-usb-service.exe`: the Windows Deck's optional Arcen USB component
//! (ADR 0024).
//!
//! A `LocalSystem` service linking no UI, transport or media code. When the
//! verified Deck connects on its named pipe it swaps the attached, prepared
//! tablet to Microsoft's in-box `WinUSB` (per session, with transient trust),
//! re-runs the shared tablet admission on descriptors it read itself, serves
//! the Deck the same frames as the macOS helper, and puts the tablet's own
//! driver back when the Deck goes away. A claim journal undoes whatever a
//! crash left at the next start. Off Windows this is an empty program.
//!
//! ```text
//! arcen-usb-service                      run under the service control manager
//! arcen-usb-service console [--allow-client EXE]... [--emulate] [--lab-any-session]
//! arcen-usb-service recover              undo a crashed claim, then exit
//! arcen-usb-service uninstall            remove every trace (installer; service stopped)
//! arcen-usb-service status               prepared models, attached tablets, journal
//! arcen-usb-service claim-probe ID       claim one instance, read it, release it
//! ```

#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod codec;
#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    windows_main::main()
}

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    eprintln!("arcen-usb-service runs on Windows only");
    std::process::ExitCode::FAILURE
}

#[cfg(windows)]
mod windows_main {
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::sync::atomic::AtomicBool;

    use arcen_usb_bridge::winusb_package::TabletModel;
    use arcen_usb_bridge::{CapturedUsbDevice, UsbExporter};

    use crate::win::claim::{self, Package};
    use crate::win::runtime::{self, Config};
    use crate::win::{devices, scm, secure_component_root, winusb};

    pub(crate) fn main() -> ExitCode {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if args.is_empty() {
            if scm::dispatch() {
                return ExitCode::SUCCESS;
            }
            eprintln!("arcen-usb-service: not started by the service control manager; see --help");
            return ExitCode::FAILURE;
        }
        let root = match secure_component_root() {
            Ok(root) => root,
            Err(error) => {
                eprintln!("arcen-usb-service: {error} (run elevated)");
                return ExitCode::FAILURE;
            }
        };
        let outcome = match args[0].as_str() {
            "console" => console(root, &args[1..]),
            "recover" => {
                for line in claim::recover(&Config::installed(root).journal()) {
                    println!("{line}");
                }
                Ok(())
            }
            "uninstall" => {
                let config = Config::installed(root.clone());
                let (log, leftovers) = claim::uninstall(&config.journal(), &root);
                for line in log {
                    println!("{line}");
                }
                for leftover in &leftovers {
                    println!("LEFTOVER {leftover}");
                }
                // The installer discards stdout: on failure, leave the list
                // where a supporter finds it after a rolled-back uninstall.
                let report = crate::win::uninstall_report_path();
                if leftovers.is_empty() {
                    let _ = std::fs::remove_file(&report);
                    println!("clean: no package, certificate or state of the component is left");
                    Ok(())
                } else {
                    let text: String = leftovers
                        .iter()
                        .flat_map(|leftover| ["LEFTOVER ", leftover.as_str(), "\r\n"])
                        .collect();
                    let _ = std::fs::write(&report, text);
                    println!("leftovers written to {}", report.display());
                    Err(format!("{} leftover(s)", leftovers.len()))
                }
            }
            "status" => status(&Config::installed(root)),
            "claim-probe" => match args.get(1) {
                Some(instance) => claim_probe(&Config::installed(root), instance),
                None => Err("claim-probe needs a device instance ID".to_owned()),
            },
            _ => Err(format!(
                "unknown command {:?}; commands: console, recover, uninstall, status, claim-probe",
                args[0]
            )),
        };
        match outcome {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("arcen-usb-service: {error}");
                ExitCode::FAILURE
            }
        }
    }

    fn console(root: PathBuf, args: &[String]) -> Result<(), String> {
        let mut config = Config::installed(root);
        config.echo = true;
        let mut allowed = Vec::new();
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--allow-client" => {
                    allowed.push(PathBuf::from(
                        iter.next().ok_or("--allow-client needs a path")?,
                    ));
                }
                "--emulate" => config.emulate = true,
                #[cfg(feature = "lab-emulate")]
                "--lab-any-session" => config.policy.any_session = true,
                other => return Err(format!("unknown console option {other:?}")),
            }
        }
        if !allowed.is_empty() {
            config.policy.allowed_images = allowed;
        }
        let stop = AtomicBool::new(false);
        runtime::run(&config, &stop);
        Ok(())
    }

    fn status(config: &Config) -> Result<(), String> {
        let models = Package::prepared_models(&config.packages());
        println!("prepared models: {}", models.len());
        for model in &models {
            println!("  {}", model.hardware_id());
        }
        for instance in devices::present_usb_instances()? {
            if let Some(model) = TabletModel::from_device_id(&instance) {
                println!(
                    "attached {instance} prepared={} driver={}",
                    models.contains(&model),
                    devices::driver_inf(&instance)?.unwrap_or_else(|| "(none)".to_owned())
                );
            }
        }
        println!("journal: {:?}", config.journal().load());
        Ok(())
    }

    /// Claims one instance, reads it through `WinUSB` and the shared
    /// admission, and releases it: the lab proof of one whole swap.
    fn claim_probe(config: &Config, instance: &str) -> Result<(), String> {
        let model = devices::hardware_ids(instance)?
            .iter()
            .find_map(|id| TabletModel::from_device_id(id))
            .ok_or("the device's hardware IDs name no USB device model")?;
        let package = Package::load(&config.packages(), model)?;
        println!("prior driver: {:?}", devices::driver_inf(instance)?);
        let started = std::time::Instant::now();
        let journal = config.journal();
        let path = claim::claim(&journal, &package, instance)?;
        println!("claimed in {} ms: {path}", started.elapsed().as_millis());
        let hook_journal = journal.clone();
        let hook_instance = instance.to_owned();
        let device = winusb::WinUsbDevice::open(
            &path,
            (model.vendor_id, model.product_id),
            Box::new(move |_| claim::release(&hook_journal, &hook_instance)),
        );
        match device {
            Ok(device) => {
                println!("facts: {:?}", device.facts().snapshot.id);
                match UsbExporter::new(Box::new(device)) {
                    Ok(exporter) => {
                        println!("admitted by the shared policy");
                        drop(exporter);
                    }
                    Err((refusal, outcome)) => {
                        println!("refused by the shared policy: {refusal} ({outcome:?})");
                    }
                }
            }
            Err(error) => {
                println!("open failed: {error}");
                claim::release(&journal, instance)?;
            }
        }
        println!(
            "released after {} ms; driver now: {:?}",
            started.elapsed().as_millis(),
            devices::driver_inf(instance)?
        );
        Ok(())
    }
}
