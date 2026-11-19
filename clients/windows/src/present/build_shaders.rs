// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Build-time half of the presenter: compiles `shaders/present.hlsl` (the
//! conversion pass) and `shaders/canvas.hlsl` (the canvas passes) to DXBC
//! with the Windows SDK's `fxc.exe`, so the Deck never loads `D3DCompiler`
//! at run time.
//!
//! The crate's `build.rs` includes this file:
//!
//! ```ignore
//! #[path = "src/present/build_shaders.rs"]
//! mod present_shaders;
//! fn main() { present_shaders::compile(); }
//! ```
//!
//! A non-Windows host (`cargo xwin check` on a Mac) has no `fxc`: it writes
//! empty blobs and sets `arcen_shaders_missing`, and the presenter refuses
//! to start with that reason. `ARCEN_FXC` overrides the compiler path.

#![allow(
    clippy::expect_used,
    unreachable_pub,
    reason = "a build script fails by panicking; this file is a module of build.rs"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// Shader sources, relative to the crate's manifest directory, with their
/// entry points and profiles; the blobs land in `$OUT_DIR/present/`
/// (entry names are unique across sources).
pub const SHADERS: &[(&str, &[(&str, &str)])] = &[
    (
        "src/present/shaders/present.hlsl",
        &[
            ("vs_main", "vs_5_0"),
            ("ps_biplanar", "ps_5_0"),
            ("ps_y410", "ps_5_0"),
            ("ps_ayuv", "ps_5_0"),
            ("ps_biplanar_refined_rgb", "ps_5_0"),
            ("ps_biplanar_refined_ycbcr", "ps_5_0"),
            ("ps_y410_refined_rgb", "ps_5_0"),
            ("ps_y410_refined_ycbcr", "ps_5_0"),
        ],
    ),
    (
        "src/present/shaders/canvas.hlsl",
        &[
            ("vs_rect", "vs_5_0"),
            ("ps_fill", "ps_5_0"),
            ("ps_paint", "ps_5_0"),
        ],
    ),
];

fn find_fxc() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("ARCEN_FXC") {
        return Some(PathBuf::from(path));
    }
    let kits = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(kits)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.join("x64").join("fxc.exe").is_file())
        .collect();
    versions.sort();
    versions.pop().map(|path| path.join("x64").join("fxc.exe"))
}

/// Compiles every entry point; panics (failing the build) on an HLSL error.
pub fn compile() {
    for (source, _) in SHADERS {
        println!("cargo:rerun-if-changed={source}");
    }
    println!("cargo:rerun-if-env-changed=ARCEN_FXC");
    println!("cargo::rustc-check-cfg=cfg(arcen_shaders_missing)");
    println!("cargo::rustc-check-cfg=cfg(arcen_present_hub_e2e)");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("present");
    std::fs::create_dir_all(&out).expect("create OUT_DIR/present");
    let host_is_windows = std::env::var("HOST").is_ok_and(|host| host.contains("windows"));
    let fxc = if host_is_windows { find_fxc() } else { None };
    let Some(fxc) = fxc else {
        println!(
            "cargo:warning=fxc.exe is not available on this host: presenter shaders are empty"
        );
        println!("cargo:rustc-cfg=arcen_shaders_missing");
        for (_, entries) in SHADERS {
            for (entry, _) in *entries {
                std::fs::write(out.join(format!("{entry}.cso")), []).expect("write placeholder");
            }
        }
        return;
    };
    for (source, entries) in SHADERS {
        for (entry, profile) in *entries {
            let target = out.join(format!("{entry}.cso"));
            // /Gis: strict IEEE, so the GPU stays close to the CPU reference.
            let status = Command::new(&fxc)
                .args([
                    "/nologo", "/O3", "/Gis", "/WX", "/T", profile, "/E", entry, "/Fo",
                ])
                .arg(&target)
                .arg(Path::new(source))
                .status()
                .expect("run fxc.exe");
            assert!(status.success(), "fxc failed for {entry}");
        }
    }
}
