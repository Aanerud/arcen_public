// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! `arcen-deck doc-shots`: renders the manual's scenes for one Deck and
//! writes the PNGs and `deck-shots-geometry.json` (both Decks' CLIs call
//! [`run`] with their own [`DocShotShell`]).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::capture::{Capture, capture};
use super::gpu::{Gpu, write_png};
use super::hotspots::{Placement, place};
use super::scenes::{SCENE_IDS, Scene};
use super::{DocShotShell, DocTheme, PIXELS_PER_POINT, WINDOW_POINTS};

/// The usage line.
pub const USAGE: &str = "doc-shots --out DIR [--scene ID]... [--theme light|dark] \
[--manifest deck-shots/manifest.json] [--widgets]   (developer-only: the manual's screenshots, \
offscreen; docs/testing/deck-manual-screenshots.md)";

/// The geometry file's name, beside the PNGs.
pub const GEOMETRY_FILE: &str = "deck-shots-geometry.json";

#[derive(Debug, Default)]
struct Options {
    out: Option<PathBuf>,
    scenes: Vec<String>,
    themes: Vec<DocTheme>,
    manifest: Option<PathBuf>,
    widgets: bool,
}

fn options(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| {
            args.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match arg.as_str() {
            "--out" => options.out = Some(PathBuf::from(value("--out")?)),
            "--scene" => options.scenes.push(value("--scene")?),
            "--theme" => options.themes.push(match value("--theme")?.as_str() {
                "light" => DocTheme::Light,
                "dark" => DocTheme::Dark,
                other => return Err(format!("--theme {other}: light or dark")),
            }),
            "--manifest" => options.manifest = Some(PathBuf::from(value("--manifest")?)),
            "--widgets" => options.widgets = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if options.themes.is_empty() {
        options.themes = DocTheme::ALL.to_vec();
    }
    if options.scenes.is_empty() {
        options.scenes = SCENE_IDS.iter().map(|id| (*id).to_owned()).collect();
    }
    if let Some(unknown) = options
        .scenes
        .iter()
        .find(|id| !SCENE_IDS.contains(&id.as_str()))
    {
        return Err(format!(
            "no scene {unknown}; the scenes are {}",
            SCENE_IDS.join(", ")
        ));
    }
    Ok(options)
}

/// Runs the command with `args` (after `doc-shots`) for `shell`'s Deck and
/// returns the process exit code: 0 when every image and every mapped
/// hotspot was produced, 1 otherwise, 2 for a usage error.
pub fn run(args: &[String], shell: &dyn DocShotShell) -> i32 {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("usage: arcen-deck {USAGE}");
        return 0;
    }
    let options = match options(args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("doc-shots: {error}\nusage: arcen-deck {USAGE}");
            return 2;
        }
    };
    let Some(out) = options.out.clone() else {
        eprintln!("doc-shots: --out DIR is required\nusage: arcen-deck {USAGE}");
        return 2;
    };
    match render_all(&options, &out, shell) {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(error) => {
            eprintln!("doc-shots: {error}");
            1
        }
    }
}

