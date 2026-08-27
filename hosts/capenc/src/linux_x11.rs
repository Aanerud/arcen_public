//! Dedicated-Xorg capture and portable OpenH264 encoding.

use std::os::fd::AsRawFd;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use arcen_keel::{ActivityHint, BgraFrame, EmitMode, IdleCadence};
use arcen_media::video::{
    convert_bgra_to_i420, convert_bgra_to_i420_rows, EncoderBackend, I420Frame, I420FrameMut,
    ResolvedMediaPlan, SoftwareH264Config, SoftwareH264Encoder,
};
use arcen_media::ForcedKeyframe;
use x11rb::connection::Connection as _;
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    ConnectionExt as _, ImageFormat, ImageOrder, VisualClass, Visualid, Window,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

const MAX_WIDTH: u32 = 1920;
const MAX_HEIGHT: u32 = 1200;
const MAX_FPS: u32 = 30;
const KEEPALIVE: Duration = Duration::from_secs(1);
/// Bounded recovery keyframe interval for one region's activity scheduler.
/// Long enough that a busy region's bitrate is unchanged in practice, short
/// enough that a suppressed static region can never go without a complete
/// picture for an unbounded time.
const REGION_KEYFRAME_INTERVAL: Duration = Duration::from_secs(10);
/// How long input/focus activity keeps a region responsive without new pixels.
const REGION_INPUT_WAKE_GRACE: Duration = Duration::from_millis(100);
const MAX_CAPTURE_BYTES: usize = 1920 * 1200 * 4;
const RED_MASK: u32 = 0x00ff_0000;
const GREEN_MASK: u32 = 0x0000_ff00;
const BLUE_MASK: u32 = 0x0000_00ff;
static DAMAGE_DEGRADED_WARNED: AtomicBool = AtomicBool::new(false);
static SHM_DEGRADED_WARNED: AtomicBool = AtomicBool::new(false);

fn warn_once(flag: &AtomicBool, message: impl FnOnce() -> String) {
    if flag
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        crate::log(&message());
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CaptureRect {
    x: i16,
    y: i16,
    width: u16,
    height: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PixelLayout {
    depth: u8,
    visual: Visualid,
    stride: usize,
    byte_len: usize,
}

fn checked_layout(
    width: u16,
    height: u16,
    depth: u8,
    bits_per_pixel: u8,
    scanline_pad: u8,
) -> Result<PixelLayout, String> {
    if width == 0
        || height == 0
        || depth != 24
        || bits_per_pixel != 32
        || !matches!(scanline_pad, 8 | 16 | 32)
    {
        return Err("unsupported X11 depth, bits-per-pixel, or scanline padding".to_string());
    }
    let row_bits = usize::from(width)
        .checked_mul(usize::from(bits_per_pixel))
        .ok_or_else(|| "X11 row geometry overflow".to_string())?;
    let pad = usize::from(scanline_pad);
    let stride_bits = row_bits
        .checked_add(pad - 1)
        .ok_or_else(|| "X11 stride overflow".to_string())?
        / pad
        * pad;
    let stride = stride_bits / 8;
    let byte_len = stride
        .checked_mul(usize::from(height))
        .ok_or_else(|| "X11 image length overflow".to_string())?;
    if byte_len == 0 || byte_len > MAX_CAPTURE_BYTES {
        return Err("X11 image exceeds the bounded software capture buffer".to_string());
    }
    Ok(PixelLayout {
        depth,
        visual: 0,
        stride,
        byte_len,
    })
}

/// One owned mapping for an MIT-SHM 1.2 server-created segment.
///
/// Invariants:
/// - `address` is non-null, page-mapped for exactly `len` readable/writable bytes;
/// - the mapping remains live until `Drop` calls `munmap` exactly once;
/// - safe borrows require `&mut self`, so no borrow can overlap the next XShm
///   request that lets the server mutate the segment;
/// - no raw pointer is exposed by the safe API.
struct MmapRegion {
    address: NonNull<u8>,
    len: usize,
}

impl MmapRegion {
    fn map(fd: &impl AsRawFd, len: usize) -> Result<Self, String> {
        if len == 0 {
            return Err("cannot map an empty XShm segment".to_string());
        }
        // SAFETY: `fd` is the live descriptor returned by MIT-SHM 1.2 for a
        // segment of at least `len` bytes. The mapping is checked against
        // MAP_FAILED and is exclusively owned by the returned wrapper.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(format!(
                "mmap MIT-SHM segment: {}",
                std::io::Error::last_os_error()
            ));
        }
        let address = NonNull::new(address.cast::<u8>())
            .ok_or_else(|| "mmap returned a null address".to_string())?;
        Ok(Self { address, len })
    }

    fn bytes(&mut self) -> &[u8] {
        // SAFETY: construction proves the mapping covers `len` initialized
        // bytes. `&mut self` prevents another XShm request through this owner
        // while the returned slice is borrowed.
        unsafe { std::slice::from_raw_parts(self.address.as_ptr(), self.len) }
    }
}

