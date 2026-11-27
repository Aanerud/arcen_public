// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The change-of-account question and its progress, over whatever screen
//! the shell shows (Connecting, Reconnecting). Both Decks draw this; the
//! state and the words are [`crate::account_switch`].

use arcen_protocol::messages::AccountSwitchChoiceMsg;

use super::widgets::{button_width, paint_connecting_progress, primary_button, secondary_button};
use crate::account_switch::{
    AccountSwitchAction, AccountSwitchPrompt, AccountSwitchView, CANCEL, CONFIRM, QUESTION, TITLE,
};

/// Draws the prompt's view as a modal and applies the user's action.
/// Returns the answer for the shell to send
/// (`SessionCommand::AccountSwitch`), once.
///
/// Cancel is the default: it takes the focus when the question opens, so
/// Enter answers Cancel, and Escape or a click outside cancels too. "Sign
/// out and continue" takes no click or key for
/// [`crate::account_switch::CONFIRM_ARMING_DELAY`] after the question
/// appears: the host decides when that is, and a click meant for the
/// screen under it must not sign anyone out.
pub fn render_account_switch(
    ctx: &egui::Context,
    prompt: &mut AccountSwitchPrompt,
) -> Option<AccountSwitchChoiceMsg> {
    render_at(ctx, prompt, std::time::Instant::now())
}

fn render_at(
    ctx: &egui::Context,
    prompt: &mut AccountSwitchPrompt,
    now: std::time::Instant,
) -> Option<AccountSwitchChoiceMsg> {
    if let Some(choice) = prompt.expire(now) {
        return Some(choice);
    }
    let focus = prompt.take_focus();
    let action = match prompt.view()? {
        AccountSwitchView::Question => question(ctx, focus),
        AccountSwitchView::Progress(text) => {
            progress(ctx, text);
            AccountSwitchAction::None
        }
    };
    prompt.answer(action, now)
}

fn question(ctx: &egui::Context, focus_cancel: bool) -> AccountSwitchAction {
    let mut action = AccountSwitchAction::None;
    let modal = egui::Modal::new(egui::Id::new("account_switch_question")).show(ctx, |ui| {
        let colours = super::theme::colours(ui);
        ui.set_max_width(420.0);
        ui.label(
            egui::RichText::new(TITLE)
                .color(colours.text)
                .size(super::theme::TEXT_BODY + 4.0)
                .strong(),
        );
        ui.add_space(10.0);
        ui.add(
            egui::Label::new(
                egui::RichText::new(QUESTION)
                    .color(colours.text)
                    .size(super::theme::TEXT_BODY),
            )
            .wrap(),
        );
        ui.add_space(16.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = super::theme::BUTTON_GAP;
            // Right to left: Cancel, the default and the one filled
            // button, sits rightmost; the destructive choice is the plain one.
            let cancel = primary_button(ui, CANCEL, button_width(ui, &[CANCEL]));
            if focus_cancel {
                cancel.request_focus();
            }
            if cancel.clicked() {
                action = AccountSwitchAction::Cancel;
            }
            if secondary_button(ui, CONFIRM, button_width(ui, &[CONFIRM])).clicked() {
                action = AccountSwitchAction::Confirm;
            }
        });
    });
    if action == AccountSwitchAction::None && modal.should_close() {
        action = AccountSwitchAction::Cancel;
    }
    // Wake to close the question when the offer expires.
    ctx.request_repaint_after(std::time::Duration::from_secs(1));
    action
}