fn render_all(options: &Options, out: &Path, shell: &dyn DocShotShell) -> Result<bool, String> {
    let platform = shell.platform();
    std::fs::create_dir_all(out).map_err(|error| format!("{}: {error}", out.display()))?;
    let mut complete = true;
    if let Some(manifest) = &options.manifest {
        complete &= check_manifest(manifest)?;
    }
    let gpu = Gpu::new()?;
    println!("doc-shots: {} Deck on {}", platform.token(), gpu.adapter());
    let size = pixel_size();
    let geometry_path = out.join(GEOMETRY_FILE);
    let mut geometry = read_geometry(&geometry_path);
    let mut written = 0_usize;
    let mut unmapped = Vec::new();
    for id in &options.scenes {
        for theme in &options.themes {
            let mut scene = Scene::build(id, platform).ok_or_else(|| format!("no scene {id}"))?;
            let shot = capture(shell, &mut scene, *theme)?;
            let pixels = gpu.render(&shot, size)?;
            let name = format!("deck-{id}-{}-{}.png", platform.token(), theme.token());
            write_png(&out.join(&name), size, &pixels)?;
            written += 1;
            if options.widgets {
                write_widgets(out, &name, &shot)?;
            }
            let (entry, missing, groups) = hotspot_geometry(&shot, platform);
            for message in &missing {
                eprintln!("doc-shots: {name}: {message}");
            }
            complete &= missing.is_empty();
            if *theme == DocTheme::Light {
                unmapped.extend(groups.into_iter().map(|label| format!("{id} {label}")));
            }
            set_entry(&mut geometry, id, platform.token(), theme.token(), entry);
            println!("doc-shots: wrote {name}");
        }
    }
    let text = serde_json::to_string_pretty(&geometry)
        .map_err(|error| format!("{GEOMETRY_FILE}: {error}"))?;
    std::fs::write(&geometry_path, text + "\n")
        .map_err(|error| format!("{}: {error}", geometry_path.display()))?;
    println!(
        "doc-shots: {written} images and {GEOMETRY_FILE} in {}",
        out.display()
    );
    if !unmapped.is_empty() {
        println!(
            "doc-shots: unmapped or outside the image: {}",
            unmapped.join("; ")
        );
    }
    Ok(complete)
}

/// The image size: the window at [`PIXELS_PER_POINT`].
#[must_use]
pub fn pixel_size() -> [u32; 2] {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "1120 × 760 points at 2×"
    )]
    WINDOW_POINTS.map(|points| (points * PIXELS_PER_POINT).round() as u32)
}

fn percent(point: egui::Pos2) -> Value {
    let round = |value: f32| f64::from((value * 100.0).round()) / 100.0;
    json!([
        round(point.x / WINDOW_POINTS[0] * 100.0),
        round(point.y / WINDOW_POINTS[1] * 100.0)
    ])
}

fn rect_percent(rect: egui::Rect) -> Value {
    let min = percent(rect.min);
    let max = percent(rect.max);
    json!([min[0], min[1], max[0], max[1]])
}

/// Whether `rect`'s centre is inside the image.
fn in_image(rect: egui::Rect) -> bool {
    egui::Rect::from_min_size(
        egui::Pos2::ZERO,
        egui::vec2(WINDOW_POINTS[0], WINDOW_POINTS[1]),
    )
    .contains(rect.center())
}

/// One image's hotspots: the entry, what could not be found, and what is
/// unmapped or outside the image.
fn hotspot_geometry(
    shot: &Capture,
    platform: super::DocPlatform,
) -> (Value, Vec<String>, Vec<String>) {
    let mut entry = Map::new();
    let mut missing = Vec::new();
    let mut unmapped = Vec::new();
    for (hotspot, placement) in place(shot.scene, platform, &shot.widgets) {
        let key = hotspot.n.to_string();
        let what = format!("{} \u{201c}{}\u{201d}", hotspot.n, hotspot.label);
        let value = match placement {
            Placement::At { widget, rect } => {
                let visible = in_image(rect);
                if !visible {
                    unmapped.push(format!("{what} (outside the image)"));
                }
                json!({
                    "label": hotspot.label,
                    "widget": widget.describe(),
                    "at": percent(rect.center()),
                    "rect": rect_percent(rect),
                    "in_image": visible,
                })
            }
            Placement::Group { reason, parts } => {
                unmapped.push(format!("{what} (group)"));
                json!({
                    "label": hotspot.label,
                    "unmapped": reason,
                    "parts": parts
                        .iter()
                        .map(|(widget, rect)| json!({
                            "widget": widget.describe(),
                            "at": rect.map(|rect| percent(rect.center())),
                            "rect": rect.map(rect_percent),
                        }))
                        .collect::<Vec<_>>(),
                })
            }
            Placement::Unmapped { reason } => {
                unmapped.push(format!("{what} (not on screen)"));
                json!({ "label": hotspot.label, "unmapped": reason })
            }
            Placement::Missing { widget } => {
                missing.push(format!(
                    "hotspot {what}: no widget named \u{201c}{}\u{201d}",
                    widget.text()
                ));
                json!({ "label": hotspot.label, "missing": widget.text() })
            }
            Placement::Absent => continue,
        };
        entry.insert(key, value);
    }
    (Value::Object(entry), missing, unmapped)
}

