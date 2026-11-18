// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! D3D11 device and adapter selection, plus texture helpers.
//!
//! One adapter for the decoder and the presenters: the one that owns the
//! window's monitor (matched by LUID), never "whatever DXGI lists first".
//! A hybrid laptop's integrated GPU drives the panel; decoding on the
//! discrete GPU would add a cross-adapter copy per frame. Each window has a
//! device of its own on that adapter, and so does the decoder
//! (`crate::decode::shared`), unless the driver cannot share pictures.

use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::core::Interface;

use super::{Result, created, hr};

/// A window's device: its presenters, composition tree and cursor layer
/// run on it (and the decoder, where the driver cannot share pictures).
#[derive(Clone)]
pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub factory: IDXGIFactory2,
    pub adapter_name: String,
    pub luid: LUID,
    pub warp: bool,
    pub feature_level: D3D_FEATURE_LEVEL,
    /// Held across every multi-call sequence on `context` (state setup +
    /// draw, copy + map, resize): several threads share this immediate
    /// context (each pipeline's presenter, the cursor layer, and the
    /// decoder's video context where it shares this device), and
    /// multithread protection only makes single calls atomic.
    pub context_lock: std::sync::Arc<std::sync::Mutex<()>>,
}

#[derive(Debug, Clone)]
pub enum AdapterChoice {
    /// The adapter that owns this monitor (the product rule).
    Monitor(isize),
    /// The first adapter whose description contains this text.
    Name(String),
    /// The adapter with this LUID (a further window on the root window's
    /// adapter).
    Luid(LUID),
    /// The Microsoft Basic Render Driver (WARP), for shader tests.
    Warp,
    /// DXGI's high-performance preference (diagnostics only).
    HighPerformance,
}

#[derive(Debug, Clone)]
pub struct AdapterFacts {
    pub index: u32,
    pub name: String,
    pub luid: LUID,
    pub vendor: u32,
    pub device: u32,
    pub software: bool,
    pub dedicated_video_memory: usize,
    pub outputs: Vec<String>,
}

pub fn luid_text(luid: LUID) -> String {
    format!("{:08x}:{:08x}", luid.HighPart, luid.LowPart)
}

fn wide(text: &[u16]) -> String {
    let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
    String::from_utf16_lossy(&text[..end])
}

pub fn factory() -> Result<IDXGIFactory6> {
    hr("CreateDXGIFactory2", unsafe {
        CreateDXGIFactory2::<IDXGIFactory6>(DXGI_CREATE_FACTORY_FLAGS(0))
    })
}

