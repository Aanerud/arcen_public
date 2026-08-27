// Windows.Graphics.Capture (WGC) capture backend.
//
// WHY THIS EXISTS: DXGI Desktop Duplication (win.rs `Capture`) is the lowest-
// latency path on bare-metal, but on headless NVIDIA GRID / vGPU consoles (and
// RDP sessions) `AcquireNextFrame` times out FOREVER — the desktop is composited
// by DWM (GDI BitBlt can read it) but no present/flip reaches the duplication
// path, so zero frames are delivered. WGC talks to DWM directly and DOES deliver
// frames in exactly those environments (proven on pier-windows.example.internal: RTX6000-8Q vGPU).
//
// This backend produces the SAME contract as `Capture`: it owns a D3D11 device +
// context (on the NVIDIA adapter, shared with the NVENC encoder) and hands the
// caller a live `ID3D11Texture2D` per new frame via `acquire_into`, so the
// encoder can `CopyResource` it into its registered input with zero CPU copy.
//
// Design notes:
//  * FreeThreaded frame pool -> no DispatcherQueue / message pump required.
//  * We POLL `TryGetNextFrame` on the encode thread (single-threaded use of the
//    D3D11 immediate context — no cross-thread context races).
//  * Cursor capture is fixed at startup from the connection negotiation.
//  * The capture border (Win11 yellow highlight) is disabled when the runtime
//    allows it; failure is non-fatal.

use crate::log;
use crate::CursorCaptureMode;

use windows::core::Interface;
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

pub struct WgcCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    pub width: u32,
    pub height: u32,
    _item: GraphicsCaptureItem,
    frame_pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
}

impl WgcCapture {
    /// Build a WGC capture over the selected monitor, using the supplied D3D11
    /// device/context (created on the NVENC adapter in win.rs). The frame pool
    /// is created on the same device so captured textures need no cross-device
    /// copy before encode.
    pub unsafe fn from_device(
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        monitor: HMONITOR,
        cursor_mode: CursorCaptureMode,
    ) -> windows::core::Result<Self> {
        // WinRT must be initialised on this thread. RPC_E_CHANGED_MODE means it
        // was already initialised in a different apartment — harmless for our
        // free-threaded use, so ignore it.
        let _ = RoInitialize(RO_INIT_MULTITHREADED);

        // Wrap the D3D11 device as a WinRT IDirect3DDevice.
        let dxgi_device: IDXGIDevice = device.cast()?;
        let inspectable = CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)?;
        let d3d_device: IDirect3DDevice = inspectable.cast()?;

        // Capture item for the exact desktop output selected by the capenc CLI.
        let interop: IGraphicsCaptureItemInterop =
            windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = interop.CreateForMonitor(monitor)?;
        let size: SizeInt32 = item.Size()?;
        let width = size.Width.max(0) as u32;
        let height = size.Height.max(0) as u32;
        log(&format!(
            "WGC item: selected monitor {}x{} (hmon={:?})",
            width, height, monitor.0
        ));

        // Free-threaded pool: TryGetNextFrame works without a DispatcherQueue.
        // BGRA matches the NVENC ARGB register path.
        let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &d3d_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )?;
        let session = frame_pool.CreateCaptureSession(&item)?;

        let include_cursor = cursor_mode.include_cursor();
        session.SetIsCursorCaptureEnabled(include_cursor)?;
        log(&format!(
            "WGC: cursor capture {}",
            if include_cursor {
                "enabled"
            } else {
                "disabled"
            }
        ));
        // Drop the capture border when the runtime allows it (Win11); non-fatal.
        if let Err(e) = session.SetIsBorderRequired(false) {
            log(&format!(
                "WGC: SetIsBorderRequired(false) unavailable: {e:?}"
            ));
        }

        // Keep the item alive; if the monitor is removed WGC raises Closed.
        item.Closed(&TypedEventHandler::<GraphicsCaptureItem, _>::new(
            |_item, _| {
                log("WGC: capture item closed (monitor removed?)");
                Ok(())
            },
        ))?;

        session.StartCapture()?;
        log(&format!("WGC capture started: {}x{}", width, height));

        Ok(Self {
            device,
            context,
            width,
            height,
            _item: item,
            frame_pool,
            session,
        })
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }
    pub fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }

    /// Poll for the newest captured frame. On a new frame, extract its
    /// `ID3D11Texture2D` and hand it to `on_new` (which stages it into the
    /// encoder input) BEFORE closing the frame, then return true. Returns false
    /// when no frame is queued (the caller re-encodes the last staged frame to
    /// keep the stream flowing). `dbg` = (new, timeout, cursor_only).
    pub unsafe fn acquire_into(
        &mut self,
        dbg: &mut (u64, u64, u64),
        on_new: &mut dyn FnMut(&ID3D11Texture2D),
    ) -> windows::core::Result<bool> {
        // Drain to the newest frame so we never fall behind the pool.
        let mut newest: Option<windows::Graphics::Capture::Direct3D11CaptureFrame> = None;
        while let Ok(frame) = self.frame_pool.TryGetNextFrame() {
            // Close the previous (now stale) frame before overwriting.
            if let Some(prev) = newest.take() {
                let _ = prev.Close();
            }
            newest = Some(frame);
        }
        match newest {
            Some(frame) => {
                let surface = frame.Surface()?;
                let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
                let tex: ID3D11Texture2D = access.GetInterface()?;
                dbg.0 += 1;
                on_new(&tex);
                frame.Close()?;
                Ok(true)
            }
            None => {
                dbg.1 += 1;
                // TryGetNextFrame returns instantly (unlike DXGI's blocking
                // AcquireNextFrame), so without this the encode loop busy-spins a
                // whole core. 1ms keeps latency well under the frame budget while
                // capping poll rate to ~1k/s.
                std::thread::sleep(std::time::Duration::from_millis(1));
                Ok(false)
            }
        }
    }
}

impl Drop for WgcCapture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.frame_pool.Close();
    }
}
