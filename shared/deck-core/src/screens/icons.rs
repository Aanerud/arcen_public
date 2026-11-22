// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Deck's icons: outlines painted from vector paths on a 20-point grid,
//! every one at the same stroke width. No icon font, image or pack: the
//! paths are the Deck's own, so they carry no third-party licence.

use egui::{Color32, Pos2, Shape, Stroke, vec2};

/// The grid the icons are drawn on, in points.
pub const ICON_GRID: f32 = 20.0;

/// Every icon's stroke width, in points.
pub const ICON_STROKE: f32 = 1.5;

/// The Deck's icons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    /// Settings.
    Gear,
    /// Help: a question mark in a ring.
    Help,
    /// Add.
    Plus,
    /// A row's menu: three dots, one above the other.
    Kebab,
    /// A computer: a monitor on its stand.
    Monitor,
    /// A closed disclosure: a chevron pointing right.
    ChevronRight,
    /// An open disclosure: a chevron pointing down.
    ChevronDown,
}

impl Icon {
    /// Every icon.
    pub const ALL: [Self; 7] = [
        Self::Gear,
        Self::Help,
        Self::Plus,
        Self::Kebab,
        Self::Monitor,
        Self::ChevronRight,
        Self::ChevronDown,
    ];
}

/// The icon's outline, centred on `center`, drawn `size` points square
/// (the grid scaled), in `color`.
#[must_use]
pub fn shapes(icon: Icon, center: Pos2, size: f32, color: Color32) -> Vec<Shape> {
    let scale = size / ICON_GRID;
    // Grid coordinates run from -10 to 10 on both axes.
    let at = |x: f32, y: f32| center + vec2(x * scale, y * scale);
    let stroke = Stroke::new(ICON_STROKE, color);
    match icon {
        Icon::Plus => vec![
            Shape::line_segment([at(-6.0, 0.0), at(6.0, 0.0)], stroke),
            Shape::line_segment([at(0.0, -6.0), at(0.0, 6.0)], stroke),
        ],
        Icon::ChevronRight => vec![Shape::line(
            vec![at(-2.0, -5.0), at(3.0, 0.0), at(-2.0, 5.0)],
            stroke,
        )],
        Icon::ChevronDown => vec![Shape::line(
            vec![at(-5.0, -2.0), at(0.0, 3.0), at(5.0, -2.0)],
            stroke,
        )],
        Icon::Kebab => [-5.0, 0.0, 5.0]
            .into_iter()
            .map(|y| Shape::circle_stroke(at(0.0, y), 0.8 * scale, stroke))
            .collect(),
        Icon::Monitor => vec![
            Shape::closed_line(
                vec![at(-7.5, -6.5), at(7.5, -6.5), at(7.5, 3.5), at(-7.5, 3.5)],
                stroke,
            ),
            Shape::line_segment([at(0.0, 3.5), at(0.0, 6.5)], stroke),
            Shape::line_segment([at(-4.0, 7.0), at(4.0, 7.0)], stroke),
        ],
        Icon::Help => {
            // The hook of the question mark: over the top from the left,
            // down to the right, then the stem.
            let mut hook: Vec<Pos2> = (0..=10_u8)
                .map(|step| {
                    let angle = std::f32::consts::PI
                        + f32::from(step) / 10.0 * (std::f32::consts::PI * 4.0 / 3.0);
                    at(2.8 * angle.cos(), -2.0 + 2.8 * angle.sin())
                })
                .collect();
            hook.extend([at(0.0, 1.6), at(0.0, 2.6)]);
            vec![
                Shape::circle_stroke(at(0.0, 0.0), 8.5 * scale, stroke),
                Shape::line(hook, stroke),
                Shape::circle_stroke(at(0.0, 5.2), 0.5 * scale, stroke),
            ]
        }
        Icon::Gear => {
            // Eight teeth: each a flat top on the outer radius between two
            // slopes down to the root.
            let mut outline = Vec::with_capacity(32);
            for tooth in 0..8_u8 {
                let middle = f32::from(tooth) * std::f32::consts::TAU / 8.0;
                for (radius, offset) in [(6.0, -0.36), (8.3, -0.17), (8.3, 0.17), (6.0, 0.36)] {
                    let angle: f32 = middle + offset;
                    outline.push(at(radius * angle.cos(), radius * angle.sin()));
                }
            }
            vec![
                Shape::closed_line(outline, stroke),
                Shape::circle_stroke(at(0.0, 0.0), 2.8 * scale, stroke),
            ]
        }
    }
}

/// Paints the icon (see [`shapes`]).
pub fn paint(painter: &egui::Painter, icon: Icon, center: Pos2, size: f32, color: Color32) {
    painter.extend(shapes(icon, center, size, color));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each shape's stroke width, and whether it is filled.
    fn stroke_of(shape: &Shape) -> (f32, bool) {
        match shape {
            Shape::LineSegment { stroke, .. } => (stroke.width, false),
            Shape::Path(path) => (path.stroke.width, path.fill != Color32::TRANSPARENT),
            Shape::Circle(circle) => (circle.stroke.width, circle.fill != Color32::TRANSPARENT),
            other => panic!("not an outline: {other:?}"),
        }
    }

    #[test]
    fn every_icon_is_an_outline_at_one_stroke_width_inside_its_grid() {
        let center = Pos2::new(100.0, 100.0);
        for size in [ICON_GRID, 16.0, 24.0] {
            for icon in Icon::ALL {
                let shapes = shapes(icon, center, size, Color32::BLACK);
                assert!(!shapes.is_empty(), "{icon:?}");
                for shape in &shapes {
                    let (width, filled) = stroke_of(shape);
                    assert!(
                        (width - ICON_STROKE).abs() < f32::EPSILON,
                        "{icon:?}: {width}"
                    );
                    assert!(!filled, "{icon:?} is filled");
                    let half = size / 2.0 + ICON_STROKE;
                    let bounds = shape.visual_bounding_rect();
                    assert!(
                        (bounds.min.x >= center.x - half - 0.01)
                            && (bounds.max.x <= center.x + half + 0.01)
                            && (bounds.min.y >= center.y - half - 0.01)
                            && (bounds.max.y <= center.y + half + 0.01),
                        "{icon:?} at {size}: {bounds:?}"
                    );
                }
            }
        }
    }
}
