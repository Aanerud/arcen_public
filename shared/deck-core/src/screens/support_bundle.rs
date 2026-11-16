// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Settings › Logs › "Create support bundle…": where to save it, what it
//! holds, and — once written — its path with "Copy path" and "Show in
//! folder". The same panel on both Decks; the shell only forwards
//! [`SupportBundleAction`]s to [`SupportBundlePanel::apply`].

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::theme;
use super::wording::DeckPlatform;

/// What the user asked of the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportBundleAction {
    /// "Create support bundle…": show the panel.
    Open,
    /// "Change…": pick another folder.
    ChooseFolder,
    /// "Create bundle".
    Create,
    /// "Include crash reports" ticked or cleared.
    ToggleCrashReports,
    /// "Copy path".
    CopyPath,
    /// "Show in folder".
    ShowInFolder,
    /// "Close".
    Close,
}

/// What the panel shows; built by [`SupportBundlePanel::view`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one flag per thing the panel shows"
)]
pub struct SupportBundleView {
    /// The panel is open.
    pub open: bool,
    /// The folder the bundle goes to.
    pub folder: String,
    /// The platform offers a folder dialog.
    pub can_choose: bool,
    /// A bundle is being written.
    pub busy: bool,
    /// The last bundle's path.
    pub saved: Option<String>,
    /// Why the last attempt failed.
    pub error: Option<String>,
    /// "Copy path" was used for [`Self::saved`].
    pub copied: bool,
    /// "Include crash reports" is ticked (off by default).
    pub include_crash_reports: bool,
}

type JobSlot = Arc<Mutex<Option<Result<PathBuf, String>>>>;

/// The panel's state; the shell keeps one for the process.
#[derive(Debug, Default)]
pub struct SupportBundlePanel {
    open: bool,
    folder: Option<PathBuf>,
    running: Option<JobSlot>,
    saved: Option<PathBuf>,
    error: Option<String>,
    copied: bool,
    include_crash_reports: bool,
}

/// What [`SupportBundlePanel::apply`] needs from the shell.
pub struct SupportBundleContext<'a> {
    /// The Deck's services.
    pub services: Arc<dyn crate::DeckServices>,
    /// The bundle's component (`arcen-deck-macos`/`arcen-deck-windows`,
    /// version, OS, arch).
    pub component: arcen_telemetry::BundleComponent,
    /// The log level the Deck runs at.
    pub log_level: crate::settings::LogLevel,
    /// The displays, read on the UI thread (only called for Create).
    pub displays: &'a dyn Fn() -> Vec<arcen_protocol::messages::ClientMonitor>,
    /// Wakes the UI when the bundle is written.
    pub wake: Arc<dyn Fn() + Send + Sync>,
    /// The UI's egui context (fallback for "Copy path").
    pub ctx: &'a egui::Context,
}

impl std::fmt::Debug for SupportBundleContext<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SupportBundleContext")
            .field("component", &self.component)
            .field("log_level", &self.log_level)
            .finish_non_exhaustive()
    }
}

impl SupportBundlePanel {
    /// The view for this pass (collects a finished bundle first).
    pub fn view(&mut self, services: &dyn crate::DeckServices) -> SupportBundleView {
        self.collect();
        if self.folder.is_none() {
            self.folder = crate::support_bundle::default_folder(services);
        }
        SupportBundleView {
            open: self.open,
            folder: self
                .folder
                .as_ref()
                .map(|folder| folder.display().to_string())
                .unwrap_or_default(),
            can_choose: services.can_choose_folder(),
            busy: self.running.is_some(),
            saved: self.saved.as_ref().map(|path| path.display().to_string()),
            error: self.error.clone(),
            copied: self.copied,
            include_crash_reports: self.include_crash_reports,
        }
    }

