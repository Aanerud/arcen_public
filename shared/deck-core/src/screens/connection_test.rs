// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! **Test connection**: the report panel of the connection editor. The
//! steps, findings and wording come from
//! `arcen_session::connection_diagnostics`; the shell runs the test off the
//! UI thread (it needs the session runtime) and fills the [`ReportSlot`].

use std::sync::{Arc, Mutex};

use arcen_session::connection_diagnostics::{DiagnosticReport, DiagnosticTarget, Verdict};

/// A test in flight or finished.
#[derive(Debug)]
pub struct ConnectionTest {
    /// What was typed, for the heading.
    pub target_label: String,
    /// Why the test could not start (an unreadable address).
    pub input_error: Option<String>,
    deck_label: String,
    pending: ReportSlot,
    report: Option<DiagnosticReport>,
    /// The report was copied to the clipboard.
    pub copied: bool,
}

/// Where a running test leaves its report for the UI to pick up.
#[derive(Clone, Debug, Default)]
pub struct ReportSlot(Arc<Mutex<Option<DiagnosticReport>>>);

impl ReportSlot {
    /// Hands the finished report to the UI (seen on its next [`ConnectionTest::poll`]).
    pub fn fill(&self, report: DiagnosticReport) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(report);
    }

    fn take(&self) -> Option<DiagnosticReport> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

/// What the user asked for in the report panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionTestAction {
    Copy,
    RunAgain,
    Close,
}

impl ConnectionTest {
    /// Starts testing `host` (which may carry `:port`) on `port`, or on the
    /// Pier default when `port` is empty. `deck_label` names this Deck in
    /// the copied report. `run` starts the test for a readable address and
    /// fills the slot when done (and wakes the UI); it is not called for an
    /// unreadable one.
    #[must_use]
    pub fn start(
        host: &str,
        port: &str,
        deck_label: String,
        run: impl FnOnce(DiagnosticTarget, ReportSlot),
    ) -> Self {
        let port = port.trim();
        let parsed = match port {
            "" => DiagnosticTarget::parse(host),
            value => match value.parse::<u16>() {
                Ok(port) if port != 0 => DiagnosticTarget::parse_with_port(host, Some(port)),
                _ => Err(arcen_session::connection_diagnostics::TargetError::InvalidPort),
            },
        };
        let pending = ReportSlot::default();
        let target = match parsed {
            Ok(target) => target,
            Err(error) => {
                return Self {
                    target_label: host.trim().to_owned(),
                    input_error: Some(error.to_string()),
                    deck_label,
                    pending,
                    report: None,
                    copied: false,
                };
            }
        };
        let target_label = target.display();
        run(target, pending.clone());
        Self {
            target_label,
            input_error: None,
            deck_label,
            pending,
            report: None,
            copied: false,
        }
    }

    /// Picks up a finished report.
    pub fn poll(&mut self) {
        if self.report.is_none() {
            self.report = self.pending.take();
        }
    }

    /// Whether the test is still running.
    #[must_use]
    pub fn running(&self) -> bool {
        self.input_error.is_none() && self.report.is_none()
    }

    /// The finished report.
    #[must_use]
    pub const fn report(&self) -> Option<&DiagnosticReport> {
        self.report.as_ref()
    }

    /// The finished test's verdict in a few words, for beside Test
    /// connection: the report's first sentence ("Ready to sign in.",
    /// "Cannot connect."), or why the test could not start.
    #[must_use]
    pub fn verdict_line(&self) -> Option<(String, Verdict)> {
        if let Some(error) = &self.input_error {
            return Some((error.clone(), Verdict::Fail));
        }
        let report = self.report.as_ref()?;
        let summary = report.summary();
        let first = summary
            .split_inclusive(". ")
            .next()
            .unwrap_or(&summary)
            .trim_end();
        Some((first.to_owned(), report.overall()))
    }

    /// The text copied to the clipboard.
    #[must_use]
    pub fn report_text(&self) -> Option<String> {
        self.report
            .as_ref()
            .map(|report| report.render_text(&self.deck_label))
    }
}

/// The ink a verdict is written in.
pub(super) fn verdict_colour(colours: &super::theme::Palette, verdict: Verdict) -> egui::Color32 {
    match verdict {
        Verdict::Pass => colours.success,
        Verdict::Info => colours.info,
        Verdict::Skipped => colours.text2,
        Verdict::Warning => colours.warning,
        Verdict::Fail => colours.error,
    }
}

