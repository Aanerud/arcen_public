// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

#![allow(warnings)]
#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals)]
mod guid;
mod version;
#[rustfmt::skip]
mod windows_sys;
pub use guid::*;
pub use version::*;
pub use windows_sys::*;