pub fn adapters() -> Result<Vec<(IDXGIAdapter1, AdapterFacts)>> {
    let factory = factory()?;
    let mut out = Vec::new();
    for index in 0.. {
        let adapter: IDXGIAdapter1 = match unsafe {
            factory.EnumAdapterByGpuPreference(index, DXGI_GPU_PREFERENCE_UNSPECIFIED)
        } {
            Ok(adapter) => adapter,
            Err(_) => break,
        };
        let desc = hr("GetDesc1", unsafe { adapter.GetDesc1() })?;
        let mut outputs = Vec::new();
        for output_index in 0.. {
            let Ok(output) = (unsafe { adapter.EnumOutputs(output_index) }) else {
                break;
            };
            if let Ok(desc) = unsafe { output.GetDesc() } {
                outputs.push(wide(&desc.DeviceName));
            }
        }
        out.push((
            adapter,
            AdapterFacts {
                index,
                name: wide(&desc.Description),
                luid: desc.AdapterLuid,
                vendor: desc.VendorId,
                device: desc.DeviceId,
                software: (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0,
                dedicated_video_memory: desc.DedicatedVideoMemory,
                outputs,
            },
        ));
    }
    Ok(out)
}

/// The adapter whose output shows `monitor`.
pub fn adapter_for_monitor(monitor: HMONITOR) -> Result<Option<(IDXGIAdapter1, AdapterFacts)>> {
    for (adapter, facts) in adapters()? {
        for output_index in 0.. {
            let Ok(output) = (unsafe { adapter.EnumOutputs(output_index) }) else {
                break;
            };
            if unsafe { output.GetDesc() }.is_ok_and(|desc| desc.Monitor == monitor) {
                return Ok(Some((adapter, facts)));
            }
        }
    }
    Ok(None)
}

fn create_on(
    adapter: Option<&IDXGIAdapter1>,
    warp: bool,
) -> Result<(ID3D11Device, ID3D11DeviceContext, D3D_FEATURE_LEVEL)> {
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let driver = if warp {
        D3D_DRIVER_TYPE_WARP
    } else {
        D3D_DRIVER_TYPE_UNKNOWN
    };
    let mut last = String::new();
    // Video support first (the decoder shares this device); WARP has none.
    for flags in [
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    ] {
        let mut device = None;
        let mut context = None;
        let mut level = D3D_FEATURE_LEVEL::default();
        let adapter: Option<IDXGIAdapter> = adapter.and_then(|adapter| adapter.cast().ok());
        match unsafe {
            D3D11CreateDevice(
                adapter.as_ref(),
                driver,
                HMODULE::default(),
                flags,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )
        } {
            Ok(()) => {
                return Ok((
                    created("D3D11CreateDevice", device)?,
                    created("D3D11CreateDevice(context)", context)?,
                    level,
                ));
            }
            Err(error) => last = format!("D3D11CreateDevice({flags:?}): {error}"),
        }
    }
    Err(last)
}

impl Gpu {
    /// Takes the shared-context lock (see [`Self::context_lock`]).
    pub fn lock_context(&self) -> std::sync::MutexGuard<'_, ()> {
        self.context_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn create(choice: &AdapterChoice) -> Result<Self> {
        let factory6 = factory()?;
        let factory: IDXGIFactory2 = hr("IDXGIFactory2", factory6.cast())?;
        if matches!(choice, AdapterChoice::Warp) {
            let (device, context, level) = create_on(None, true)?;
            let dxgi: IDXGIDevice = hr("IDXGIDevice", device.cast())?;
            let adapter = hr("GetAdapter", unsafe { dxgi.GetAdapter() })?;
            let desc = hr("GetDesc", unsafe { adapter.GetDesc() })?;
            return Ok(Self {
                device,
                context,
                factory,
                adapter_name: wide(&desc.Description),
                luid: desc.AdapterLuid,
                warp: true,
                feature_level: level,
                context_lock: std::sync::Arc::default(),
            });
        }
        let chosen = match choice {
            AdapterChoice::Monitor(monitor) => adapter_for_monitor(HMONITOR(*monitor as _))?
                .ok_or_else(|| "no adapter owns the window's monitor".to_owned())?,
            AdapterChoice::Luid(luid) => adapters()?
                .into_iter()
                .find(|(_, facts)| facts.luid == *luid)
                .ok_or_else(|| format!("no adapter with LUID {}", luid_text(*luid)))?,
            AdapterChoice::Name(name) => adapters()?
                .into_iter()
                .find(|(_, facts)| facts.name.to_lowercase().contains(&name.to_lowercase()))
                .ok_or_else(|| format!("no adapter matches {name:?}"))?,
            AdapterChoice::HighPerformance => {
                let adapter: IDXGIAdapter1 = hr("EnumAdapterByGpuPreference", unsafe {
                    factory6.EnumAdapterByGpuPreference(0, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)
                })?;
                let luid = hr("GetDesc1", unsafe { adapter.GetDesc1() })?.AdapterLuid;
                adapters()?
                    .into_iter()
                    .find(|(_, facts)| facts.luid == luid)
                    .ok_or("high-performance adapter vanished")?
            }
            AdapterChoice::Warp => unreachable!(),
        };
        let (adapter, facts) = chosen;
        let (device, context, level) = create_on(Some(&adapter), false)?;
        // Presenter threads (and, where pictures cannot be shared, the
        // decoder) share this device.
        if let Ok(multithread) = device.cast::<ID3D11Multithread>() {
            unsafe {
                let _ = multithread.SetMultithreadProtected(true);
            }
        }
        Ok(Self {
            device,
            context,
            factory,
            adapter_name: facts.name,
            luid: facts.luid,
            warp: false,
            feature_level: level,
            context_lock: std::sync::Arc::default(),
        })
    }

    pub fn texture(
        &self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
        bind: D3D11_BIND_FLAG,
        initial: Option<(&[u8], u32)>,
    ) -> Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let data = initial.map(|(bytes, pitch)| D3D11_SUBRESOURCE_DATA {
            pSysMem: bytes.as_ptr().cast(),
            SysMemPitch: pitch,
            SysMemSlicePitch: 0,
        });
        let mut texture = None;
        hr(
            &format!("CreateTexture2D({format:?} {width}x{height})"),
            unsafe {
                self.device.CreateTexture2D(
                    &desc,
                    data.as_ref().map(std::ptr::from_ref),
                    Some(&mut texture),
                )
            },
        )?;
        created("CreateTexture2D", texture)
    }

    /// A one-slice `Texture2DArray` view (decoder arrays and plain
    /// textures bind alike).
    pub fn srv_slice(
        &self,
        texture: &ID3D11Texture2D,
        format: DXGI_FORMAT,
        slice: u32,
    ) -> Result<ID3D11ShaderResourceView> {
        let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: format,
            ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2DARRAY,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2DArray: D3D11_TEX2D_ARRAY_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                    FirstArraySlice: slice,
                    ArraySize: 1,
                },
            },
        };
        let mut view = None;
        hr(&format!("CreateShaderResourceView({format:?})"), unsafe {
            self.device
                .CreateShaderResourceView(texture, Some(&desc), Some(&mut view))
        })?;
        created("CreateView", view)
    }

    pub fn srv_2d(
        &self,
        texture: &ID3D11Texture2D,
        format: DXGI_FORMAT,
    ) -> Result<ID3D11ShaderResourceView> {
        let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: format,
            ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                },
            },
        };
        let mut view = None;
        hr("CreateShaderResourceView(2d)", unsafe {
            self.device
                .CreateShaderResourceView(texture, Some(&desc), Some(&mut view))
        })?;
        created("CreateView", view)
    }

    pub fn rtv(&self, texture: &ID3D11Texture2D) -> Result<ID3D11RenderTargetView> {
        let mut view = None;
        hr("CreateRenderTargetView", unsafe {
            self.device
                .CreateRenderTargetView(texture, None, Some(&mut view))
        })?;
        created("CreateView", view)
    }

    /// A CPU-readable copy of a texture's region, row by row (tightly
    /// packed, `bytes_per_texel` each). Blocks until the GPU is done.
    pub fn read_region(
        &self,
        source: &ID3D11Texture2D,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        bytes_per_texel: u32,
    ) -> Result<Vec<u8>> {
        let _context = self.lock_context();
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut desc) };
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: desc.Format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        hr("CreateTexture2D(staging)", unsafe {
            self.device
                .CreateTexture2D(&staging_desc, None, Some(&mut staging))
        })?;
        let staging = created("CreateTexture2D(staging)", staging)?;
        let region = D3D11_BOX {
            left: x,
            top: y,
            front: 0,
            right: x + width,
            bottom: y + height,
            back: 1,
        };
        unsafe {
            self.context
                .CopySubresourceRegion(&staging, 0, 0, 0, 0, source, 0, Some(&region));
        }
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        hr("Map(staging)", unsafe {
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        })?;
        let row = (width * bytes_per_texel) as usize;
        let mut out = Vec::with_capacity(row * height as usize);
        for line in 0..height as usize {
            let start = unsafe { (mapped.pData as *const u8).add(line * mapped.RowPitch as usize) };
            out.extend_from_slice(unsafe { std::slice::from_raw_parts(start, row) });
        }
        unsafe { self.context.Unmap(&staging, 0) };
        Ok(out)
    }

    /// Waits until everything submitted so far completed on the GPU.
    pub fn flush_and_wait(&self) -> Result<()> {
        let query = self.event_query()?;
        unsafe { self.context.End(&query) };
        while !self.query_done(&query) {
            std::thread::yield_now();
        }
        Ok(())
    }

    pub fn event_query(&self) -> Result<ID3D11Query> {
        let desc = D3D11_QUERY_DESC {
            Query: D3D11_QUERY_EVENT,
            MiscFlags: 0,
        };
        let mut query = None;
        hr("CreateQuery(event)", unsafe {
            self.device.CreateQuery(&desc, Some(&mut query))
        })?;
        created("CreateQuery", query)
    }

    /// Non-blocking: has the GPU passed this event? (`GetData` returns
    /// S_FALSE, which is `Ok`, until it has; the BOOL is written only then.)
    pub fn query_done(&self, query: &ID3D11Query) -> bool {
        let mut done = windows::Win32::Foundation::BOOL(0);
        let result = unsafe {
            self.context.GetData(
                query,
                Some(std::ptr::from_mut(&mut done).cast()),
                std::mem::size_of::<windows::Win32::Foundation::BOOL>() as u32,
                0,
            )
        };
        result.is_ok() && done.as_bool()
    }
}

super::opaque_debug!(Gpu);
