// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The decoder → presenter picture: the decoder's
//! `decode::D3d11Picture` (the Windows `VideoDecoderBackend::NativeFrame`),
//! one slice of the decoder's texture array on the shared device with its
//! decode fence, visible rectangle and slot lease.

pub use crate::decode::D3d11Picture;
