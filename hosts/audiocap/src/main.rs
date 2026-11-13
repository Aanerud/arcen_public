// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Thin binary wrapper. All behaviour lives in the library so the Pier can
//! link it and expose it as a subcommand of the single `arcen-pier` binary.
fn main() {
    arcen_audiocap::run();
}
