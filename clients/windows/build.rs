// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Build steps of the Windows Deck: the presenter's shaders (fxc at build
//! time, no `D3DCompiler` at run time).

#[path = "src/present/build_shaders.rs"]
mod present_shaders;

fn main() {
    present_shaders::compile();
}
