// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

use std::path::PathBuf;

fn main() {
    embed(
        "ARCEN_PIER_BINARY",
        "the release arcen-pier binary",
        "ARCEN_PIER_BINARY_ABS",
    );
    embed(
        "ARCEN_PAM_GREETER",
        "the release pam_arcen_greeter.so (cargo build --release -p arcen-pam-greeter)",
        "ARCEN_PAM_GREETER_ABS",
    );
}

fn embed(variable: &str, what: &str, exported: &str) {
    println!("cargo:rerun-if-env-changed={variable}");
    let Some(path) = std::env::var_os(variable) else {
        panic!("{variable} must point to {what} to embed");
    };
    let path = PathBuf::from(path);
    let absolute = if path.is_absolute() {
        path
    } else {
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"))
            .join("../../..")
            .join(path)
    };
    if !absolute.is_file() {
        panic!("{variable} does not name a file: {}", absolute.display());
    }
    println!("cargo:rustc-env={exported}={}", absolute.display());
    println!("cargo:rerun-if-changed={}", absolute.display());
}