fn progress(ctx: &egui::Context, text: &str) {
    let _ = egui::Modal::new(egui::Id::new("account_switch_progress")).show(ctx, |ui| {
        let colours = super::theme::colours(ui);
        ui.set_max_width(420.0);
        ui.horizontal(|ui| {
            paint_connecting_progress(ui, 18.0);
            ui.add_space(10.0);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(text)
                        .color(colours.text)
                        .size(super::theme::TEXT_BODY),
                )
                .wrap(),
            );
        });
    });
    ctx.request_repaint();
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::account_switch::{AccountSwitchNews, SIGNING_IN, SIGNING_OUT};

    /// The Decks' default and minimum window sizes, in points.
    const SIZES: [[f32; 2]; 2] = [[1120.0, 760.0], [860.0, 600.0]];

    fn input(size: [f32; 2], events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(size[0], size[1]),
            )),
            events,
            ..egui::RawInput::default()
        }
    }

    fn key(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn asking() -> AccountSwitchPrompt {
        let mut prompt = AccountSwitchPrompt::default();
        prompt.on_news(
            AccountSwitchNews::Offered {
                expires_in: Duration::from_secs(120),
            },
            Instant::now(),
        );
        prompt
    }

    fn texts(output: &egui::FullOutput) -> Vec<(String, egui::Rect)> {
        fn walk(shape: &egui::Shape, out: &mut Vec<(String, egui::Rect)>) {
            match shape {
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| walk(shape, out)),
                egui::Shape::Text(text) => {
                    let rect = text.galley.rect.translate(text.pos.to_vec2());
                    out.push((text.galley.text().to_owned(), rect));
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        for clipped in &output.shapes {
            walk(&clipped.shape, &mut out);
        }
        out
    }

    /// Runs passes until the modal has faded in; returns the last output
    /// and every answer given.
    fn run(
        ctx: &egui::Context,
        prompt: &mut AccountSwitchPrompt,
        size: [f32; 2],
        events_on_last: Vec<egui::Event>,
    ) -> (egui::FullOutput, Vec<AccountSwitchChoiceMsg>) {
        let mut answers = Vec::new();
        let mut output = None;
        for pass in 0..30 {
            let events = if pass == 29 {
                events_on_last.clone()
            } else {
                Vec::new()
            };
            output = Some(ctx.run_ui(input(size, events), |ui| {
                answers.extend(render_account_switch(ui.ctx(), prompt));
            }));
        }
        (output.unwrap(), answers)
    }

    #[test]
    fn the_question_fits_both_window_sizes_and_names_nobody() {
        for size in SIZES {
            let ctx = egui::Context::default();
            super::super::theme::apply(&ctx);
            let mut prompt = asking();
            let (output, answers) = run(&ctx, &mut prompt, size, Vec::new());
            assert!(answers.is_empty());
            let shown = texts(&output);
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(size[0], size[1]));
            for wanted in [TITLE, CANCEL, CONFIRM] {
                let (_, rect) = shown
                    .iter()
                    .find(|(text, _)| text == wanted)
                    .unwrap_or_else(|| panic!("{wanted} at {size:?}"));
                assert!(
                    screen.contains_rect(*rect),
                    "{wanted} at {size:?}: {rect:?}"
                );
            }
            let body: String = shown.iter().map(|(text, _)| text.as_str()).collect();
            assert!(body.contains("Signing them out will close their"), "{body}");
            let cancel = shown.iter().find(|(text, _)| text == CANCEL).unwrap().1;
            let confirm = shown.iter().find(|(text, _)| text == CONFIRM).unwrap().1;
            assert!(cancel.left() > confirm.right(), "Cancel sits rightmost");
            assert_eq!(prompt.view(), Some(AccountSwitchView::Question));
        }
    }

    #[test]
    fn enter_on_the_default_and_escape_both_cancel() {
        for pressed in [egui::Key::Enter, egui::Key::Escape] {
            let ctx = egui::Context::default();
            super::super::theme::apply(&ctx);
            let mut prompt = asking();
            let (_, answers) = run(&ctx, &mut prompt, SIZES[1], vec![key(pressed)]);
            assert_eq!(answers, vec![AccountSwitchChoiceMsg::Cancel], "{pressed:?}");
            assert_eq!(prompt.view(), None);
        }
    }

    #[test]
    fn the_progress_offers_no_choice_and_escape_does_nothing() {
        for (news, text) in [
            (AccountSwitchNews::SigningOut, SIGNING_OUT),
            (AccountSwitchNews::SignedOut, SIGNING_IN),
        ] {
            let ctx = egui::Context::default();
            super::super::theme::apply(&ctx);
            let mut prompt = AccountSwitchPrompt::default();
            prompt.on_news(news, Instant::now());
            let (output, answers) = run(&ctx, &mut prompt, SIZES[1], vec![key(egui::Key::Escape)]);
            assert!(answers.is_empty());
            let shown = texts(&output);
            assert!(shown.iter().any(|(shown, _)| shown == text), "{text}");
            assert!(
                !shown
                    .iter()
                    .any(|(shown, _)| shown == CANCEL || shown == CONFIRM)
            );
            assert_eq!(prompt.view(), Some(AccountSwitchView::Progress(text)));
        }
    }

    /// One click (press, release, a quiet pass) at `pos`, the shell's clock
    /// reading `at`; every answer given.
    fn click_at(
        ctx: &egui::Context,
        prompt: &mut AccountSwitchPrompt,
        pos: egui::Pos2,
        at: Instant,
    ) -> Vec<AccountSwitchChoiceMsg> {
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let mut answers = Vec::new();
        for events in [
            vec![egui::Event::PointerMoved(pos), button(true)],
            vec![button(false)],
            Vec::new(),
        ] {
            let _ = ctx.run_ui(input(SIZES[1], events), |ui| {
                answers.extend(render_at(ui.ctx(), prompt, at));
            });
        }
        answers
    }

    #[test]
    fn a_click_on_sign_out_and_continue_counts_only_once_it_is_armed() {
        use crate::account_switch::CONFIRM_ARMING_DELAY;
        let ctx = egui::Context::default();
        super::super::theme::apply(&ctx);
        let opened = Instant::now();
        let mut prompt = AccountSwitchPrompt::default();
        prompt.on_news(
            AccountSwitchNews::Offered {
                expires_in: Duration::from_secs(120),
            },
            opened,
        );
        let mut output = None;
        for _ in 0..30 {
            output = Some(ctx.run_ui(input(SIZES[1], Vec::new()), |ui| {
                assert_eq!(render_at(ui.ctx(), &mut prompt, opened), None);
            }));
        }
        let confirm = texts(&output.unwrap())
            .into_iter()
            .find(|(text, _)| text == CONFIRM)
            .unwrap()
            .1
            .center();
        let early = opened + CONFIRM_ARMING_DELAY - Duration::from_millis(100);
        assert!(click_at(&ctx, &mut prompt, confirm, early).is_empty());
        assert_eq!(prompt.view(), Some(AccountSwitchView::Question));
        assert_eq!(
            click_at(&ctx, &mut prompt, confirm, opened + CONFIRM_ARMING_DELAY),
            vec![AccountSwitchChoiceMsg::Confirm]
        );
    }
}
