// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Reading a decoded picture back to the CPU as planar YUV (the layout a
//! software decoder writes), for bit-exact comparison in lab tests. Not on
//! the presentation path.

use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, ID3D11Device, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_AYUV, DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_FORMAT_Y410, DXGI_SAMPLE_DESC,
};

use super::session::D3d11Picture;

/// Planar samples of the visible picture: Y, then Cb, then Cr; one byte per
/// sample at eight bits, two little-endian bytes above.
///
/// # Errors
///
/// Staging or mapping failure, or a format this reader does not know.
#[allow(clippy::too_many_lines)]
pub fn read_planar(
    device: &ID3D11Device,
    context_lock: &std::sync::Mutex<()>,
    picture: &D3d11Picture,
) -> Result<Vec<u8>, String> {
    let _guard = context_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (coded_width, coded_height) = picture.coded_size;
    let (left, top, width, height) = picture.visible;
    // SAFETY: staging copy and map of a texture this function owns; the
    // mapped pointer is read within its reported pitch and height.
    #[allow(unsafe_code)]
    unsafe {
        let context = device
            .GetImmediateContext()
            .map_err(|error| format!("context: {error}"))?;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: coded_width,
            Height: coded_height,
            MipLevels: 1,
            ArraySize: 1,
            Format: picture.format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: u32::try_from(D3D11_CPU_ACCESS_READ.0).unwrap_or(0),
            MiscFlags: 0,
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&desc, None, Some(&mut staging))
            .map_err(|error| format!("staging texture: {error}"))?;
        let staging = staging.ok_or("staging texture: none")?;
        context.CopySubresourceRegion(
            &staging,
            0,
            0,
            0,
            0,
            &picture.texture,
            picture.array_slice,
            None,
        );
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|error| format!("map: {error}"))?;
        let pitch = mapped.RowPitch as usize;
        let base = mapped.pData.cast::<u8>().cast_const();
        let rows = if picture.format == DXGI_FORMAT_NV12 || picture.format == DXGI_FORMAT_P010 {
            coded_height as usize * 3 / 2
        } else {
            coded_height as usize
        };
        let surface = std::slice::from_raw_parts(base, pitch * rows);
        let (w, h, l, t) = (width as usize, height as usize, left as usize, top as usize);
        let mut out = Vec::new();
        let result = if picture.format == DXGI_FORMAT_NV12 || picture.format == DXGI_FORMAT_P010 {
            let wide = picture.format == DXGI_FORMAT_P010;
            let push = |out: &mut Vec<u8>, row: &[u8], index: usize| {
                if wide {
                    let value = u16::from_le_bytes([row[index * 2], row[index * 2 + 1]]) >> 6;
                    out.extend_from_slice(&value.to_le_bytes());
                } else {
                    out.push(row[index]);
                }
            };
            for y in t..t + h {
                let row = &surface[y * pitch..];
                for x in l..l + w {
                    push(&mut out, row, x);
                }
            }
            let chroma = coded_height as usize * pitch;
            for component in 0..2 {
                for y in t / 2..t / 2 + h / 2 {
                    let row = &surface[chroma + y * pitch..];
                    for x in l / 2..l / 2 + w / 2 {
                        push(&mut out, row, x * 2 + component);
                    }
                }
            }
            Ok(())
        } else if picture.format == DXGI_FORMAT_AYUV {
            // Memory order per pixel: V, U, Y, A.
            for component in [2, 1, 0] {
                for y in t..t + h {
                    let row = &surface[y * pitch..];
                    for x in l..l + w {
                        out.push(row[x * 4 + component]);
                    }
                }
            }
            Ok(())
        } else if picture.format == DXGI_FORMAT_Y410 {
            // 32-bit little-endian pixels: U bits 0-9, Y 10-19, V 20-29.
            for shift in [10, 0, 20] {
                for y in t..t + h {
                    let row = &surface[y * pitch..];
                    for x in l..l + w {
                        let pixel = u32::from_le_bytes([
                            row[x * 4],
                            row[x * 4 + 1],
                            row[x * 4 + 2],
                            row[x * 4 + 3],
                        ]);
                        let value = u16::try_from((pixel >> shift) & 0x3ff).unwrap_or(0);
                        out.extend_from_slice(&value.to_le_bytes());
                    }
                }
            }
            Ok(())
        } else {
            Err(format!("no planar reader for {:?}", picture.format))
        };
        context.Unmap(&staging, 0);
        result.map(|()| out)
    }
}