    /// Carries out one action. Call on the UI thread (the folder dialog and
    /// the clipboard are UI-thread services); the bundle itself is written
    /// on its own thread.
    pub fn apply(&mut self, action: SupportBundleAction, context: &SupportBundleContext<'_>) {
        match action {
            SupportBundleAction::Open => self.open = true,
            SupportBundleAction::Close => self.open = false,
            SupportBundleAction::ChooseFolder => {
                if let Some(folder) = context.services.choose_folder(self.folder.as_deref()) {
                    self.folder = Some(folder);
                }
            }
            SupportBundleAction::Create => self.start(context),
            SupportBundleAction::ToggleCrashReports => {
                self.include_crash_reports = !self.include_crash_reports;
            }
            SupportBundleAction::CopyPath => {
                if let Some(path) = &self.saved {
                    let text = path.display().to_string();
                    if !context.services.copy_text(&text) {
                        context.ctx.copy_text(text);
                    }
                    self.copied = true;
                }
            }
            SupportBundleAction::ShowInFolder => {
                if let Some(path) = &self.saved
                    && !context.services.reveal_in_file_browser(path)
                {
                    self.error = Some("Could not open the folder.".to_owned());
                }
            }
        }
    }

    fn start(&mut self, context: &SupportBundleContext<'_>) {
        if self.running.is_some() {
            return;
        }
        let Some(folder) = self.folder.clone() else {
            self.error = Some("Choose a folder first.".to_owned());
            return;
        };
        self.error = None;
        self.saved = None;
        self.copied = false;
        let displays = (context.displays)();
        let services = Arc::clone(&context.services);
        let component = context.component.clone();
        let log_level = context.log_level;
        let include_crash_reports = self.include_crash_reports;
        let wake = Arc::clone(&context.wake);
        let slot: JobSlot = Arc::new(Mutex::new(None));
        let result = Arc::clone(&slot);
        let spawned = std::thread::Builder::new()
            .name("arcen-support-bundle".to_owned())
            .spawn(move || {
                let outcome = crate::support_bundle::run(
                    services.as_ref(),
                    displays,
                    component,
                    log_level,
                    folder,
                    include_crash_reports,
                )
                .map(|outcome| outcome.path)
                .map_err(|error| error.to_string());
                if let Ok(mut result) = result.lock() {
                    *result = Some(outcome);
                }
                wake();
            });
        match spawned {
            Ok(_) => self.running = Some(slot),
            Err(error) => self.error = Some(format!("Could not start: {error}")),
        }
    }

    fn collect(&mut self) {
        let Some(slot) = &self.running else {
            return;
        };
        let Some(result) = slot.lock().ok().and_then(|mut result| result.take()) else {
            return;
        };
        self.running = None;
        // Crash reports are asked for once, for one bundle.
        self.include_crash_reports = false;
        match result {
            Ok(path) => self.saved = Some(path),
            Err(error) => {
                tracing::warn!(target: crate::log_target::UI, %error, "support bundle failed");
                self.error = Some(error);
            }
        }
    }
}

/// Why crash reports are off unless ticked.
pub const CRASH_REPORTS_HINT: &str = "A crash report can hold fragments of what Arcen Deck was \
     doing when it crashed, such as a password being typed. Leave this off unless someone \
     helping you asks for it.";

/// What the bundle holds, in the user's words.
#[must_use]
pub fn contents_description(platform: DeckPlatform) -> String {
    format!(
        "A .zip for whoever helps you, and for Arcen's own debugging: Arcen Deck's logs at the \
         level above (the last {days} days), settings without secrets, saved connections \
         without passwords, trusted certificate fingerprints, the build, {this}'s system, GPU, \
         display and decoder facts, the network path to your last host, the last session's \
         statistics and the Native tablet component's state; recent Arcen Deck crash reports \
         only if you tick the box below.\n\n\
         Never included: passwords, private keys, clipboard contents or typed text. Usernames, \
         host names and addresses that Arcen logs or saves are replaced by codes that differ \
         in every bundle, and {this}'s home folder and account name are removed. Connection \
         names stay as you typed them. Look inside before you share it.",
        days = crate::support_bundle::LOG_FILES_KEPT,
        this = platform.this_machine(),
    )
}

