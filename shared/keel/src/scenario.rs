// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Deterministic synthetic frames used by Keel correctness tests and benches.

use crate::PixelRect;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScenarioKind {
    Idle,
    Typing,
    Drag,
    Scroll,
    Video,
    Burst,
    /// A text document scrolling inside a viewport under a static toolbar
    /// and beside a static sidebar, at varying speeds in both directions,
    /// with pauses.
    DocumentScroll,
    /// Text typed into an editor at about eight characters a second, with a
    /// caret blinking about twice a second.
    Editor,
}

/// The true scroll between two ticks of a scrolling scenario.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScrollTruth {
    /// The scrolling area.
    pub viewport: PixelRect,
    /// Current row `y` shows the previous frame's row `y + dy`.
    pub dy: i64,
}

/// Per-tick document speeds (pixels) of [`ScenarioKind::DocumentScroll`].
const DOCUMENT_SPEEDS: [i64; 24] = [
    0, 0, 3, 7, 12, 16, 24, 40, 64, 40, 24, 16, 12, 7, 3, 0, 0, -5, -11, -16, -30, -16, -11, -5,
];
const DOCUMENT_START: i64 = 4_000;
const LINE_HEIGHT: usize = 20;
const GLYPH_WIDTH: usize = 8;
const EDITOR_MARGIN: usize = 16;
const TICKS_PER_CHARACTER: u64 = 7;
const TICKS_PER_CARET_PHASE: u64 = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Scenario {
    width: usize,
    height: usize,
    stride: usize,
    kind: ScenarioKind,
    seed: u64,
}

impl Scenario {
    #[must_use]
    pub fn new(width: usize, height: usize, kind: ScenarioKind, seed: u64) -> Self {
        Self {
            width,
            height,
            stride: width.saturating_mul(4),
            kind,
            seed,
        }
    }

    #[must_use]
    pub const fn width(self) -> usize {
        self.width
    }

    #[must_use]
    pub const fn height(self) -> usize {
        self.height
    }

    #[must_use]
    pub const fn stride(self) -> usize {
        self.stride
    }

    #[must_use]
    pub const fn kind(self) -> ScenarioKind {
        self.kind
    }

    /// Rectangles an honest OS damage report would name between two ticks:
    /// a superset of every pixel that differs, as a compositor reports it.
    /// Cleared and refilled; the caller keeps the storage.
    pub fn damage_rects(self, previous_tick: u64, tick: u64, output: &mut Vec<PixelRect>) {
        output.clear();
        let full = PixelRect {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        };
        match self.kind {
            ScenarioKind::Idle => {}
            ScenarioKind::Typing => {
                for at in [previous_tick, tick] {
                    if let Some(rect) = typing_rect(self, at) {
                        output.push(rect.into());
                    }
                }
            }
            ScenarioKind::Drag => {
                output.push(drag_rect(self, previous_tick).into());
                output.push(drag_rect(self, tick).into());
            }
            ScenarioKind::Scroll | ScenarioKind::Video => output.push(full),
            ScenarioKind::Editor => {
                let typed = |at: u64| editor_typed(at);
                for index in
                    typed(previous_tick).min(typed(tick))..=typed(previous_tick).max(typed(tick))
                {
                    if let Some(cell) = editor_cell(self, index) {
                        output.push(cell);
                    }
                }
            }
            ScenarioKind::DocumentScroll => {
                if let Some(truth) = self
                    .scroll_truth(previous_tick, tick)
                    .filter(|truth| truth.dy != 0)
                {
                    output.push(truth.viewport);
                }
            }
            ScenarioKind::Burst => {
                if previous_tick % 12 >= 10 || tick % 12 >= 10 {
                    output.push(full);
                }
            }
        }
    }

