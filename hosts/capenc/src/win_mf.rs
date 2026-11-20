// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
// Windows software capture adapter (no-NVENC hosts and VMs).
//
// Sibling to `win.rs::run_encode`; the NVENC path is untouched. Everything
// about what to send is the shared software loop
// (`arcen_media::canvas::CanvasEncodeLoop`, ADR 0028), the same loop the
// Linux Xorg and Wayland Piers run: Keel's damage, the canvas planner and
// governor for a Deck that offered `canvas_v1`, and the degenerate
// full-frame path (one monitor-sized OpenH264 session) for an older Deck.
// This file owns only capture.
//
// Per tick:
//   1. WGC.acquire_into hands us a D3D11 BGRA texture holding the newest frame.
//   2. CopyResource -> a CPU-readable staging texture (created lazily).
//   3. When the loop wants a picture: Map(READ) -> BGRA rows -> loop.tick.
//   4. Its records go to stdout (framed-v1, or framed-v2 with the canvas).
//
// A CPU encoder cannot ingest D3D11 surfaces, so the GPU->CPU round trip is
// unavoidable on this path. WGC delivers a frame only when DWM composed a
// change, so a delivered frame is the damage signal; WGC's dirty-region
// reports and DDA's dirty and move rectangles are not read yet, so Keel
// hashes the whole picture.

use std::time::{Duration, Instant};

use arcen_keel::BgraFrame;
use arcen_media::canvas::serve::CanvasCapencArgs;
use arcen_media::capenc_control::CapencControlCommand;
use arcen_media::video::{open_software_loop, EncoderBackend, SoftwareLoop, SoftwareLoopConfig};
use windows::core::Interface;
use windows::Win32::Foundation::{E_INVALIDARG, HMODULE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
};

use crate::log;
use crate::wgc::WgcCapture;
use crate::CursorCaptureMode;

/// Runtime parameters for the software path. Kept small and explicit so
/// callers (win.rs::run) can construct one from CLI args + defaults without
/// pulling the whole config surface into this module.
pub(crate) struct OpenH264RunOpts {
    pub output_index: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub framed: bool,
    /// framed-v2 stdout (`arcen_media::capenc_stream`), with the canvas.
    pub framed_v2: bool,
    /// The canvas for a Deck that offered it (ADR 0028).
    pub canvas: Option<CanvasCapencArgs>,
    pub adapter_hint: Option<String>,
    pub adapter_output_index: Option<u32>,
    pub device_name: Option<String>,
    pub cursor_mode: CursorCaptureMode,
    /// The negotiated colour contract for this run — resolved once by the
    /// caller (`win.rs::run_with_args`, via `crate::requested_color`) from
    /// argv, so this struct never re-derives its own, potentially
    /// disagreeing, `ColorSpec::legacy(...)`.
    pub color: crate::ColorSpec,
}

const KEEPALIVE: Duration = Duration::from_secs(1);

pub(crate) fn run_openh264(opts: OpenH264RunOpts) -> ! {
    std::process::exit(run_inner(opts))
}

/// The OpenH264 admission probe: the exact output is bound as for a live
/// run, then the shared software probe encodes the same representative
/// pictures every Pier's software session is measured with.
pub(crate) fn run_openh264_admission_probe(
    opts: OpenH264RunOpts,
    probe: &crate::admission_probe::AdmissionProbeOptions,
) -> i32 {
    match unsafe {
        pick_device(
            opts.output_index,
            opts.adapter_hint.as_deref(),
            opts.adapter_output_index,
            opts.device_name.as_deref(),
        )
    } {
        Ok((_device, _context, adapter_desc, _monitor)) => log(&format!(
            "software admission bound exact output on adapter {adapter_desc:?}"
        )),
        Err(error) => {
            log(&format!(
                "software admission exact-output resolution failed: {error:?}"
            ));
            return 2;
        }
    }
    crate::admission_probe::run_software_probe(
        opts.color.video(arcen_media::VideoCodec::H264),
        opts.fps,
        probe,
    )
}