impl Drop for MmapRegion {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns the exact mapping and Drop runs once.
        let _ = unsafe { libc::munmap(self.address.as_ptr().cast(), self.len) };
    }
}

enum Transfer {
    Shm {
        segment: shm::Seg,
        mapping: MmapRegion,
    },
    GetImage {
        storage: Vec<u8>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Activity {
    None,
    Damage,
    Modeset,
}

struct X11Capture {
    connection: RustConnection,
    root: Window,
    output_index: u32,
    rect: CaptureRect,
    layout: PixelLayout,
    damage: Option<damage::Damage>,
    transfer: Transfer,
}

impl X11Capture {
    fn connect(output_index: u32) -> Result<Self, String> {
        let (connection, screen_index) =
            x11rb::connect(None).map_err(|error| format!("connect authenticated X11: {error}"))?;
        let screen = connection
            .setup()
            .roots
            .get(screen_index)
            .ok_or_else(|| "X11 selected screen is absent".to_string())?;
        let root = screen.root;
        let rect = selected_output_rect(&connection, root, output_index)?;
        validate_software_geometry(rect)?;

        let setup = connection.setup();
        if setup.image_byte_order != ImageOrder::LSB_FIRST {
            return Err("X11 image byte order is not little-endian BGRA".to_string());
        }
        let format = setup
            .pixmap_formats
            .iter()
            .find(|format| format.depth == screen.root_depth)
            .ok_or_else(|| "X11 root depth has no pixmap format".to_string())?;
        let mut layout = checked_layout(
            rect.width,
            rect.height,
            screen.root_depth,
            format.bits_per_pixel,
            format.scanline_pad,
        )?;
        let visual = screen
            .allowed_depths
            .iter()
            .flat_map(|depth| depth.visuals.iter())
            .find(|visual| visual.visual_id == screen.root_visual)
            .ok_or_else(|| "X11 root visual is absent from allowed depths".to_string())?;
        if visual.class != VisualClass::TRUE_COLOR
            || visual.red_mask != RED_MASK
            || visual.green_mask != GREEN_MASK
            || visual.blue_mask != BLUE_MASK
        {
            return Err("X11 root visual is not 8-bit little-endian BGRX".to_string());
        }
        layout.visual = visual.visual_id;
        validate_root_bounds(screen.width_in_pixels, screen.height_in_pixels, rect)?;

        randr::ConnectionExt::randr_select_input(
            &connection,
            root,
            randr::NotifyMask::SCREEN_CHANGE
                | randr::NotifyMask::CRTC_CHANGE
                | randr::NotifyMask::OUTPUT_CHANGE
                | randr::NotifyMask::RESOURCE_CHANGE,
        )
        .map_err(|error| format!("select XRandR events: {error}"))?
        .check()
        .map_err(|error| format!("select XRandR events: {error}"))?;

        let damage = install_damage(&connection, root)?;
        let transfer = match install_shm(&connection, layout.byte_len) {
            Ok(transfer) => transfer,
            Err(error) => {
                warn_once(&SHM_DEGRADED_WARNED, || {
                    format!("WARNING: MIT-SHM 1.2 unavailable ({error}); using bounded XGetImage")
                });
                Transfer::GetImage {
                    storage: allocate_zeroed(layout.byte_len)?,
                }
            }
        };
        connection
            .flush()
            .map_err(|error| format!("flush X11 capture setup: {error}"))?;
        crate::log(&format!(
            "X11 capture ready: rect={}x{}+{}+{} stride={} depth={} visual={:#x} transfer={}",
            rect.width,
            rect.height,
            rect.x,
            rect.y,
            layout.stride,
            layout.depth,
            layout.visual,
            if matches!(transfer, Transfer::Shm { .. }) {
                "mit-shm-1.2"
            } else {
                "get-image"
            }
        ));
        Ok(Self {
            connection,
            root,
            output_index,
            rect,
            layout,
            damage,
            transfer,
        })
    }

    fn poll_activity(&self) -> Result<Activity, String> {
        let mut result = Activity::None;
        while let Some(event) = self
            .connection
            .poll_for_event()
            .map_err(|error| format!("poll X11 capture event: {error}"))?
        {
            match event {
                Event::DamageNotify(event) if Some(event.damage) == self.damage => {
                    if result != Activity::Modeset {
                        result = Activity::Damage;
                    }
                    if let Some(damage) = self.damage {
                        self.connection
                            .damage_subtract(damage, 0u32, 0u32)
                            .map_err(|error| format!("subtract XDamage region: {error}"))?
                            .check()
                            .map_err(|error| format!("subtract XDamage region: {error}"))?;
                    }
                }
                Event::RandrNotify(_) | Event::RandrScreenChangeNotify(_) => {
                    result = Activity::Modeset;
                }
                _ => {}
            }
        }
        Ok(result)
    }

    fn capture(&mut self) -> Result<BgraFrame<'_>, String> {
        let bytes = match &mut self.transfer {
            Transfer::Shm { segment, mapping } => {
                let reply = self
                    .connection
                    .shm_get_image(
                        self.root,
                        self.rect.x,
                        self.rect.y,
                        self.rect.width,
                        self.rect.height,
                        u32::MAX,
                        u8::from(ImageFormat::Z_PIXMAP),
                        *segment,
                        0,
                    )
                    .map_err(|error| format!("request XShmGetImage: {error}"))?
                    .reply()
                    .map_err(|error| format!("XShmGetImage: {error}"))?;
                validate_image_reply(
                    reply.depth,
                    reply.visual,
                    usize::try_from(reply.size)
                        .map_err(|_| "XShm image size overflow".to_string())?,
                    self.layout,
                )?;
                mapping.bytes()
            }
            Transfer::GetImage { storage } => {
                let reply = self
                    .connection
                    .get_image(
                        ImageFormat::Z_PIXMAP,
                        self.root,
                        self.rect.x,
                        self.rect.y,
                        self.rect.width,
                        self.rect.height,
                        u32::MAX,
                    )
                    .map_err(|error| format!("request XGetImage: {error}"))?
                    .reply()
                    .map_err(|error| format!("XGetImage: {error}"))?;
                validate_image_reply(reply.depth, reply.visual, reply.data.len(), self.layout)?;
                storage.clear();
                storage
                    .try_reserve_exact(reply.data.len())
                    .map_err(|_| "reserve bounded XGetImage storage".to_string())?;
                storage.extend_from_slice(&reply.data);
                storage.as_slice()
            }
        };
        BgraFrame::new(
            bytes,
            usize::from(self.rect.width),
            usize::from(self.rect.height),
            self.layout.stride,
        )
        .map_err(|error| format!("validate captured BGRA frame: {error}"))
    }

    fn recreate(self) -> Result<Self, String> {
        let expected = self.rect;
        let output_index = self.output_index;
        drop(self);
        let replacement = Self::connect(output_index)?;
        validate_recreated_rect(expected, replacement.rect)?;
        Ok(replacement)
    }
}

impl Drop for X11Capture {
    fn drop(&mut self) {
        if let Some(damage) = self.damage.take() {
            let _ = self.connection.damage_destroy(damage);
        }
        if let Transfer::Shm { segment, .. } = &self.transfer {
            let _ = self.connection.shm_detach(*segment);
        }
        let _ = self.connection.flush();
    }
}

fn selected_output_rect(
    connection: &RustConnection,
    root: Window,
    output_index: u32,
) -> Result<CaptureRect, String> {
    let resources = connection
        .randr_get_screen_resources_current(root)
        .map_err(|error| format!("request XRandR resources: {error}"))?
        .reply()
        .map_err(|error| format!("XRandR resources: {error}"))?;
    let mut connected_index = 0u32;
    for output in resources.outputs {
        let info = connection
            .randr_get_output_info(output, resources.config_timestamp)
            .map_err(|error| format!("request XRandR output: {error}"))?
            .reply()
            .map_err(|error| format!("XRandR output: {error}"))?;
        if info.connection != randr::Connection::CONNECTED || info.crtc == 0 {
            continue;
        }
        if connected_index != output_index {
            connected_index = connected_index.saturating_add(1);
            continue;
        }
        let crtc = connection
            .randr_get_crtc_info(info.crtc, resources.config_timestamp)
            .map_err(|error| format!("request XRandR CRTC: {error}"))?
            .reply()
            .map_err(|error| format!("XRandR CRTC: {error}"))?;
        return Ok(CaptureRect {
            x: crtc.x,
            y: crtc.y,
            width: crtc.width,
            height: crtc.height,
        });
    }
    Err(format!(
        "XRandR connected output index {output_index} is unavailable"
    ))
}

fn validate_software_geometry(rect: CaptureRect) -> Result<(), String> {
    let width = u32::from(rect.width);
    let height = u32::from(rect.height);
    if rect.x < 0
        || rect.y < 0
        || width == 0
        || height == 0
        || (width | height) & 1 != 0
        || width > MAX_WIDTH
        || height > MAX_HEIGHT
    {
        return Err(format!(
            "unsupported software capture rectangle {}x{}+{}+{}",
            rect.width, rect.height, rect.x, rect.y
        ));
    }
    Ok(())
}

fn validate_root_bounds(
    root_width: u16,
    root_height: u16,
    rect: CaptureRect,
) -> Result<(), String> {
    let right = i32::from(rect.x)
        .checked_add(i32::from(rect.width))
        .ok_or_else(|| "XRandR horizontal geometry overflow".to_string())?;
    let bottom = i32::from(rect.y)
        .checked_add(i32::from(rect.height))
        .ok_or_else(|| "XRandR vertical geometry overflow".to_string())?;
    if right > i32::from(root_width) || bottom > i32::from(root_height) {
        return Err("XRandR selected rectangle exceeds the root geometry".to_string());
    }
    Ok(())
}

fn validate_recreated_rect(expected: CaptureRect, actual: CaptureRect) -> Result<(), String> {
    if actual != expected {
        return Err(format!(
            "unsupported X11 mode after modeset: expected {}x{}+{}+{}, got {}x{}+{}+{}",
            expected.width,
            expected.height,
            expected.x,
            expected.y,
            actual.width,
            actual.height,
            actual.x,
            actual.y
        ));
    }
    Ok(())
}

fn validate_image_reply(
    depth: u8,
    visual: Visualid,
    bytes: usize,
    layout: PixelLayout,
) -> Result<(), String> {
    if depth != layout.depth || visual != layout.visual || bytes != layout.byte_len {
        return Err(format!(
            "X11 image layout changed: depth={depth} visual={visual:#x} bytes={bytes}, \
             expected depth={} visual={:#x} bytes={}",
            layout.depth, layout.visual, layout.byte_len
        ));
    }
    Ok(())
}

fn install_damage(
    connection: &RustConnection,
    root: Window,
) -> Result<Option<damage::Damage>, String> {
    let version = match connection.damage_query_version(1, 1) {
        Ok(cookie) => match cookie.reply() {
            Ok(version) => version,
            Err(error) => {
                warn_once(&DAMAGE_DEGRADED_WARNED, || {
                    format!(
                        "WARNING: XDamage unavailable ({error}); using bounded full-frame polling"
                    )
                });
                return Ok(None);
            }
        },
        Err(error) => {
            warn_once(&DAMAGE_DEGRADED_WARNED, || {
                format!("WARNING: XDamage unavailable ({error}); using bounded full-frame polling")
            });
            return Ok(None);
        }
    };
    if version.major_version < 1 {
        warn_once(&DAMAGE_DEGRADED_WARNED, || {
            "WARNING: XDamage version is unsupported; using bounded full-frame polling".to_string()
        });
        return Ok(None);
    }
    let damage = connection
        .generate_id()
        .map_err(|error| format!("allocate XDamage id: {error}"))?;
    connection
        .damage_create(damage, root, damage::ReportLevel::BOUNDING_BOX)
        .map_err(|error| format!("create XDamage: {error}"))?
        .check()
        .map_err(|error| format!("create XDamage: {error}"))?;
    Ok(Some(damage))
}

fn install_shm(connection: &RustConnection, bytes: usize) -> Result<Transfer, String> {
    let version = connection
        .shm_query_version()
        .map_err(|error| format!("query MIT-SHM: {error}"))?
        .reply()
        .map_err(|error| format!("query MIT-SHM: {error}"))?;
    if (version.major_version, version.minor_version) < (1, 2) {
        return Err(format!(
            "server MIT-SHM version {}.{} is older than 1.2",
            version.major_version, version.minor_version
        ));
    }
    let segment = connection
        .generate_id()
        .map_err(|error| format!("allocate MIT-SHM id: {error}"))?;
    let size = u32::try_from(bytes).map_err(|_| "MIT-SHM segment size overflow".to_string())?;
    let reply = connection
        .shm_create_segment(segment, size, false)
        .map_err(|error| format!("create MIT-SHM 1.2 segment: {error}"))?
        .reply()
        .map_err(|error| format!("create MIT-SHM 1.2 segment: {error}"))?;
    let mapping = match MmapRegion::map(&reply.shm_fd, bytes) {
        Ok(mapping) => mapping,
        Err(error) => {
            let _ = connection.shm_detach(segment);
            return Err(error);
        }
    };
    Ok(Transfer::Shm { segment, mapping })
}

fn allocate_zeroed(len: usize) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|_| format!("reserve bounded {len}-byte frame buffer"))?;
    bytes.resize(len, 0);
    Ok(bytes)
}

