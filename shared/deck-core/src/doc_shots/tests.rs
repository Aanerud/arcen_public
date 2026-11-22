// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Every manifest scene has a fixture, and each lays out at the manual's
//! window size in both themes on both Decks with every mapped hotspot on a
//! widget. Layout only: CI has no GPU, and the pictures are checked by eye,
//! never against golden images.

use super::capture::capture;
use super::hotspots::{Placement, TABLE, place};
use super::scenes::{SCENE_IDS, Scene};
use super::{DocPlatform, DocShotShell, DocTheme, PIXELS_PER_POINT};

/// A shell with the shared setup the Decks build on (the real shells lend
/// their own; see `clients/*/…/doc_shots.rs`).
struct TestShell(DocPlatform);

impl DocShotShell for TestShell {
    fn platform(&self) -> DocPlatform {
        self.0
    }

    fn setup(&self, ctx: &egui::Context) {
        crate::screens::theme::apply(ctx);
        crate::screens::wording::set_deck_platform(ctx, self.0.deck_platform());
    }

    fn root_frame(&self, ui: &egui::Ui) -> egui::Frame {
        egui::Frame::new()
            .fill(crate::screens::theme::colours(ui).bg)
            .inner_margin(0.0)
    }

    fn canvas_available(&self) -> bool {
        true
    }
}

const PLATFORMS: [DocPlatform; 2] = [DocPlatform::Mac, DocPlatform::Windows];

#[test]
fn every_manifest_scene_has_a_fixture_and_a_hotspot_table() {
    let mapped: Vec<&str> = TABLE.iter().map(|(id, _)| *id).collect();
    assert_eq!(mapped, SCENE_IDS);
    for id in SCENE_IDS {
        for platform in PLATFORMS {
            assert!(Scene::build(id, platform).is_some(), "{id} on {platform:?}");
        }
    }
    assert!(Scene::build("not-a-scene", DocPlatform::Mac).is_none());
}

#[test]
fn every_scene_lays_out_in_both_themes_with_its_hotspots_on_widgets() {
    for id in SCENE_IDS {
        for platform in PLATFORMS {
            for theme in DocTheme::ALL {
                let shell = TestShell(platform);
                let mut scene = Scene::build(id, platform).expect("a fixture");
                let shot = capture(&shell, &mut scene, theme)
                    .unwrap_or_else(|error| panic!("{id} {platform:?} {theme:?}: {error}"));
                assert!(
                    (shot.pixels_per_point - PIXELS_PER_POINT).abs() < f32::EPSILON,
                    "{id}: painted at {}",
                    shot.pixels_per_point
                );
                assert!(!shot.primitives.is_empty(), "{id}: nothing painted");
                for (hotspot, placement) in place(id, platform, &shot.widgets) {
                    if let Placement::Missing { widget } = placement {
                        let names: Vec<String> = shot
                            .widgets
                            .iter()
                            .flat_map(|widget| {
                                widget.texts().map(str::to_owned).collect::<Vec<_>>()
                            })
                            .collect();
                        panic!(
                            "{id} {platform:?} {theme:?}: hotspot {} \u{201c}{}\u{201d} names \
                             {widget:?}, which is not shown; shown: {names:?}",
                            hotspot.n, hotspot.label
                        );
                    }
                }
            }
        }
    }
}
