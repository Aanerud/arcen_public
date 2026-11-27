// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! `arcen-pier diagnose displays [--json]`: the console's displays as the
//! Pier reads them before and after a Deck (shared `display_truth`, ADR 0025
//! §14). Read-only; run as root on the host, like the Pier. It reads the
//! console's X11 desktop; a Wayland console is not read yet.

use std::process::ExitCode;

use arcen_outputs::display_truth::{parse_diagnose, DiagnoseRequest, DIAGNOSE_USAGE};

/// Runs the command; `args` follow `diagnose`.
#[must_use]
pub fn diagnose_main(args: &[String]) -> ExitCode {
    let json = match parse_diagnose(args) {
        Ok(DiagnoseRequest::Displays { json }) => json,
        Ok(DiagnoseRequest::Help) => {
            println!("USAGE: {DIAGNOSE_USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("{error}\nUSAGE: {DIAGNOSE_USAGE}");
            return ExitCode::from(2);
        }
    };
    let truth = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())
        .and_then(|runtime| runtime.block_on(super::console_layout::console_truth()));
    match truth {
        Ok(truth) => {
            print!("{}", truth.render(json));
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("diagnose displays: {error}");
            ExitCode::FAILURE
        }
    }
}