struct I420Storage {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

fn allocate_i420(width: usize, height: usize) -> Result<I420Storage, String> {
    let y_len = width
        .checked_mul(height)
        .ok_or_else(|| "I420 luma geometry overflow".to_string())?;
    let chroma_len = y_len
        .checked_div(4)
        .ok_or_else(|| "I420 chroma geometry overflow".to_string())?;
    Ok(I420Storage {
        y: allocate_zeroed(y_len)?,
        u: allocate_zeroed(chroma_len)?,
        v: allocate_zeroed(chroma_len)?,
    })
}

fn failure_code(message: &str, code: i32) -> i32 {
    crate::log(&format!("ERROR: {message}"));
    code
}

fn software_plan(
    width: u32,
    height: u32,
    fps: u32,
    cursor: crate::CursorCaptureMode,
    color: crate::ColorSpec,
) -> Result<ResolvedMediaPlan, String> {
    crate::resolved_media_plan(
        EncoderBackend::OpenH264,
        "h264",
        color,
        width,
        height,
        fps,
        cursor,
    )
}

pub(crate) fn run_with_args(args: Vec<String>) -> ! {
    std::process::exit(run_inner(args))
}

fn run_admission_probe(
    capture: X11Capture,
    fps: u32,
    color: crate::ColorSpec,
    options: &crate::admission_probe::AdmissionProbeOptions,
) -> i32 {
    let width = u32::from(capture.rect.width);
    let height = u32::from(capture.rect.height);
    if width != options.width || height != options.height {
        return failure_code(
            &format!(
                "admission probe geometry {}x{} differs from exact X11 output {width}x{height}",
                options.width, options.height
            ),
            2,
        );
    }
    let width_usize = usize::from(capture.rect.width);
    let height_usize = usize::from(capture.rect.height);
    let mut storage = match allocate_i420(width_usize, height_usize) {
        Ok(storage) => storage,
        Err(error) => return failure_code(&error, 3),
    };
    storage.y.fill(16);
    storage.u.fill(128);
    storage.v.fill(128);
    let threads = std::thread::available_parallelism()
        .map(|count| count.get().min(64))
        .unwrap_or(1);
    let threads = u16::try_from(threads).unwrap_or(1);
    let mut encoder = match SoftwareH264Encoder::new(SoftwareH264Config {
        width,
        height,
        fps,
        bitrate_bps: 8_000_000,
        num_threads: threads,
        range: color.range,
        matrix: color.matrix,
        primaries: color.primaries,
        transfer: color.transfer,
    }) {
        Ok(encoder) => encoder,
        Err(error) => {
            return failure_code(&format!("OpenH264 admission init failed: {error}"), 4);
        }
    };
    let mut frame = 0u8;
    let result =
        crate::admission_probe::run_probe_loop(options, std::io::stdout().lock(), |input| {
            frame = frame.wrapping_add(1);
            let changed_luma = if input.kind == arcen_media::RepresentativeFrameKind::FullMotion {
                storage.y.len()
            } else {
                storage
                    .y
                    .len()
                    .saturating_mul(usize::from(input.dirty_ratio.basis_points()))
                    .div_ceil(10_000)
                    .max(1)
            };
            storage.y[..changed_luma].fill(frame);
            if input.kind == arcen_media::RepresentativeFrameKind::FullMotion {
                storage.u.fill(frame.wrapping_add(64));
                storage.v.fill(frame.wrapping_add(128));
            }
            if input.force_idr {
                encoder.force_idr();
            }
            let frame = I420Frame::new(
                width,
                height,
                &storage.y,
                width_usize,
                &storage.u,
                width_usize / 2,
                &storage.v,
                width_usize / 2,
            )
            .map_err(|error| format!("OpenH264 admission frame: {error}"))?;
            let started = Instant::now();
            let output = encoder
                .encode(frame)
                .map_err(|error| format!("OpenH264 admission encode: {error}"))?;
            Ok(crate::admission_probe::ProbeEncodeResult {
                encode_latency: started.elapsed(),
                delivered: output.is_some(),
            })
        });
    drop(encoder);
    drop(capture);
    match result {
        Ok(()) => 0,
        Err(error) => failure_code(&error, 5),
    }
}

fn run_inner(args: Vec<String>) -> i32 {
    let output_index = args
        .get(1)
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let codec = args.get(2).map_or("h264", String::as_str);
    let fps = args
        .get(3)
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(MAX_FPS);
    let cursor = match crate::cursor_mode_from_args(&args) {
        Ok(cursor) => cursor,
        Err(error) => return failure_code(error, 2),
    };
    // The single resolved colour contract for this run (see
    // `crate::requested_color`): an explicit `variant=<id>` wins over the
    // legacy default, and it is this one value — never a separately
    // re-derived `ColorSpec::legacy(...)` — that must reach both
    // `software_plan`'s READY line and the `ColorTransform` the BGRA -> I420
    // conversion below actually applies, so the two cannot disagree.
    let color = match crate::requested_color(&args, false) {
        Ok(color) => color,
        Err(error) => return failure_code(&error, 2),
    };
    let intent = match crate::requested_intent(&args) {
        Ok(intent) => intent,
        Err(error) => return failure_code(&format!("invalid intent: {error}"), 2),
    };
    let qp_map_policy = match crate::requested_qp_map(&args) {
        Ok(policy) => policy,
        Err(error) => return failure_code(&format!("invalid qp-map: {error}"), 2),
    };
    if !crate::linux_software_policy_supported(intent, qp_map_policy) {
        return failure_code(
            "software-h264 supports only intent=interactive and qp-map=off; \
             quality and QP delta maps require native NVENC",
            2,
        );
    }
    if codec != "h264"
        || color.chroma != arcen_media::ChromaSubsampling::Yuv420
        || color.bit_depth != arcen_media::BitDepth::Eight
        || fps == 0
        || fps > MAX_FPS
        || cursor.include_cursor()
    {
        return failure_code(
            "software-h264 requires h264/yuv420 8-bit, local cursor, and 1..=30 fps",
            2,
        );
    }
    let admission_probe = match crate::admission_probe::options_from_args(&args) {
        Ok(options) => options,
        Err(error) => return failure_code(&error, 2),
    };
    let framed = crate::framed_output_from_args(&args);
    let capture = match X11Capture::connect(output_index) {
        Ok(capture) => capture,
        Err(error) => {
            return failure_code(&format!("X11 capture initialization failed: {error}"), 3);
        }
    };
    if let Some(options) = admission_probe.as_ref() {
        return run_admission_probe(capture, fps, color, options);
    }
    let mut capture = capture;
    let width = u32::from(capture.rect.width);
    let height = u32::from(capture.rect.height);
    let width_usize = usize::from(capture.rect.width);
    let height_usize = usize::from(capture.rect.height);
    // The BGRA -> I420 conversion below must apply the same colour contract
    // `software_plan` announces in the READY line, not an implicit default.
    let transform = color.transform();
    let mut i420 = match allocate_i420(width_usize, height_usize) {
        Ok(storage) => storage,
        Err(error) => return failure_code(&error, 3),
    };
    let threads = std::thread::available_parallelism()
        .map(|count| count.get().min(64))
        .unwrap_or(1);
    let threads = u16::try_from(threads).unwrap_or(1);
    let mut encoder = match SoftwareH264Encoder::new(SoftwareH264Config {
        width,
        height,
        fps,
        bitrate_bps: 8_000_000,
        num_threads: threads,
        range: color.range,
        matrix: color.matrix,
        primaries: color.primaries,
        transfer: color.transfer,
    }) {
        Ok(encoder) => encoder,
        Err(error) => {
            return failure_code(&format!("OpenH264 initialization failed: {error}"), 4);
        }
    };
    let plan = match software_plan(width, height, fps, cursor, color) {
        Ok(plan) => plan,
        Err(error) => return failure_code(&error, 2),
    };
    let control = crate::spawn_control_thread("OpenH264");
    let mut cadence = IdleCadence::new(KEEPALIVE);
    cadence.note_frame();
    let interval = crate::frame_interval_from_fps(fps);
    let mut next_tick = Instant::now();
    let mut last_emit = Instant::now();
    let mut ready = false;
    let mut stdout = std::io::stdout();
    let mut last_stats = Instant::now();
    let mut emitted = 0u64;
    let mut bytes = 0u64;
    let mut modeset_idr = false;
    // Region-owned activity scheduler (`arcen_media::RegionActivityScheduler`).
    // It contains the Keel damage tracker that skips unchanged rows during
    // BGRA→I420 conversion — matching the Windows MF path — and additionally
    // turns that same single hash pass into a serve/skip decision so a static
    // region stops paying for conversion, encode, and emission between its
    // bounded refresh and keyframe deadlines.
    let mut schedule = match crate::region_schedule::CaptureRegionScheduler::try_new(
        output_index,
        width_usize,
        height_usize,
        fps,
        KEEPALIVE,
        REGION_KEYFRAME_INTERVAL,
        REGION_INPUT_WAKE_GRACE,
    ) {
        Ok(schedule) => schedule,
        Err(error) => {
            crate::log(&format!(
                "OpenH264: region activity scheduler init failed ({error}); falling back to full-frame conversion"
            ));
            // Degraded mode reports full damage and serves every frame, which
            // is exactly the pre-activity full-conversion behaviour.
            crate::region_schedule::CaptureRegionScheduler::degraded(interval)
        }
    };

    while !control.stop_requested() {
        match capture.poll_activity() {
            Ok(Activity::Damage) => cadence.note_frame(),
            Ok(Activity::Modeset) => {
                capture = match capture.recreate() {
                    Ok(capture) => capture,
                    Err(error) => {
                        return failure_code(&format!("X11 modeset recreation failed: {error}"), 3);
                    }
                };
                cadence.reset();
                cadence.note_frame();
                encoder.force_idr();
                modeset_idr = true;
                // Modeset invalidates the previous frame buffer and every
                // activity/deadline observation derived from it.
                schedule.reset();
            }
            Ok(Activity::None) => {
                if capture.damage.is_none() {
                    cadence.note_frame();
                }
            }
            Err(error) => return failure_code(&error, 3),
        }

        let now = Instant::now();
        if now >= next_tick {
            let idr_requested = control.take_idr();
            let decision = cadence.decision(idr_requested || modeset_idr, last_emit.elapsed());
            if let Some(mode) = decision {
                if matches!(mode, EmitMode::Idr) || idr_requested || modeset_idr {
                    encoder.force_idr();
                }
                let mut service_attempted = false;
                let encoded = {
                    let source = match capture.capture() {
                        Ok(source) => source,
                        Err(error) => return failure_code(&error, 3),
                    };
                    // One hash pass over this frame produces both the 16×16
                    // damage map that drives selective BGRA→I420 conversion
                    // (matching the Windows MF path) and this region's
                    // serve/skip decision. Host-forced keyframes always win
                    // over measured activity.
                    let forced_keyframe = if modeset_idr {
                        Some(ForcedKeyframe::Recovery)
                    } else if idr_requested || matches!(mode, EmitMode::Idr) {
                        Some(ForcedKeyframe::ClientRequest)
                    } else if matches!(mode, EmitMode::FirstFrame) {
                        Some(ForcedKeyframe::Startup)
                    } else {
                        None
                    };
                    let service = schedule.observe(
                        source,
                        now,
                        forced_keyframe,
                        control.take_input_activity(),
                        ActivityHint::None,
                    );
                    if service.serve {
                        service_attempted = true;
                        let mut destination = match I420FrameMut::new(
                            width,
                            height,
                            &mut i420.y,
                            width_usize,
                            &mut i420.u,
                            width_usize / 2,
                            &mut i420.v,
                            width_usize / 2,
                        ) {
                            Ok(destination) => destination,
                            Err(error) => {
                                return failure_code(&format!("construct I420 frame: {error}"), 3);
                            }
                        };
                        if service.keyframe {
                            encoder.force_idr();
                        }
                        let summary = service.summary;
                        if service.keyframe || summary.is_full_damage() {
                            // Baseline, recovery, and full-damage frames always
                            // rebuild every plane.
                            if let Err(error) =
                                convert_bgra_to_i420(source, &mut destination, transform)
                            {
                                return failure_code(
                                    &format!("convert BGRA to I420 (full): {error}"),
                                    3,
                                );
                            }
                        } else if !summary.is_clean() {
                            match schedule.damage_map() {
                                // Selective: convert only dirty block-row bands.
                                Some(map) => {
                                    for rows in map.dirty_block_rows() {
                                        if let Err(error) = convert_bgra_to_i420_rows(
                                            source,
                                            &mut destination,
                                            rows,
                                            transform,
                                        ) {
                                            return failure_code(
                                                &format!("convert BGRA to I420 (rows): {error}"),
                                                3,
                                            );
                                        }
                                    }
                                }
                                // Degraded scheduler: no map, convert fully.
                                None => {
                                    if let Err(error) =
                                        convert_bgra_to_i420(source, &mut destination, transform)
                                    {
                                        return failure_code(
                                            &format!("convert BGRA to I420 (degraded): {error}"),
                                            3,
                                        );
                                    }
                                }
                            }
                        }
                        // A clean mandatory refresh re-encodes the retained
                        // planes; nothing changed to convert.
                        match encoder.encode(destination.as_frame()) {
                            Ok(encoded) => encoded,
                            Err(error) => {
                                return failure_code(
                                    &format!("OpenH264 encode failed: {error}"),
                                    5,
                                );
                            }
                        }
                    } else {
                        // Static region: no conversion, no encode, no wire
                        // bytes. The bounded max-idle refresh and keyframe
                        // deadlines still force a service on their own
                        // schedule, so suppression can never starve it.
                        None
                    }
                };
                if let Some(access_unit) = encoded {
                    if !ready {
                        if let Err(error) = crate::announce_ready(plan) {
                            return failure_code(&format!("emit READY: {error}"), 5);
                        }
                        ready = true;
                    }
                    if crate::write_access_unit(&mut stdout, access_unit.bytes, framed).is_err() {
                        return 0;
                    }
                    emitted = emitted.saturating_add(1);
                    bytes = bytes.saturating_add(access_unit.bytes.len() as u64);
                    cadence.on_submitted();
                    last_emit = now;
                    modeset_idr = false;
                } else if service_attempted {
                    // The encoder buffered this frame instead of returning an
                    // access unit. Re-arm the region so its deadlines are not
                    // consumed and a pending keyframe is not downgraded.
                    schedule.note_service_failed();
                }
            }
            next_tick += interval;
            if next_tick < now {
                next_tick = now + interval;
            }
        }
        if last_stats.elapsed() >= Duration::from_secs(1) {
            crate::log(&format!(
                "enc_fps={emitted} kbps={} backend=openh264 damage_source={} keel_enabled={} modeset_idr={modeset_idr} {}",
                bytes.saturating_mul(8) / 1000,
                if capture.damage.is_some() {
                    "xdamage"
                } else {
                    "full_poll"
                },
                schedule.is_active(),
                schedule.telemetry_fragment(),
            ));
            emitted = 0;
            bytes = 0;
            last_stats = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    crate::log("OpenH264 control closed; dropping capture and encoder before exit");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_bounded_and_requires_bgra_compatible_depth() {
        let layout = checked_layout(1920, 1080, 24, 32, 32).expect("layout");
        assert_eq!(layout.stride, 1920 * 4);
        assert_eq!(layout.byte_len, 1920 * 1080 * 4);
        let secondary = checked_layout(1800, 1130, 24, 32, 32).expect("Philips layout");
        assert_eq!(secondary.byte_len, 1800 * 1130 * 4);
        assert!(checked_layout(1920, 1080, 30, 32, 32).is_err());
        assert!(checked_layout(1920, 1080, 24, 24, 32).is_err());
    }

    #[test]
    fn geometry_rejects_negative_odd_and_oversize_rectangles() {
        assert!(validate_software_geometry(CaptureRect {
            x: -1,
            y: 0,
            width: 1920,
            height: 1080,
        })
        .is_err());
        assert!(validate_software_geometry(CaptureRect {
            x: 0,
            y: 0,
            width: 1919,
            height: 1080,
        })
        .is_err());
        assert!(validate_software_geometry(CaptureRect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        })
        .is_ok());
        assert!(validate_software_geometry(CaptureRect {
            x: 0,
            y: 832,
            width: 1800,
            height: 1130,
        })
        .is_ok());
    }

    #[test]
    fn image_reply_validation_detects_layout_drift() {
        let mut layout = checked_layout(1280, 720, 24, 32, 32).expect("layout");
        layout.visual = 42;
        assert!(validate_image_reply(24, 42, layout.byte_len, layout).is_ok());
        assert!(validate_image_reply(24, 43, layout.byte_len, layout).is_err());
        assert!(validate_image_reply(24, 42, layout.byte_len - 1, layout).is_err());
    }

    #[test]
    fn modeset_policy_rejects_a_hidden_resize() {
        let expected = CaptureRect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        assert!(validate_recreated_rect(expected, expected).is_ok());
        assert!(validate_recreated_rect(
            expected,
            CaptureRect {
                width: 1280,
                height: 720,
                ..expected
            }
        )
        .is_err());
    }

    #[test]
    fn selected_rectangle_must_stay_inside_the_root() {
        assert!(validate_root_bounds(
            3840,
            1080,
            CaptureRect {
                x: 1920,
                y: 0,
                width: 1920,
                height: 1080,
            }
        )
        .is_ok());
        assert!(validate_root_bounds(
            1920,
            1080,
            CaptureRect {
                x: 1,
                y: 0,
                width: 1920,
                height: 1080,
            }
        )
        .is_err());
    }
}