    /// The scroll between two ticks, for the scrolling scenarios.
    #[must_use]
    pub fn scroll_truth(self, previous_tick: u64, tick: u64) -> Option<ScrollTruth> {
        match self.kind {
            ScenarioKind::Scroll if self.height > 0 => {
                let height = self.height as u64;
                let offset = |at: u64| (at % height) * 16 % height;
                let dy = (offset(tick) + height - offset(previous_tick)) % height;
                Some(ScrollTruth {
                    viewport: PixelRect {
                        x: 0,
                        y: 0,
                        width: self.width,
                        height: self.height,
                    },
                    dy: i64::try_from(dy).ok()?,
                })
            }
            ScenarioKind::DocumentScroll => Some(ScrollTruth {
                viewport: document_viewport(self),
                dy: document_position(tick) - document_position(previous_tick),
            }),
            _ => None,
        }
    }

    pub fn render(self, tick: u64, output: &mut Vec<u8>) {
        output.resize(self.stride.saturating_mul(self.height), 0);
        fill_background(output, self.stride, self.width, self.height, self.seed);
        match self.kind {
            ScenarioKind::Idle => {}
            ScenarioKind::Typing => render_typing(self, tick, output),
            ScenarioKind::Drag => render_drag(self, tick, output),
            ScenarioKind::Scroll => render_scroll(self, tick, output),
            ScenarioKind::Video => render_video(self, tick, output),
            ScenarioKind::Burst => {
                if tick % 12 >= 10 {
                    render_video(self, tick, output);
                }
            }
            ScenarioKind::DocumentScroll => render_document(self, tick, output),
            ScenarioKind::Editor => render_editor(self, tick, output),
        }
    }
}

const fn editor_typed(tick: u64) -> u64 {
    tick / TICKS_PER_CHARACTER
}

/// The cell of character `index`; the caret sits in the cell after the last
/// typed character.
fn editor_cell(scenario: Scenario, index: u64) -> Option<PixelRect> {
    let columns = scenario.width.saturating_sub(2 * EDITOR_MARGIN) / GLYPH_WIDTH;
    if columns == 0 {
        return None;
    }
    let index = usize::try_from(index).ok()?;
    let x = EDITOR_MARGIN + (index % columns) * GLYPH_WIDTH;
    let y = EDITOR_MARGIN + (index / columns) * LINE_HEIGHT;
    if y + LINE_HEIGHT > scenario.height {
        return None;
    }
    Some(PixelRect {
        x,
        y,
        width: GLYPH_WIDTH,
        height: LINE_HEIGHT,
    })
}

fn render_editor(scenario: Scenario, tick: u64, output: &mut [u8]) {
    let typed = editor_typed(tick);
    for index in 0..typed {
        let Some(cell) = editor_cell(scenario, index) else {
            break;
        };
        let line = usize::try_from(index).unwrap_or_default() * 31 % 997;
        for row in 0..LINE_HEIGHT {
            let y = cell.y + row;
            let pixels = &mut output[y * scenario.stride + cell.x * 4..][..GLYPH_WIDTH * 4];
            for (column, pixel) in pixels.chunks_exact_mut(4).enumerate() {
                let value = if document_ink(line * LINE_HEIGHT + row, column) {
                    40
                } else {
                    250
                };
                pixel.copy_from_slice(&[value, value, value, 0xff]);
            }
        }
    }
    let caret = editor_cell(scenario, typed).filter(|_| (tick / TICKS_PER_CARET_PHASE) % 2 == 0);
    if let Some(cell) = caret {
        for y in cell.y + 2..cell.y + 18 {
            let pixels = &mut output[y * scenario.stride + cell.x * 4..][..8];
            pixels.copy_from_slice(&[0, 0, 0, 0xff, 0, 0, 0, 0xff]);
        }
    }
}

fn document_viewport(scenario: Scenario) -> PixelRect {
    let top = (scenario.height / 8).min(48);
    let left = (scenario.width / 6).min(160);
    PixelRect {
        x: left,
        y: top,
        width: scenario.width - left,
        height: scenario.height - top,
    }
}