/// Draws the report panel.
pub fn render(ui: &mut egui::Ui, test: &ConnectionTest) -> Option<ConnectionTestAction> {
    let mut action = None;
    egui::Frame::new()
        .fill(super::theme::colours(ui).panel)
        .stroke(egui::Stroke::new(1.0, super::theme::colours(ui).border))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(format!("Connection test — {}", test.target_label))
                    .strong()
                    .size(14.0)
                    .color(super::theme::colours(ui).text),
            );
            ui.label(
                egui::RichText::new("No user name or password is sent.")
                    .size(11.0)
                    .color(super::theme::colours(ui).text2),
            );
            // The same secondary buttons as the rest of Home.
            ui.horizontal(|ui| {
                use super::widgets::{button_width, secondary_button};
                ui.spacing_mut().item_spacing.x = super::theme::BUTTON_GAP;
                let width = button_width(ui, &["Copy report", "Run again", "Close"]);
                if test.report().is_some() {
                    let label = if test.copied { "Copied" } else { "Copy report" };
                    if secondary_button(ui, label, width).clicked() {
                        action = Some(ConnectionTestAction::Copy);
                    }
                }
                if !test.running() && secondary_button(ui, "Run again", width).clicked() {
                    action = Some(ConnectionTestAction::RunAgain);
                }
                if secondary_button(ui, "Close", width).clicked() {
                    action = Some(ConnectionTestAction::Close);
                }
            });
            ui.add_space(8.0);
            if let Some(error) = &test.input_error {
                ui.label(
                    egui::RichText::new(error)
                        .color(verdict_colour(&super::theme::colours(ui), Verdict::Fail))
                        .size(13.0),
                );
            } else if let Some(report) = test.report() {
                ui.label(
                    egui::RichText::new(report.summary())
                        .strong()
                        .size(13.0)
                        .color(verdict_colour(&super::theme::colours(ui), report.overall())),
                );
                ui.add_space(8.0);
                for step in &report.steps {
                    let verdict = step.verdict();
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new(verdict.label())
                                .monospace()
                                .strong()
                                .size(11.0)
                                .color(verdict_colour(&super::theme::colours(ui), verdict)),
                        );
                        ui.label(
                            egui::RichText::new(format!(
                                "{}. {}",
                                step.step.number(),
                                step.step.title()
                            ))
                            .strong()
                            .size(12.5),
                        );
                    });
                    ui.label(egui::RichText::new(step.finding.meaning()).size(12.0));
                    for detail in &step.details {
                        ui.label(
                            egui::RichText::new(detail)
                                .monospace()
                                .size(10.5)
                                .color(super::theme::colours(ui).text2),
                        );
                    }
                    let advice = step.finding.action();
                    if !advice.is_empty() && verdict != Verdict::Pass {
                        ui.label(
                            egui::RichText::new(format!("What to do: {advice}"))
                                .size(11.5)
                                .color(verdict_colour(&super::theme::colours(ui), verdict)),
                        );
                    }
                    ui.label(
                        egui::RichText::new(step.finding.code())
                            .monospace()
                            .size(9.5)
                            .color(super::theme::colours(ui).text2),
                    );
                    ui.add_space(6.0);
                }
            } else {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Testing name, UDP 18444, certificate, greeting, MTU and round trip…");
                });
            }
        });
    action
}

/// A finished test of `host`: the name resolves and the Pier does not
/// answer.
#[cfg(test)]
pub(super) fn unanswered(host: &str) -> ConnectionTest {
    use arcen_session::connection_diagnostics::{
        AddressAttempt, ExpectedIdentity, HandshakeOutcome, Observations, PathObservation,
        ResolutionObservation, RoundTripObservation, assess,
    };
    let mut test = ConnectionTest::start(host, "", String::new(), |target, slot| {
        let address = std::net::IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 11));
        let observations = Observations {
            resolution: ResolutionObservation::Resolved {
                addresses: vec![address],
                elapsed_ms: 4,
            },
            attempts: vec![AddressAttempt {
                address: std::net::SocketAddr::new(address, target.port),
                outcome: HandshakeOutcome::TimedOut { waited_ms: 5_000 },
            }],
            expected_identity: ExpectedIdentity::Unknown,
            certificate: None,
            greeting: None,
            path: PathObservation {
                interface_kind: None,
                interface_mtu: None,
                quic_mtu: None,
            },
            round_trip: RoundTripObservation::default(),
            target,
        };
        slot.fill(assess(
            &observations,
            1_790_000_000,
            arcen_telemetry::QosTargets::default(),
        ));
    });
    test.poll();
    test
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_verdict_line_is_the_reports_first_sentence() {
        let test = unanswered("pier.example.internal");
        assert!(!test.running());
        assert_eq!(
            test.verdict_line(),
            Some(("Cannot connect.".to_owned(), Verdict::Fail))
        );
        let unreadable = ConnectionTest::start("bad host", "", String::new(), |_, _| {});
        let (line, verdict) = unreadable.verdict_line().expect("a verdict");
        assert_eq!(Some(line.as_str()), unreadable.input_error.as_deref());
        assert_eq!(verdict, Verdict::Fail);
        let running = ConnectionTest::start("pier.example.internal", "", String::new(), |_, _| {});
        assert_eq!(running.verdict_line(), None);
    }

    #[test]
    fn an_unreadable_address_reports_without_running() {
        let start = |host: &str, port: &str| {
            ConnectionTest::start(host, port, String::new(), |_, _| {
                panic!("an unreadable address must not run")
            })
        };
        let test = start("bad host", "");
        assert!(!test.running());
        assert!(test.input_error.is_some());
        let test = start("pier.example.internal", "0");
        assert_eq!(
            test.input_error.as_deref(),
            Some("the port must be a number from 1 to 65535")
        );
    }
}
