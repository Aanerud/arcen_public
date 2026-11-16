// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! **Test connection**: the panel is shared
//! (`arcen_deck_core::screens::connection_test`); this file runs the test off
//! the UI thread with this Deck's diagnosis.

pub use arcen_deck_core::screens::connection_test::{render, ConnectionTest, ConnectionTestAction};

use crate::transport::diagnose::{deck_label, run, DiagnoseRequest};

/// Starts testing `host` (which may carry `:port`) on `port`, or on the
/// Pier default when `port` is empty. Repaints `ctx` when done.
#[must_use]
pub fn start(host: &str, port: &str, ctx: &egui::Context) -> ConnectionTest {
    ConnectionTest::start(host, port, deck_label(), |target, slot| {
        let request = DiagnoseRequest::with_remembered_pin(target);
        let repaint = ctx.clone();
        let task = async move {
            slot.fill(run(request).await);
            repaint.request_repaint();
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(task);
            }
            Err(_) => {
                std::thread::spawn(move || {
                    if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        runtime.block_on(task);
                    }
                });
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreadable_address_reports_without_running() {
        let ctx = egui::Context::default();
        let test = start("bad host", "", &ctx);
        assert!(!test.running());
        assert!(test.input_error.is_some());
    }
}