fn document_position(tick: u64) -> i64 {
    let cycle = DOCUMENT_SPEEDS.len() as u64;
    let per_cycle: i64 = DOCUMENT_SPEEDS.iter().sum();
    let cycles = i64::try_from(tick / cycle).unwrap_or(i64::MAX / per_cycle.max(1));
    let partial: i64 = DOCUMENT_SPEEDS[..=usize::try_from(tick % cycle).unwrap_or(0)]
        .iter()
        .sum();
    DOCUMENT_START + cycles * per_cycle + partial
}

fn render_document(scenario: Scenario, tick: u64, output: &mut [u8]) {
    let viewport = document_viewport(scenario);
    let position = usize::try_from(document_position(tick)).unwrap_or_default();
    for y in 0..scenario.height {
        let row = &mut output[y * scenario.stride..y * scenario.stride + scenario.width * 4];
        for (x, pixel) in row.chunks_exact_mut(4).enumerate() {
            let in_viewport = x >= viewport.x && y >= viewport.y;
            // Chrome text lives on its own pseudo-lines far from the document.
            let (line_row, column) = if in_viewport {
                (y - viewport.y + position, x - viewport.x)
            } else if y < viewport.y {
                (1_000_000 + y, x)
            } else {
                (2_000_000 + y, x)
            };
            let ink = document_ink(line_row, column);
            let value = if ink {
                40
            } else if in_viewport {
                250
            } else {
                225
            };
            pixel.copy_from_slice(&[value, value, value.wrapping_sub(8), 0xff]);
        }
    }
}

/// Deterministic glyph-like ink: 6x12 glyphs on 8x20 cells, word gaps,
/// blank lines and an occasional rule.
fn document_ink(line_row: usize, column: usize) -> bool {
    let line = line_row / LINE_HEIGHT;
    let row = line_row % LINE_HEIGHT;
    if line % 13 == 12 {
        return row == 10;
    }
    if line % 7 == 6 || !(4..16).contains(&row) {
        return false;
    }
    let glyph = column / 8;
    let cell_x = column % 8;
    if cell_x >= 6 {
        return false;
    }
    let seed =
        xorshift64(((line as u64).wrapping_mul(0x0001_0000_01b3) ^ ((glyph as u64) << 1)) | 1);
    if seed % 6 == 0 {
        return false;
    }
    let bit = ((row - 4) * 6 + cell_x) % 64;
    (xorshift64(seed) >> bit) & 1 == 1
}

fn fill_background(output: &mut [u8], stride: usize, width: usize, height: usize, seed: u64) {
    let base = seed.to_le_bytes()[0].wrapping_mul(17);
    for y in 0..height {
        let row = &mut output[y * stride..y * stride + width * 4];
        for (x, pixel) in row.chunks_exact_mut(4).enumerate() {
            let checker = u8::from(((x / 32) + (y / 32)) & 1 != 0);
            pixel.copy_from_slice(&[
                base.wrapping_add(checker * 8),
                base.wrapping_add(24 + checker * 8),
                base.wrapping_add(48 + checker * 8),
                0xff,
            ]);
        }
    }
}

fn render_typing(scenario: Scenario, tick: u64, output: &mut [u8]) {
    if let Some(rect) = typing_rect(scenario, tick) {
        paint_rect(
            output,
            scenario.stride,
            scenario.width,
            scenario.height,
            rect,
            [240, 240, 240, 0xff],
        );
    }
}

fn typing_rect(scenario: Scenario, tick: u64) -> Option<Rect> {
    if scenario.height == 0 {
        return None;
    }
    let band_height = 12.min(scenario.height);
    let max_start = scenario.height.saturating_sub(band_height);
    let start_y = if max_start == 0 {
        0
    } else {
        tick_index(tick, max_start + 1) * 16 % (max_start + 1)
    };
    Some(Rect {
        x: 0,
        y: start_y,
        width: (scenario.width / 3).max(1),
        height: band_height,
    })
}

