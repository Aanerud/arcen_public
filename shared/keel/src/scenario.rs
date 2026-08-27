//! Deterministic synthetic frames used by Keel correctness tests and benches.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScenarioKind {
    Idle,
    Typing,
    Drag,
    Scroll,
    Video,
    Burst,
}

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
        }
    }
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
    if scenario.height == 0 {
        return;
    }
    let band_height = 12.min(scenario.height);
    let max_start = scenario.height.saturating_sub(band_height);
    let start_y = if max_start == 0 {
        0
    } else {
        tick_index(tick, max_start + 1) * 16 % (max_start + 1)
    };
    let width = (scenario.width / 3).max(1);
    paint_rect(
        output,
        scenario.stride,
        scenario.width,
        scenario.height,
        Rect {
            x: 0,
            y: start_y,
            width,
            height: band_height,
        },
        [240, 240, 240, 0xff],
    );
}

fn render_drag(scenario: Scenario, tick: u64, output: &mut [u8]) {
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
    paint_rect(
        output,
        scenario.stride,
        scenario.width,
        scenario.height,
        Rect {
            x,
            y,
            width,
            height,
        },
        [32, 180, 240, 0xff],
    );
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
