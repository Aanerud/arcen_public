// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The overlays over a held frame: "Lost connection, attempting to
//! reconnect" (and resuming), and "Signing in…" while the Deck follows a
//! host's sign-in screen into the desktop. The shell says what is under the
//! overlay and maps Disconnect.

/// What stays visible under the overlay.
#[derive(Debug)]
pub enum HeldFrame<F> {
    /// A native video layer below the UI still holds the last frame; the
    /// overlay only darkens it.
    UnderOverlay,
    /// The last frame as the UI paints it (black behind it).
    Picture(F),
    /// No frame: black.
    Nothing,
}

/// What the user did on an overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldFrameAction {
    None,
    Disconnect,
}

/// Fills the screen with the held frame and returns its rectangle.
fn held_frame_rect<F: FnOnce(&egui::Painter, egui::Rect)>(
    ui: &mut egui::Ui,
    held: HeldFrame<F>,
) -> egui::Rect {
    let available = egui::vec2(ui.available_width(), ui.available_height().max(240.0));
    let (rect, _) = ui.allocate_exact_size(available, egui::Sense::hover());
    match held {
        HeldFrame::UnderOverlay => {}
        HeldFrame::Picture(paint) => {
            ui.painter().rect_filled(rect, 0.0, egui::Color32::BLACK);
            paint(ui.painter(), rect);
        }
        HeldFrame::Nothing => {
            ui.painter().rect_filled(rect, 0.0, egui::Color32::BLACK);
        }
    }
    rect
}

/// The headline, the detail line under it and the Disconnect button.
fn overlay_text_and_disconnect(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    headline: &str,
    detail: String,
) -> HeldFrameAction {
    ui.painter().text(
        rect.center() - egui::vec2(0.0, 20.0),
        egui::Align2::CENTER_CENTER,
        headline,
        egui::FontId::proportional(22.0),
        egui::Color32::WHITE,
    );
    ui.painter().text(
        rect.center() + egui::vec2(0.0, 16.0),
        egui::Align2::CENTER_CENTER,
        detail,
        egui::FontId::proportional(14.0),
        egui::Color32::from_gray(200),
    );
    // Disconnect button — bottom-center
    let btn_rect = egui::Rect::from_center_size(
        egui::pos2(rect.center().x, rect.bottom() - 44.0),
        egui::vec2(120.0, 34.0),
    );
    if ui.put(btn_rect, egui::Button::new("Disconnect")).clicked() {
        HeldFrameAction::Disconnect
    } else {
        HeldFrameAction::None
    }
}

/// "Signing in…": the host's sign-in screen closed because somebody signed
/// in, and the Deck is connecting to the desktop that replaced it.
pub fn render_signing_in<F: FnOnce(&egui::Painter, egui::Rect)>(
    ui: &mut egui::Ui,
    held: HeldFrame<F>,
) -> HeldFrameAction {
    let rect = held_frame_rect(ui, held);
    ui.painter()
        .rect_filled(rect, 0.0, egui::Color32::from_black_alpha(120));
    overlay_text_and_disconnect(
        ui,
        rect,
        "Signing in…",
        "Connecting to your desktop".to_owned(),
    )
}

/// The reconnect overlay; `resuming` once the host answered and the session
/// is being resumed. `retry_left_secs` counts down to the next attempt.
pub fn render_reconnecting<F: FnOnce(&egui::Painter, egui::Rect)>(
    ui: &mut egui::Ui,
    held: HeldFrame<F>,
    resuming: bool,
    retry_left_secs: Option<f32>,
) -> HeldFrameAction {
    let rect = held_frame_rect(ui, held);
    // 60% gray overlay
    ui.painter()
        .rect_filled(rect, 0.0, egui::Color32::from_black_alpha(153));
    let waiting = if resuming {
        "Authenticating…"
    } else {
        "Connecting…"
    };
    let detail =
        retry_left_secs.map_or_else(|| waiting.to_owned(), |s| format!("Retry in {s:.0}s"));
    overlay_text_and_disconnect(ui, rect, "Lost connection, attempting to reconnect", detail)
}