fn render_drag(scenario: Scenario, tick: u64, output: &mut [u8]) {
    paint_rect(
        output,
        scenario.stride,
        scenario.width,
        scenario.height,
        drag_rect(scenario, tick),
        [32, 180, 240, 0xff],
    );
}

fn drag_rect(scenario: Scenario, tick: u64) -> Rect {
    let width = (scenario.width / 4).max(1);
    let height = (scenario.height / 4).max(1);
    let max_x = scenario.width.saturating_sub(width);
    let max_y = scenario.height.saturating_sub(height);
    let x = if max_x == 0 {
        0
    } else {
        tick_index(tick, max_x + 1) * 13 % (max_x + 1)
    };
    let y = if max_y == 0 {
        0
    } else {
        tick_index(tick, max_y + 1) * 7 % (max_y + 1)
    };
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn render_scroll(scenario: Scenario, tick: u64, output: &mut [u8]) {
    if scenario.height == 0 {
        return;
    }
    let offset = tick_index(tick, scenario.height) * 16 % scenario.height;
    for y in 0..scenario.height {
        let source_y = (y + offset) % scenario.height;
        let value = (source_y / 8).to_le_bytes()[0].wrapping_mul(11);
        let row = &mut output[y * scenario.stride..y * scenario.stride + scenario.width * 4];
        for pixel in row.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[value, value.wrapping_add(32), 220, 0xff]);
        }
    }
}

fn render_video(scenario: Scenario, tick: u64, output: &mut [u8]) {
    let mut state = scenario.seed ^ tick.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    for y in 0..scenario.height {
        let row = &mut output
            [y * scenario.stride..y * scenario.stride + scenario.width.saturating_mul(4)];
        for pixel in row.chunks_exact_mut(4) {
            state = xorshift64(state);
            let bytes = state.to_le_bytes();
            pixel.copy_from_slice(&[bytes[0], bytes[3], bytes[6], 0xff]);
        }
    }
}

#[derive(Clone, Copy)]
struct Rect {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

impl From<Rect> for PixelRect {
    fn from(rect: Rect) -> Self {
        Self {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        }
    }
}

fn paint_rect(
    output: &mut [u8],
    stride: usize,
    frame_width: usize,
    frame_height: usize,
    rect: Rect,
    color: [u8; 4],
) {
    let end_y = rect.y.saturating_add(rect.height).min(frame_height);
    let end_x = rect.x.saturating_add(rect.width).min(frame_width);
    for y in rect.y..end_y {
        let row = &mut output[y * stride..y * stride + frame_width * 4];
        for pixel in row[rect.x * 4..end_x * 4].chunks_exact_mut(4) {
            pixel.copy_from_slice(&color);
        }
    }
}

fn xorshift64(mut value: u64) -> u64 {
    if value == 0 {
        value = 0x4d59_5df4_d0f3_3173;
    }
    value ^= value << 13;
    value ^= value >> 7;
    value ^ (value << 17)
}

fn tick_index(tick: u64, modulus: usize) -> usize {
    if modulus == 0 {
        return 0;
    }
    let modulus_u64 = u64::try_from(modulus).unwrap_or(u64::MAX);
    usize::try_from(tick % modulus_u64).unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn scenarios_are_deterministic() {
        for kind in [
            ScenarioKind::Idle,
            ScenarioKind::Typing,
            ScenarioKind::Drag,
            ScenarioKind::Scroll,
            ScenarioKind::Video,
            ScenarioKind::Burst,
            ScenarioKind::DocumentScroll,
            ScenarioKind::Editor,
        ] {
            let scenario = Scenario::new(64, 32, kind, 42);
            let mut first = Vec::new();
            let mut second = Vec::new();
            scenario.render(7, &mut first);
            scenario.render(7, &mut second);
            assert_eq!(first, second);
        }
    }
}
