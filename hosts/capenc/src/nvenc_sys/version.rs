#![allow(missing_docs)]
//! NVENC structure-version constants transcribed from the vendored MIT-licensed
//! nv-codec-headers header.
//! Source: `third_party/nv-codec-headers/include/ffnvcodec/nvEncodeAPI.h` at tag `n12.1.14.1`.
//! Do not regenerate from the NVIDIA Video Codec SDK.
//!
//! `NVENCAPI_STRUCT_VERSION` is a function-like C macro, so bindgen emits none of
//! these. They are transcribed from the header and MUST match it exactly: a wrong
//! struct version makes the driver reject the call with `NV_ENC_ERR_INVALID_VERSION`
//! at runtime, which no amount of compiling will reveal. Note that several
//! constants carry a `| (1u<<31)` high bit -- dropping it produces exactly that
//! failure, and did.

#[must_use]
#[allow(non_snake_case)]
/// Macro to generate per-structure version for use with API.
pub const fn NVENCAPI_STRUCT_VERSION(ver: u32) -> u32 {
    super::nvEncodeAPI::NVENCAPI_VERSION | (ver << 16) | (0x7 << 28)
}

pub const NV_ENC_CAPS_PARAM_VER: u32 = NVENCAPI_STRUCT_VERSION(1);
pub const NV_ENC_RESTORE_ENCODER_STATE_PARAMS_VER: u32 = NVENCAPI_STRUCT_VERSION(1);
pub const NV_ENC_OUTPUT_STATS_BLOCK_VER: u32 = NVENCAPI_STRUCT_VERSION(1);
pub const NV_ENC_OUTPUT_STATS_ROW_VER: u32 = NVENCAPI_STRUCT_VERSION(1);
pub const NV_ENC_ENCODE_OUT_PARAMS_VER: u32 = NVENCAPI_STRUCT_VERSION(1);
pub const NV_ENC_LOOKAHEAD_PIC_PARAMS_VER: u32 = NVENCAPI_STRUCT_VERSION(1);
pub const NV_ENC_CREATE_INPUT_BUFFER_VER: u32 = NVENCAPI_STRUCT_VERSION(1);
pub const NV_ENC_CREATE_BITSTREAM_BUFFER_VER: u32 = NVENCAPI_STRUCT_VERSION(1);
pub const NV_ENC_CREATE_MV_BUFFER_VER: u32 = NVENCAPI_STRUCT_VERSION(1);
pub const NV_ENC_RC_PARAMS_VER: u32 = NVENCAPI_STRUCT_VERSION(1);
pub const NV_ENC_CONFIG_VER: u32 = (NVENCAPI_STRUCT_VERSION(8) | (1 << 31));
pub const NV_ENC_INITIALIZE_PARAMS_VER: u32 = (NVENCAPI_STRUCT_VERSION(6) | (1 << 31));
pub const NV_ENC_RECONFIGURE_PARAMS_VER: u32 = (NVENCAPI_STRUCT_VERSION(1) | (1 << 31));
pub const NV_ENC_PRESET_CONFIG_VER: u32 = (NVENCAPI_STRUCT_VERSION(4) | (1 << 31));
pub const NV_ENC_PIC_PARAMS_MVC_VER: u32 = NVENCAPI_STRUCT_VERSION(1);
pub const NV_ENC_PIC_PARAMS_VER: u32 = (NVENCAPI_STRUCT_VERSION(6) | (1 << 31));
pub const NV_ENC_MEONLY_PARAMS_VER: u32 = NVENCAPI_STRUCT_VERSION(3);
pub const NV_ENC_LOCK_BITSTREAM_VER: u32 = (NVENCAPI_STRUCT_VERSION(1) | (1 << 31));
pub const NV_ENC_LOCK_INPUT_BUFFER_VER: u32 = NVENCAPI_STRUCT_VERSION(1);
pub const NV_ENC_MAP_INPUT_RESOURCE_VER: u32 = NVENCAPI_STRUCT_VERSION(4);
pub const NV_ENC_FENCE_POINT_D3D12_VER: u32 = NVENCAPI_STRUCT_VERSION(1);
pub const NV_ENC_INPUT_RESOURCE_D3D12_VER: u32 = NVENCAPI_STRUCT_VERSION(1);
pub const NV_ENC_OUTPUT_RESOURCE_D3D12_VER: u32 = NVENCAPI_STRUCT_VERSION(1);
pub const NV_ENC_REGISTER_RESOURCE_VER: u32 = NVENCAPI_STRUCT_VERSION(4);
pub const NV_ENC_STAT_VER: u32 = NVENCAPI_STRUCT_VERSION(1);
pub const NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER: u32 = NVENCAPI_STRUCT_VERSION(1);
pub const NV_ENC_EVENT_PARAMS_VER: u32 = NVENCAPI_STRUCT_VERSION(1);
pub const NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = NVENCAPI_STRUCT_VERSION(1);
pub const NV_ENCODE_API_FUNCTION_LIST_VER: u32 = NVENCAPI_STRUCT_VERSION(2);