#[allow(clippy::too_many_lines)]
fn run_inner(opts: OpenH264RunOpts) -> i32 {
    let backend_label = "OpenH264";
    // Keep the (virtual) display awake so WGC keeps receiving DWM composites.
    unsafe {
        SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED);
    }
    log(&format!(
        "{backend_label}: display keep-awake armed (ES_DISPLAY_REQUIRED|ES_CONTINUOUS)"
    ));

    let (device, context, adapter_desc, monitor) = match unsafe {
        pick_device(
            opts.output_index,
            opts.adapter_hint.as_deref(),
            opts.adapter_output_index,
            opts.device_name.as_deref(),
        )
    } {
        Ok(v) => v,
        Err(e) => {
            log(&format!("{backend_label}: adapter init failed: {e:?}"));
            return 2;
        }
    };
    log(&format!("{backend_label}: bound adapter '{adapter_desc}'"));

    let capture = match unsafe {
        // Software encode paths stay 8-bit: they have no wide surface
        // format to hand a wide capture to, so asking for one would cost
        // bandwidth and buy nothing.
        WgcCapture::from_device(
            device.clone(),
            context.clone(),
            monitor,
            opts.cursor_mode,
            false,
        )
    } {
        Ok(c) => c,
        Err(e) => {
            log(&format!("{backend_label}: WGC init failed: {e:?}"));
            return 2;
        }
    };
    let (width, height) = (capture.width, capture.height);
    if width == 0 || height == 0 {
        log(&format!(
            "{backend_label}: capture reported invalid size {width}x{height}"
        ));
        return 3;
    }

    let SoftwareLoop {
        mut encode,
        bitrate_bounds: (start_bps, ceiling_bps),
        threading,
    } = match open_software_loop(SoftwareLoopConfig {
        width,
        height,
        fps: opts.fps,
        video: opts.color.video(arcen_media::VideoCodec::H264),
        canvas: opts.canvas,
        start_bitrate_bps: (opts.bitrate_kbps != 0).then(|| u64::from(opts.bitrate_kbps) * 1_000),
        keepalive: KEEPALIVE,
    }) {
        Ok(built) => built,
        Err(error) => {
            log(&format!("software encoder init failed: {error}"));
            return 4;
        }
    };
    // `opts.color` — the exact value the session was just opened with —
    // not a separately re-derived `ColorSpec::legacy(...)`, so the READY
    // line and the actual encode cannot disagree.
    let ready_plan = match crate::resolved_media_plan(
        EncoderBackend::OpenH264,
        "h264",
        opts.color,
        width,
        height,
        opts.fps,
        opts.cursor_mode,
    ) {
        Ok(plan) => plan,
        Err(error) => {
            log(&error);
            return 4;
        }
    };

    let control = crate::spawn_control_thread("software encoder");
    log(&format!(
        "{backend_label}: software bitrate policy: start={start_bps}bps ceiling={ceiling_bps}bps {threading}"
    ));
    match opts.canvas {
        Some(canvas) => log(&format!(
            "software loop: canvas=on monitor={} max_surfaces={} cache_bytes={} cpu_share={}",
            canvas.monitor_id,
            canvas.terms.max_surfaces_per_monitor,
            canvas.terms.cache_bytes,
            canvas.cpu_share.percent()
        )),
        None => log("software loop: canvas=off (full frames)"),
    }

    let mut target_dt = crate::frame_interval_from_fps(opts.fps);
    let mut next = Instant::now();
    let mut staging: Option<ID3D11Texture2D> = None;
    // No desktop frame after a second: the loop is fed black until one comes.
    let mut black: Option<Vec<u8>> = None;
    let started = Instant::now();
    let mut sec = Instant::now();
    let mut dbg = (0u64, 0u64, 0u64);
    let mut stdout = std::io::stdout();
    let mut ready_announced = false;
    let mut damage_pending = true;
    let mut changed_frames = 0u64;
    let mut emitted = 0u64;
    let mut bytes_sum = 0u64;
    let mut last_error: Option<Instant> = None;

    let mut cap = capture;
    while !control.stop_requested() {
        let mut stage_error: Option<String> = None;
        let _ = unsafe {
            cap.acquire_into(&mut dbg, None, &mut |tex: &ID3D11Texture2D| {
                if let Err(error) = stage_texture(
                    &device,
                    &context,
                    tex,
                    &mut staging,
                    width as usize,
                    height as usize,
                ) {
                    stage_error = Some(format!("{error:?}"));
                } else {
                    // WGC hands over a frame only when DWM composed one.
                    damage_pending = true;
                    black = None;
                }
            })
        };
        if let Some(error) = stage_error {
            log(&format!("{backend_label}: stage error: {error}"));
            return 3;
        }
        if staging.is_none() && black.is_none() && started.elapsed() >= Duration::from_secs(1) {
            black = Some([0u8, 0, 0, 0xFF].repeat(width as usize * height as usize));
            damage_pending = true;
            log(&format!(
                "{backend_label}: no desktop frame after 1s — streaming black until content arrives"
            ));
        }

        let now = Instant::now();
        if let Some(fps) = control.take_framerate_fps() {
            if encode.set_frame_rate(fps) {
                target_dt = crate::frame_interval_from_fps(fps);
                log(&format!(
                    "{backend_label}: live framerate reconfigured: fps={fps}"
                ));
            }
        }
        if let Some(bps) = control.take_bitrate_bps() {
            encode.set_link_bitrate(bps);
            log(&format!("{backend_label}: live bitrate: target={bps}bps"));
        }
        for command in control.take_canvas_controls() {
            match command {
                CapencControlCommand::CanvasInput {
                    monitor_id,
                    topology_generation,
                    x,
                    y,
                } => encode.note_input(monitor_id, topology_generation, x, y, now),
                command => {
                    if let Some(message) = command.canvas_control() {
                        encode.on_control(&message);
                    }
                }
            }
        }
        if control.take_idr() {
            encode.request_recovery();
        }
        if control.take_input_activity() {
            damage_pending = true;
        }

        if now >= next {
            let have_picture = staging.is_some() || black.is_some();
            if have_picture && encode.wants_frame(now, damage_pending) {
                let fresh = damage_pending;
                damage_pending = false;
                let tick = match (staging.as_ref(), black.as_deref()) {
                    (Some(staging), _) => unsafe {
                        with_mapped_staging(&context, staging, |bgra, stride| {
                            let frame =
                                BgraFrame::new(bgra, width as usize, height as usize, stride)
                                    .map_err(|error| {
                                        windows::core::Error::new(E_INVALIDARG, error.to_string())
                                    })?;
                            Ok(encode.tick(frame, None, now))
                        })
                    },
                    (None, Some(black)) => {
                        BgraFrame::new(black, width as usize, height as usize, width as usize * 4)
                            .map(|frame| encode.tick(frame, None, now))
                            .map_err(|error| {
                                windows::core::Error::new(E_INVALIDARG, error.to_string())
                            })
                    }
                    (None, None) => unreachable!("a picture is held"),
                };
                let tick = match tick {
                    Ok(tick) => tick,
                    Err(error) => {
                        log(&format!("{backend_label}: staging map error: {error:?}"));
                        return 3;
                    }
                };
                match tick {
                    Ok(tick) => {
                        if fresh && !tick.records.is_empty() {
                            changed_frames += 1;
                        }
                        if let Some(event) = tick.event {
                            log(&format!("canvas governor: {event:?}"));
                        }
                        if let Some((generation, digest)) = tick.digest {
                            log(&format!(
                                "canvas mirror digest {}",
                                arcen_media::canvas::digest_fields(generation, &digest)
                            ));
                        }
                        for record in &tick.records {
                            if !ready_announced {
                                if crate::announce_ready_from(
                                    ready_plan,
                                    // This path builds its source with
                                    // `WgcCapture::from_device` unconditionally.
                                    Some(
                                        arcen_media::video::CaptureBackend::WindowsGraphicsCapture,
                                    ),
                                )
                                .is_err()
                                {
                                    log("could not emit READY before the first record");
                                    return 5;
                                }
                                ready_announced = true;
                            }
                            if crate::write_loop_record(
                                &mut stdout,
                                record,
                                opts.framed,
                                opts.framed_v2,
                            )
                            .is_err()
                            {
                                return 0;
                            }
                            bytes_sum += record.bytes().len() as u64;
                        }
                        if !tick.records.is_empty() {
                            emitted += 1;
                        }
                    }
                    Err(arcen_media::canvas::LoopError::Geometry) => {
                        log("software loop: capture size changed");
                        return 3;
                    }
                    Err(error) => {
                        // The loop recovers by itself at its next tick.
                        damage_pending = true;
                        if last_error.is_none_or(|at| at.elapsed() >= Duration::from_secs(1)) {
                            log(&format!("software loop tick failed: {error}"));
                            last_error = Some(Instant::now());
                        }
                    }
                }
            }
            next += target_dt;
            if next < now {
                next = now + target_dt;
            }
        }

        if sec.elapsed() >= Duration::from_secs(1) {
            let canvas_on = encode.is_canvas();
            let state = encode.governor_state();
            log(&format!(
                "enc_fps={emitted} kbps={} cap_fps={changed_frames} capture_behind={} \
                 capture_new={} capture_empty={} {}",
                bytes_sum * 8 / 1000,
                cap.take_raw_frames_superseded(),
                dbg.0,
                dbg.1 + dbg.2,
                encode.take_stats().line(canvas_on, state),
            ));
            dbg = (0, 0, 0);
            emitted = 0;
            bytes_sum = 0;
            changed_frames = 0;
            sec = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    log("software encoder control closed; dropping capture and encoder before exit");
    0
}

/// Copy the newest WGC texture into a reusable CPU-readable staging texture.
unsafe fn stage_texture(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    src: &ID3D11Texture2D,
    staging: &mut Option<ID3D11Texture2D>,
    width: usize,
    height: usize,
) -> windows::core::Result<()> {
    let mut src_desc = D3D11_TEXTURE2D_DESC::default();
    src.GetDesc(&mut src_desc);
    if src_desc.Width as usize != width || src_desc.Height as usize != height {
        return Err(windows::core::Error::new(
            E_INVALIDARG,
            format!(
                "capture size {}x{} differs from MF surface {width}x{height}",
                src_desc.Width, src_desc.Height
            ),
        ));
    }

    // Recreate staging if it's missing or size changed.
    let needs_new = match staging.as_ref() {
        None => true,
        Some(existing) => {
            let mut d = D3D11_TEXTURE2D_DESC::default();
            existing.GetDesc(&mut d);
            d.Width != src_desc.Width || d.Height != src_desc.Height || d.Format != src_desc.Format
        }
    };
    if needs_new {
        let mut desc = src_desc;
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let mut tex: Option<ID3D11Texture2D> = None;
        device.CreateTexture2D(&desc, None, Some(&mut tex))?;
        *staging = Some(tex.expect("staging texture"));
    }
    let stage = staging.as_ref().expect("staging texture ready");

    let src_res: ID3D11Resource = src.cast()?;
    let dst_res: ID3D11Resource = stage.cast()?;
    context.CopyResource(&dst_res, &src_res);
    Ok(())
}

/// Map the retained staging texture and run one synchronous pixel operation.
///
/// The staging texture remains valid after this call, allowing forced IDR and
/// periodic full refreshes even when WGC has not delivered another frame.
unsafe fn with_mapped_staging<T>(
    context: &ID3D11DeviceContext,
    staging: &ID3D11Texture2D,
    operation: impl FnOnce(&[u8], usize) -> windows::core::Result<T>,
) -> windows::core::Result<T> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    staging.GetDesc(&mut desc);
    let resource: ID3D11Resource = staging.cast()?;
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    context.Map(&resource, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
    if mapped.pData.is_null() {
        // The Map call SUCCEEDED, so the subresource is mapped and must be
        // unmapped even on this error path — a stuck mapping fails every
        // subsequent Map and kills the encode loop.
        context.Unmap(&resource, 0);
        return Err(windows::core::Error::new(
            windows::Win32::Foundation::E_POINTER,
            "staging Map returned null",
        ));
    }
    let src_stride = mapped.RowPitch as usize;
    let src_len = src_stride * (desc.Height as usize);
    let bgra = std::slice::from_raw_parts(mapped.pData as *const u8, src_len);
    let result = operation(bgra, src_stride);
    context.Unmap(&resource, 0);
    result
}

/// Pick the D3D11 device and monitor for MF live encode.
///
/// Pier supplies adapter name + adapter-local output + Win32 device name after
/// resolving the configured target in the interactive session. Standalone
/// invocations retain the positional global attached-output index. Delegates
/// the actual enumeration/match/ambiguity decision to
/// `crate::win::resolve_output` — the same resolution DDA/WGC use — so an
/// `adapter=`/`adapter-output=`/`device=` selector picks the identical DXGI
/// output regardless of which encoder backend this process ends up running.
///
/// Returns (device, context, adapter_name, selected_monitor).
unsafe fn pick_device(
    global_output_index: u32,
    adapter_hint: Option<&str>,
    adapter_output_index: Option<u32>,
    device_name: Option<&str>,
) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext, String, HMONITOR)> {
    let selector = crate::win::OutputSelector {
        global_output_index,
        adapter_hint,
        adapter_output_index,
        device_name,
    };
    let resolved = crate::win::resolve_output(&selector)?;
    log(&format!(
        "software encoder: bound {} -> adapter {} ({:?})",
        selector.describe(),
        resolved.adapter_index,
        resolved.adapter_name
    ));

    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    D3D11CreateDevice(
        &resolved.adapter,
        D3D_DRIVER_TYPE_UNKNOWN,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        None,
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        Some(&mut context),
    )?;
    Ok((
        device.expect("D3D11 device"),
        context.expect("D3D11 context"),
        resolved.adapter_name,
        resolved.monitor,
    ))
}