fn read_geometry(path: &Path) -> Value {
    let fresh = || {
        json!({
            "format": "arcen-deck-shots-geometry",
            "version": 1,
            "about": "Hotspot centres as percentages of each image's width and height from its \
                      top-left corner, keyed by scene, platform, theme and the manifest's hotspot \
                      n; written by `arcen-deck doc-shots` from the widget rectangles egui \
                      reports. An entry with `unmapped` names a group, not one widget; its \
                      parts are listed.",
            "window_points": WINDOW_POINTS,
            "pixels": pixel_size(),
            "scenes": {},
        })
    };
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(|value| value.get("format") == Some(&json!("arcen-deck-shots-geometry")))
        .unwrap_or_else(fresh)
}

fn set_entry(geometry: &mut Value, scene: &str, platform: &str, theme: &str, entry: Value) {
    let Some(scenes) = geometry.get_mut("scenes").and_then(Value::as_object_mut) else {
        return;
    };
    let scene = scenes.entry(scene.to_owned()).or_insert_with(|| json!({}));
    if let Some(scene) = scene.as_object_mut() {
        let platform = scene
            .entry(platform.to_owned())
            .or_insert_with(|| json!({}));
        if let Some(platform) = platform.as_object_mut() {
            platform.insert(theme.to_owned(), entry);
        }
    }
}

fn write_widgets(out: &Path, image: &str, shot: &Capture) -> Result<(), String> {
    let folder = out.join("widgets");
    std::fs::create_dir_all(&folder).map_err(|error| format!("{}: {error}", folder.display()))?;
    let widgets: Vec<Value> = shot
        .widgets
        .iter()
        .map(|widget| {
            json!({
                "role": format!("{:?}", widget.role),
                "label": widget.label,
                "value": widget.value,
                "placeholder": widget.placeholder,
                "rect": rect_percent(widget.rect),
            })
        })
        .collect();
    let path = folder.join(image.replace(".png", ".json"));
    let text = serde_json::to_string_pretty(&widgets).map_err(|error| error.to_string())?;
    std::fs::write(&path, text).map_err(|error| format!("{}: {error}", path.display()))
}

/// Compares the website's manifest with this harness: every scene it
/// lists must have a fixture, and every hotspot the same label here.
fn check_manifest(path: &Path) -> Result<bool, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let manifest: Value =
        serde_json::from_slice(&bytes).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut ok = true;
    let scenes = manifest
        .get("scenes")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{}: no scenes", path.display()))?;
    let mut seen = BTreeSet::new();
    for scene in scenes {
        let id = scene.get("id").and_then(Value::as_str).unwrap_or_default();
        seen.insert(id.to_owned());
        if !SCENE_IDS.contains(&id) {
            eprintln!("doc-shots: the manifest's scene {id} has no fixture");
            ok = false;
            continue;
        }
        for hotspot in scene
            .get("hotspots")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let n = hotspot.get("n").and_then(Value::as_u64).unwrap_or(0);
            let label = hotspot
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match super::hotspots::hotspot(id, n) {
                Some(known) if known.label == label => {}
                Some(known) => {
                    eprintln!(
                        "doc-shots: {id} hotspot {n} is \u{201c}{label}\u{201d} in the manifest, \
                         \u{201c}{}\u{201d} here",
                        known.label
                    );
                    ok = false;
                }
                None => {
                    eprintln!("doc-shots: {id} hotspot {n} \u{201c}{label}\u{201d} is not mapped");
                    ok = false;
                }
            }
        }
    }
    for id in SCENE_IDS {
        if !seen.contains(id) {
            eprintln!("doc-shots: scene {id} is not in the manifest");
        }
    }
    Ok(ok)
}