/// Draws the panel below the Logs controls and returns the actions asked.
pub fn render_support_bundle_panel(
    ui: &mut egui::Ui,
    view: &SupportBundleView,
    platform: DeckPlatform,
) -> Vec<SupportBundleAction> {
    let mut actions = Vec::new();
    if !view.open {
        return actions;
    }
    ui.add_space(14.0);
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.label(
            egui::RichText::new("Support bundle")
                .color(egui::Color32::BLACK)
                .size(19.0),
        );
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(contents_description(platform)).color(egui::Color32::DARK_GRAY),
        );
        ui.add_space(10.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("Save in").color(egui::Color32::DARK_GRAY));
            ui.label(egui::RichText::new(&view.folder).monospace());
            if view.can_choose
                && ui
                    .add_enabled(!view.busy, egui::Button::new("Change…"))
                    .clicked()
            {
                actions.push(SupportBundleAction::ChooseFolder);
            }
        });
        ui.add_space(6.0);
        let mut include = view.include_crash_reports;
        if ui
            .add_enabled(
                !view.busy,
                egui::Checkbox::new(&mut include, "Include crash reports"),
            )
            .on_hover_text(CRASH_REPORTS_HINT)
            .changed()
        {
            actions.push(SupportBundleAction::ToggleCrashReports);
        }
        ui.label(
            egui::RichText::new(CRASH_REPORTS_HINT)
                .small()
                .color(egui::Color32::DARK_GRAY),
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!view.busy, egui::Button::new("Create bundle"))
                .clicked()
            {
                actions.push(SupportBundleAction::Create);
            }
            if ui.button("Close").clicked() {
                actions.push(SupportBundleAction::Close);
            }
            if view.busy {
                ui.spinner();
                ui.label(
                    egui::RichText::new("Creating the support bundle…")
                        .color(egui::Color32::DARK_GRAY),
                );
            }
        });
        if let Some(saved) = &view.saved {
            ui.add_space(8.0);
            ui.label(egui::RichText::new("Saved").color(theme::ACCENT));
            ui.add(egui::Label::new(egui::RichText::new(saved).monospace()).selectable(true));
            ui.horizontal(|ui| {
                if ui.button("Copy path").clicked() {
                    actions.push(SupportBundleAction::CopyPath);
                }
                if ui.button("📂  Show in folder").clicked() {
                    actions.push(SupportBundleAction::ShowInFolder);
                }
                if view.copied {
                    ui.label(egui::RichText::new("Path copied.").color(egui::Color32::DARK_GRAY));
                }
            });
        }
        if let Some(error) = &view.error {
            ui.add_space(6.0);
            ui.label(egui::RichText::new(error).color(theme::DANGER));
        }
    });
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_panel_draws_every_state_and_says_what_is_never_included() {
        for platform in [DeckPlatform::Mac, DeckPlatform::Pc] {
            let text = contents_description(platform);
            assert!(text.contains("Never included: passwords, private keys, clipboard"));
            assert!(text.contains(platform.this_machine()));
            for view in [
                SupportBundleView::default(),
                SupportBundleView {
                    open: true,
                    folder: "/Users/user/Desktop".to_owned(),
                    can_choose: true,
                    ..SupportBundleView::default()
                },
                SupportBundleView {
                    open: true,
                    busy: true,
                    ..SupportBundleView::default()
                },
                SupportBundleView {
                    open: true,
                    saved: Some("/Users/user/Desktop/arcen-deck-support-1.zip".to_owned()),
                    copied: true,
                    error: Some("Could not open the folder.".to_owned()),
                    ..SupportBundleView::default()
                },
            ] {
                let ctx = egui::Context::default();
                let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                    assert!(render_support_bundle_panel(ui, &view, platform).is_empty());
                });
            }
        }
    }
}