#[cfg(test)]
mod tests {
    use arcen_keel::scenario::{Scenario, ScenarioKind};
    use arcen_keel::{BgraFrame, DamageTracker, KernelPreference};
    use arcen_media::video::ColorTransform;
    use arcen_media::video::{convert_bgra_to_i420, convert_bgra_to_i420_rows, I420FrameMut};

    /// Converts `source` (all rows, or only `rows`) into the I420 planes.
    fn bgra_to_i420(
        source: &[u8],
        source_stride: usize,
        planes: (&mut [u8], &mut [u8], &mut [u8]),
        width: usize,
        height: usize,
        rows: Option<std::ops::Range<usize>>,
    ) {
        let frame = BgraFrame::new(source, width, height, source_stride).expect("BGRA");
        let (y, u, v) = planes;
        let mut destination = I420FrameMut::new(
            width as u32,
            height as u32,
            y,
            width,
            u,
            width / 2,
            v,
            width / 2,
        )
        .expect("I420");
        let transform = ColorTransform::legacy_bt709_limited();
        match rows {
            Some(rows) => convert_bgra_to_i420_rows(frame, &mut destination, rows, transform),
            None => convert_bgra_to_i420(frame, &mut destination, transform),
        }
        .expect("conversion");
    }

    #[test]
    fn selective_conversion_matches_full_conversion_for_keel_corpus() {
        const WIDTH: usize = 96;
        const HEIGHT: usize = 64;
        for preference in [KernelPreference::Xxh3, KernelPreference::Crc32c] {
            for kind in [
                ScenarioKind::Idle,
                ScenarioKind::Typing,
                ScenarioKind::Drag,
                ScenarioKind::Scroll,
                ScenarioKind::Video,
                ScenarioKind::Burst,
            ] {
                let scenario = Scenario::new(WIDTH, HEIGHT, kind, 42);
                let mut tracker =
                    DamageTracker::new(WIDTH, HEIGHT, preference).expect("valid tracker");
                let mut bgra = Vec::new();
                let mut selected = (
                    vec![0u8; WIDTH * HEIGHT],
                    vec![0u8; WIDTH * HEIGHT / 4],
                    vec![0u8; WIDTH * HEIGHT / 4],
                );

                for tick in 0..16 {
                    scenario.render(tick, &mut bgra);
                    let frame =
                        BgraFrame::new(&bgra, WIDTH, HEIGHT, scenario.stride()).expect("frame");
                    tracker.update(frame).expect("damage update");
                    for rows in tracker.damage_map().dirty_block_rows() {
                        bgra_to_i420(
                            &bgra,
                            scenario.stride(),
                            (&mut selected.0, &mut selected.1, &mut selected.2),
                            WIDTH,
                            HEIGHT,
                            Some(rows),
                        );
                    }

                    let mut full = (
                        vec![0u8; WIDTH * HEIGHT],
                        vec![0u8; WIDTH * HEIGHT / 4],
                        vec![0u8; WIDTH * HEIGHT / 4],
                    );
                    bgra_to_i420(
                        &bgra,
                        scenario.stride(),
                        (&mut full.0, &mut full.1, &mut full.2),
                        WIDTH,
                        HEIGHT,
                        None,
                    );
                    assert_eq!(selected, full, "{preference:?} {kind:?} tick {tick}");
                }
            }
        }
    }
}
