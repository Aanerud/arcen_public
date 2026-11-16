// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Monitor colour facts moved to [`crate::display`] (owner `platform`, the
//! `DeckServices` owner): `IDXGIOutput6::GetDesc1`, the SDR white level and
//! the advanced-colour state are read once there and feed both the hello's
//! `DisplayColorMsg` and the presenter's effective-depth report.

pub use crate::display::{WindowsDisplayFacts as OutputFacts, color_space_name, facts as outputs};
