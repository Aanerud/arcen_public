// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Test doubles shared by the runtime's decode tests.

use std::fmt;
use std::marker::PhantomData;

use arcen_protocol::VideoHeader;

use crate::decode::{DecodedVideoFrame, SessionColor, VideoDecodeError, VideoDecoderBackend};

/// A decoder that keeps the shared decoder contract
/// (`crate::decode::VideoDecoderBackend`): reserved wire colour values are
/// errors, a keyframe that is not Annex B fails, and nothing decodes until
/// a keyframe did. The router's and the media worker's reactions to each
/// outcome are what their tests prove; the platform decoders prove the
/// contract itself. Its frames are CPU frames, so `N` is only the native
/// picture type the caller's pipeline names.
pub(crate) struct ContractDecoder<N = ()> {
    waiting: bool,
    picture: PhantomData<fn() -> N>,
}

impl<N> fmt::Debug for ContractDecoder<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ContractDecoder")
            .field("waiting", &self.waiting)
            .finish()
    }
}

impl<N> Default for ContractDecoder<N> {
    fn default() -> Self {
        Self {
            waiting: true,
            picture: PhantomData,
        }
    }
}

impl<N: Clone + fmt::Debug> VideoDecoderBackend for ContractDecoder<N> {
    type NativeFrame = N;

    fn set_session_color(&mut self, _session_color: SessionColor) {}

    fn set_prefer_native_frames(&mut self, _prefer: bool) -> bool {
        false
    }

    fn set_decode_latency_policy(&mut self, _policy: arcen_media::video::DecodeLatencyPolicy) {}

    fn decode(
        &mut self,
        header: &VideoHeader,
        payload: &[u8],
    ) -> Result<Option<DecodedVideoFrame<N>>, VideoDecodeError> {
        header
            .bit_depth()
            .map_err(VideoDecodeError::InvalidWireColor)?;
        header
            .color_matrix()
            .map_err(VideoDecodeError::InvalidWireColor)?;
        if header.is_keyframe() {
            if !(payload.starts_with(&[0, 0, 1]) || payload.starts_with(&[0, 0, 0, 1])) {
                self.waiting = true;
                return Err(VideoDecodeError::Backend("not Annex B".to_owned()));
            }
            self.waiting = false;
        }
        if self.waiting {
            return Ok(None);
        }
        Ok(Some(DecodedVideoFrame {
            width: 2,
            height: 2,
            rgba: vec![0; 16],
            timestamp_ms: header.timestamp_ms,
            pixel_format: "test".to_owned(),
            backend: "contract",
            native: None,
        }))
    }

    fn backend_name(&self) -> &'static str {
        "contract"
    }

    fn is_hardware_accelerated(&self) -> Option<bool> {
        None
    }

    fn wants_keyframe(&self) -> bool {
        self.waiting
    }

    fn notify_discontinuity(&mut self) {
        self.waiting = true;
    }
}
