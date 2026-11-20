// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Dedicated-Xorg capture and portable OpenH264 encoding.

use std::os::fd::AsRawFd;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use arcen_keel::{BgraFrame, ExternalDamage};
use arcen_media::canvas::serve::CanvasCapencArgs;
use arcen_media::capenc_control::CapencControlCommand;
use arcen_media::video::{
    changed_bounds, convert_packed_rgb10_to_bgra8, open_software_loop, union_regions,
    CaptureDamage, CaptureRegion, EncoderBackend, FrameRegion, PackedRgb10Layout, RegionPolicy,
    ResolvedMediaPlan, SoftwareLoop, SoftwareLoopConfig,
};
use x11rb::connection::Connection as _;
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    ConnectionExt as _, CreateGCAux, Drawable, Gcontext, ImageFormat, ImageOrder, Pixmap,
    SubwindowMode, VisualClass, Visualid, Window,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

/// The software path's only size and rate bounds: OpenH264's own
/// (`EncoderBackend::OpenH264`). The Pier's frame-rate governor holds a
/// session at what the CPU encodes.
const SOFTWARE_LIMITS: arcen_media::video::BackendLimits = EncoderBackend::OpenH264.contract();
const SOFTWARE_MAX_WIDTH: u32 = SOFTWARE_LIMITS.max_width;
const SOFTWARE_MAX_HEIGHT: u32 = SOFTWARE_LIMITS.max_height;
const SOFTWARE_MAX_CAPTURE_BYTES: usize =
    SOFTWARE_MAX_WIDTH as usize * SOFTWARE_MAX_HEIGHT as usize * 4;
/// The wide path is budgeted in pixels, not as a 3840 by 2160 box: see
/// `arcen_media::video::host_converted_capture_fits`.
const WIDE_MAX_CAPTURE_BYTES: usize =
    arcen_media::video::HOST_CONVERTED_CAPTURE_MAX_PIXELS as usize * 4;
const MAX_FPS: u32 = SOFTWARE_LIMITS.max_fps;
const KEEPALIVE: Duration = Duration::from_secs(1);
/// XDamage rectangles kept between two pictures; more mark the whole
/// picture.
const MAX_DAMAGE_RECTS: usize = 256;
const RED_MASK: u32 = 0x00ff_0000;
const GREEN_MASK: u32 = 0x0000_ff00;
const BLUE_MASK: u32 = 0x0000_00ff;
static DAMAGE_DEGRADED_WARNED: AtomicBool = AtomicBool::new(false);
static SHM_DEGRADED_WARNED: AtomicBool = AtomicBool::new(false);
static SNAPSHOT_DEGRADED_WARNED: AtomicBool = AtomicBool::new(false);

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CapturePurpose {
    Software,
    WideNvenc,
    /// The no-frame native probe of the wide path. It runs before the Pier
    /// arms the desktop at its served geometry, so a desktop still larger
    /// than the pixel budget is probed on a budget-sized part of itself: the
    /// probe proves depth, visual, conversion and the 10-bit encoder, while
    /// the Pier fits the geometry it then arms
    /// (`arcen_media::video::fit_host_converted_capture`).
    WideProbe,
}

impl CapturePurpose {
    const fn max_capture_bytes(self) -> usize {
        match self {
            Self::Software => SOFTWARE_MAX_CAPTURE_BYTES,
            Self::WideNvenc | Self::WideProbe => WIDE_MAX_CAPTURE_BYTES,
        }
    }

    const fn is_wide(self) -> bool {
        matches!(self, Self::WideNvenc | Self::WideProbe)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum X11PixelFormat {
    Bgrx8,
    Rgb10(PackedRgb10Layout),
}

impl X11PixelFormat {
    fn token(self) -> &'static str {
        match self {
            Self::Bgrx8 => "bgrx8",
            Self::Rgb10(layout) if layout == PackedRgb10Layout::XRGB2101010 => "xrgb2101010",
            Self::Rgb10(layout) if layout == PackedRgb10Layout::XBGR2101010 => "xbgr2101010",
            Self::Rgb10(_) => "rgb10-visual-masks",
        }
    }
}

fn checked_layout(
    width: u16,
    height: u16,
    depth: u8,
    bits_per_pixel: u8,
    scanline_pad: u8,
    max_bytes: usize,
) -> Result<PixelLayout, String> {
    // Depth 24 and depth 30 are both stored as 32 bits per pixel, so the
    // stride arithmetic is identical and only the interpretation of each word
    // differs. Depth-30 channel shifts come from the root visual masks.
    //
    // Depth 30 is the whole point of the ten-bit Linux path: X11 has no
    // per-application colour depth, so `XShmGetImage` returns the *screen's*
    // framebuffer format. A depth-24 server can only ever yield eight bits
    // per channel, whatever the encoder is asked for. Rejecting depth 30
    // here -- which this function did until ten-bit capture existed -- meant
    // that configuring the X server for ten bits made capture fail outright
    // rather than deliver the extra bits.
    if width == 0
        || height == 0
        || !matches!(depth, 24 | 30)
        || bits_per_pixel != 32
        || !matches!(scanline_pad, 8 | 16 | 32)
    {
        return Err(format!(
            "unsupported X11 geometry: {width}x{height} depth={depth} bpp={bits_per_pixel} pad={scanline_pad}"
        ));
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
    if byte_len == 0 || byte_len > max_bytes {
        return Err(format!(
            "X11 image needs {byte_len} bytes, above the {max_bytes}-byte capture bound"
        ));
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
pub(crate) enum Activity {
    None,
    Damage,
    Modeset,
}

pub(crate) struct WideX11Frame<'a> {
    pub(crate) bytes: &'a [u8],
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) stride: usize,
    pub(crate) layout: PackedRgb10Layout,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Rgb10PrecisionStats {
    pub(crate) sampled_components: u64,
    pub(crate) off_eight_bit_grid: u64,
    pub(crate) minimum: u16,
    pub(crate) maximum: u16,
}

impl WideX11Frame<'_> {
    pub(crate) fn precision_stats(&self, pixel_step: usize) -> Rgb10PrecisionStats {
        let pixel_step = pixel_step.max(1);
        let mut stats = Rgb10PrecisionStats {
            sampled_components: 0,
            off_eight_bit_grid: 0,
            minimum: u16::MAX,
            maximum: 0,
        };
        for row in (0..self.height).step_by(pixel_step) {
            let source = &self.bytes[row * self.stride..row * self.stride + self.width * 4];
            for column in (0..self.width).step_by(pixel_step) {
                let offset = column * 4;
                let components = self.layout.components(u32::from_le_bytes([
                    source[offset],
                    source[offset + 1],
                    source[offset + 2],
                    source[offset + 3],
                ]));
                for component in components {
                    let reduced =
                        u16::try_from((u32::from(component) * u32::from(u8::MAX) + 511) / 1023)
                            .expect("a ten-bit component reduces to at most 255");
                    let expanded = (reduced << 2) | (reduced >> 6);
                    stats.sampled_components += 1;
                    stats.off_eight_bit_grid += u64::from(component != expanded);
                    stats.minimum = stats.minimum.min(component);
                    stats.maximum = stats.maximum.max(component);
                }
            }
        }
        if stats.sampled_components == 0 {
            stats.minimum = 0;
        }
        stats
    }
}

pub(crate) struct X11Capture {
    connection: RustConnection,
    root: Window,
    output_index: u32,
    rect: CaptureRect,
    layout: PixelLayout,
    pixel_format: X11PixelFormat,
    purpose: CapturePurpose,
    damage: Option<damage::Damage>,
    /// What changed since the last picture, in output pixels. Cell because
    /// event polling reads through `&self`.
    pending: std::cell::Cell<CaptureDamage>,
    /// The XDamage rectangles since the last [`Self::take_damage_into`],
    /// for Keel's narrowed hashing (the software loop); `None` once more
    /// arrived than are worth keeping.
    rects: std::cell::RefCell<Option<Vec<(i64, i64, u64, u64)>>>,
    transfer: Transfer,
    /// A second MIT-SHM segment for [`Self::capture_wide_diffed`], so the
    /// last whole frame stays readable while the next one is read.
    spare: Option<(shm::Seg, MmapRegion)>,
    /// Which segment (0 the main one, 1 the spare) holds the last whole
    /// frame read by [`Self::capture_wide_diffed`]; `None` once a region read
    /// or anything else overwrote it.
    latest_whole: Option<usize>,
    /// Server-side copy every read goes through; `None` reads the root.
    snapshot: Option<Snapshot>,
}

/// A pixmap the size of the output that each read first copies the screen
/// into (`CopyArea`, `IncludeInferiors`) and then reads.
///
/// Reading the root window directly takes milliseconds while a fullscreen,
/// unredirected OpenGL client may swap at any moment, so the picture is torn
/// between two of its frames (measured: up to 98 % of reads of a fullscreen
/// 60 fps producer). The copy is one accelerated blit in the server's request
/// order and the read then sees one frame; NvFBC is atomic on its own.
struct Snapshot {
    pixmap: Pixmap,
    gc: Gcontext,
}

impl Snapshot {
    fn create(
        connection: &RustConnection,
        root: Window,
        depth: u8,
        rect: CaptureRect,
    ) -> Result<Self, String> {
        let pixmap = connection
            .generate_id()
            .map_err(|error| format!("allocate snapshot pixmap id: {error}"))?;
        connection
            .create_pixmap(depth, pixmap, root, rect.width, rect.height)
            .map_err(|error| format!("create snapshot pixmap: {error}"))?
            .check()
            .map_err(|error| format!("create snapshot pixmap: {error}"))?;
        let gc = match connection.generate_id() {
            Ok(gc) => gc,
            Err(error) => {
                let _ = connection.free_pixmap(pixmap);
                return Err(format!("allocate snapshot GC id: {error}"));
            }
        };
        let created = connection
            .create_gc(
                gc,
                pixmap,
                &CreateGCAux::new()
                    .subwindow_mode(SubwindowMode::INCLUDE_INFERIORS)
                    .graphics_exposures(0),
            )
            .map_err(|error| format!("create snapshot GC: {error}"))
            .and_then(|cookie| {
                cookie
                    .check()
                    .map_err(|error| format!("create snapshot GC: {error}"))
            });
        if let Err(error) = created {
            let _ = connection.free_pixmap(pixmap);
            return Err(error);
        }
        Ok(Self { pixmap, gc })
    }
}

/// Where a read of the root rectangle at `x`,`y` finds its pixels: the
/// snapshot pixmap (output-relative, after the copy) or the root itself.
fn read_origin(output: CaptureRect, snapshot: bool, x: i16, y: i16) -> (i16, i16) {
    if snapshot {
        (x - output.x, y - output.y)
    } else {
        (x, y)
    }
}

/// The reply layout a read must match: `GetImage` of a pixmap reports no
/// visual.
fn read_layout(layout: PixelLayout, snapshot: bool) -> PixelLayout {
    if snapshot {
        PixelLayout {
            visual: x11rb::NONE,
            ..layout
        }
    } else {
        layout
    }
}

impl X11Capture {
    fn connect_software(output_index: u32) -> Result<Self, String> {
        Self::connect(output_index, CapturePurpose::Software)
    }

    pub(crate) fn connect_wide(output_index: u32) -> Result<Self, String> {
        Self::connect(output_index, CapturePurpose::WideNvenc)
    }

    /// [`Self::connect_wide`] for the native probe: see
    /// [`CapturePurpose::WideProbe`].
    pub(crate) fn connect_wide_probe(output_index: u32) -> Result<Self, String> {
        Self::connect(output_index, CapturePurpose::WideProbe)
    }

    fn connect(output_index: u32, purpose: CapturePurpose) -> Result<Self, String> {
        let (connection, screen_index) =
            x11rb::connect(None).map_err(|error| format!("connect authenticated X11: {error}"))?;
        let screen = connection
            .setup()
            .roots
            .get(screen_index)
            .ok_or_else(|| "X11 selected screen is absent".to_string())?;
        let root = screen.root;
        let mut rect = if output_index == arcen_media::capenc_stream::WHOLE_SCREEN_OUTPUT_INDEX {
            CaptureRect {
                x: 0,
                y: 0,
                width: screen.width_in_pixels,
                height: screen.height_in_pixels,
            }
        } else {
            selected_output_rect(&connection, root, output_index)?
        };
        if purpose == CapturePurpose::WideProbe {
            rect = probe_rect(rect);
        }
        validate_capture_geometry(rect, purpose)?;

        let setup = connection.setup();
        if setup.image_byte_order != ImageOrder::LSB_FIRST {
            return Err("X11 image byte order is not little-endian".to_string());
        }
        let format = setup
            .pixmap_formats
            .iter()
            .find(|format| format.depth == screen.root_depth)
            .ok_or_else(|| "X11 root depth has no pixmap format".to_string())?;
        let visual = screen
            .allowed_depths
            .iter()
            .flat_map(|depth| depth.visuals.iter())
            .find(|visual| visual.visual_id == screen.root_visual)
            .ok_or_else(|| "X11 root visual is absent from allowed depths".to_string())?;
        let pixel_format = classify_pixel_format(
            screen.root_depth,
            visual.class,
            visual.red_mask,
            visual.green_mask,
            visual.blue_mask,
        )?;
        if purpose.is_wide() && !matches!(pixel_format, X11PixelFormat::Rgb10(_)) {
            return Err(format!(
                "wide NVENC capture requires an X11 depth-30 RGB10 visual, got depth={} format={}",
                screen.root_depth,
                pixel_format.token()
            ));
        }
        let mut layout = checked_layout(
            rect.width,
            rect.height,
            screen.root_depth,
            format.bits_per_pixel,
            format.scanline_pad,
            purpose.max_capture_bytes(),
        )?;
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
        let snapshot = match Snapshot::create(&connection, root, screen.root_depth, rect) {
            Ok(snapshot) => Some(snapshot),
            Err(error) => {
                warn_once(&SNAPSHOT_DEGRADED_WARNED, || {
                    format!(
                        "WARNING: no server-side snapshot ({error}); reading the root directly, \
                         a fullscreen OpenGL client can be captured torn"
                    )
                });
                None
            }
        };
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
            "X11 capture ready: rect={}x{}+{}+{} stride={} depth={} visual={:#x} \
             masks={:#010x}/{:#010x}/{:#010x} format={} transfer={} read={}",
            rect.width,
            rect.height,
            rect.x,
            rect.y,
            layout.stride,
            layout.depth,
            layout.visual,
            visual.red_mask,
            visual.green_mask,
            visual.blue_mask,
            pixel_format.token(),
            if matches!(transfer, Transfer::Shm { .. }) {
                "mit-shm-1.2"
            } else {
                "get-image"
            },
            if snapshot.is_some() {
                "snapshot"
            } else {
                "root"
            }
        ));
        Ok(Self {
            connection,
            root,
            output_index,
            rect,
            layout,
            pixel_format,
            purpose,
            damage,
            rects: std::cell::RefCell::new(Some(Vec::new())),
            pending: std::cell::Cell::new(CaptureDamage::new(
                usize::from(rect.width),
                usize::from(rect.height),
            )),
            transfer,
            spare: None,
            latest_whole: None,
            snapshot,
        })
    }

    /// Prepares a read of the root rectangle `x`,`y`,`width`,`height`: copies
    /// it into the snapshot when there is one. Returns the drawable, origin
    /// and reply layout the read uses.
    fn read_source(
        &self,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        layout: PixelLayout,
    ) -> Result<(Drawable, i16, i16, PixelLayout), String> {
        let snapshot = self.snapshot.as_ref();
        let (read_x, read_y) = read_origin(self.rect, snapshot.is_some(), x, y);
        let Some(snapshot) = snapshot else {
            return Ok((self.root, read_x, read_y, layout));
        };
        self.connection
            .copy_area(
                self.root,
                snapshot.pixmap,
                snapshot.gc,
                x,
                y,
                read_x,
                read_y,
                width,
                height,
            )
            .map_err(|error| format!("request snapshot CopyArea: {error}"))?;
        Ok((snapshot.pixmap, read_x, read_y, read_layout(layout, true)))
    }

    pub(crate) fn width(&self) -> u32 {
        u32::from(self.rect.width)
    }

    pub(crate) fn height(&self) -> u32 {
        u32::from(self.rect.height)
    }

    pub(crate) fn has_damage(&self) -> bool {
        self.damage.is_some()
    }

    /// The packed RGB10 layout of a wide capture (`XBGR2101010` when the
    /// capture is not RGB10, which the wide loop never runs with).
    pub(crate) fn wide_layout(&self) -> PackedRgb10Layout {
        match self.pixel_format {
            X11PixelFormat::Rgb10(layout) => layout,
            _ => PackedRgb10Layout::XBGR2101010,
        }
    }

    pub(crate) fn pixel_format_token(&self) -> &'static str {
        self.pixel_format.token()
    }

    pub(crate) fn transfer_token(&self) -> &'static str {
        if matches!(&self.transfer, Transfer::Shm { .. }) {
            "mit-shm-1.2"
        } else {
            "get-image"
        }
    }

    pub(crate) fn poll_activity(&self) -> Result<Activity, String> {
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
                    // `area` is the damage's bounding box on the root window.
                    let mut pending = self.pending.get();
                    pending.note_rect(
                        i64::from(event.area.x) - i64::from(self.rect.x),
                        i64::from(event.area.y) - i64::from(self.rect.y),
                        u64::from(event.area.width),
                        u64::from(event.area.height),
                    );
                    self.pending.set(pending);
                    {
                        let mut rects = self.rects.borrow_mut();
                        let keep = rects
                            .as_ref()
                            .is_some_and(|rects| rects.len() < MAX_DAMAGE_RECTS);
                        if keep {
                            if let Some(rects) = rects.as_mut() {
                                rects.push((
                                    i64::from(event.area.x) - i64::from(self.rect.x),
                                    i64::from(event.area.y) - i64::from(self.rect.y),
                                    u64::from(event.area.width),
                                    u64::from(event.area.height),
                                ));
                            }
                        } else {
                            *rects = None;
                        }
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

    fn capture_bytes(&mut self) -> Result<&[u8], String> {
        if self.latest_whole == Some(0) {
            self.latest_whole = None;
        }
        let (drawable, x, y, layout) = self.read_source(
            self.rect.x,
            self.rect.y,
            self.rect.width,
            self.rect.height,
            self.layout,
        )?;
        let bytes = match &mut self.transfer {
            Transfer::Shm { segment, mapping } => {
                let reply = self
                    .connection
                    .shm_get_image(
                        drawable,
                        x,
                        y,
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
                    layout,
                )?;
                mapping.bytes()
            }
            Transfer::GetImage { storage } => {
                let reply = self
                    .connection
                    .get_image(
                        ImageFormat::Z_PIXMAP,
                        drawable,
                        x,
                        y,
                        self.rect.width,
                        self.rect.height,
                        u32::MAX,
                    )
                    .map_err(|error| format!("request XGetImage: {error}"))?
                    .reply()
                    .map_err(|error| format!("XGetImage: {error}"))?;
                validate_image_reply(reply.depth, reply.visual, reply.data.len(), layout)?;
                storage.clear();
                storage
                    .try_reserve_exact(reply.data.len())
                    .map_err(|_| "reserve bounded XGetImage storage".to_string())?;
                storage.extend_from_slice(&reply.data);
                storage.as_slice()
            }
        };
        Ok(bytes)
    }

    /// Moves the XDamage rectangles since the last call into `damage` for
    /// Keel. `false` without XDamage: nothing is vouched for, so Keel
    /// hashes the whole picture.
    pub(crate) fn take_damage_into(&self, damage: &mut ExternalDamage) -> bool {
        if self.damage.is_none() {
            return false;
        }
        let rects = self.rects.replace(Some(Vec::new()));
        damage.reset();
        match rects {
            Some(rects) => {
                for (x, y, width, height) in rects {
                    damage.mark_rect_bounds(
                        x,
                        y,
                        x.saturating_add(i64::try_from(width).unwrap_or(i64::MAX)),
                        y.saturating_add(i64::try_from(height).unwrap_or(i64::MAX)),
                    );
                }
            }
            None => damage.mark_full_frame(),
        }
        true
    }

    /// Whether this capture can read a rectangle on its own: only through
    /// MIT-SHM. The bounded `XGetImage` fallback reads whole frames.
    pub(crate) fn supports_regions(&self) -> bool {
        transfer_supports_regions(&self.transfer)
    }

    /// Whether XDamage reported anything (or a whole frame is owed) since the
    /// last [`Self::take_capture_region`].
    pub(crate) fn has_pending_damage(&self) -> bool {
        self.pending.get().is_pending()
    }

    /// What the next picture has to read. Without XDamage nothing is vouched
    /// for, so every picture reads the whole frame.
    pub(crate) fn take_capture_region(&self, policy: RegionPolicy) -> CaptureRegion {
        let mut pending = self.pending.get();
        if self.damage.is_none() {
            pending.note_full();
        }
        let region = pending.take(policy);
        self.pending.set(pending);
        region
    }

    /// Reads only `region` of the output into the start of the shared
    /// segment, packed (`region.width * 4` bytes a row), as the region
    /// conversion expects it.
    pub(crate) fn capture_wide_region(
        &mut self,
        region: FrameRegion,
    ) -> Result<WideX11Frame<'_>, String> {
        let X11PixelFormat::Rgb10(layout) = self.pixel_format else {
            return Err(format!(
                "wide capture expected RGB10, got {}",
                self.pixel_format.token()
            ));
        };
        let (x, y, width, height) = region_on_root(self.rect, region)?;
        let stride = usize::from(width) * 4;
        let byte_len = stride * usize::from(height);
        if !matches!(self.transfer, Transfer::Shm { .. }) {
            return Err("region capture needs MIT-SHM".to_string());
        }
        let (drawable, x, y, layout_read) = self.read_source(
            x,
            y,
            width,
            height,
            PixelLayout {
                byte_len,
                stride,
                ..self.layout
            },
        )?;
        let Transfer::Shm { segment, mapping } = &mut self.transfer else {
            return Err("region capture needs MIT-SHM".to_string());
        };
        // The staged frame now holds pixels no whole frame read had, so a
        // later difference against that frame could miss a change.
        self.latest_whole = None;
        let reply = self
            .connection
            .shm_get_image(
                drawable,
                x,
                y,
                width,
                height,
                u32::MAX,
                u8::from(ImageFormat::Z_PIXMAP),
                *segment,
                0,
            )
            .map_err(|error| format!("request region XShmGetImage: {error}"))?
            .reply()
            .map_err(|error| format!("region XShmGetImage: {error}"))?;
        validate_image_reply(
            reply.depth,
            reply.visual,
            usize::try_from(reply.size).map_err(|_| "XShm image size overflow".to_string())?,
            layout_read,
        )?;
        Ok(WideX11Frame {
            bytes: &mapping.bytes()[..byte_len],
            width: region.width,
            height: region.height,
            stride,
            layout,
        })
    }

    /// Adds the second MIT-SHM segment [`Self::capture_wide_diffed`] reads
    /// into. Returns whether whole-frame differencing is available.
    pub(crate) fn enable_frame_diff(&mut self) -> bool {
        if self.spare.is_some() {
            return true;
        }
        if !matches!(self.transfer, Transfer::Shm { .. }) {
            return false;
        }
        match install_shm(&self.connection, self.layout.byte_len) {
            Ok(Transfer::Shm { segment, mapping }) => {
                self.spare = Some((segment, mapping));
                true
            }
            Ok(Transfer::GetImage { .. }) => false,
            Err(error) => {
                crate::log(&format!(
                    "WARNING: no second MIT-SHM segment ({error}); whole frames are converted whole"
                ));
                false
            }
        }
    }

    /// Reads the whole output into the segment that does not hold the last
    /// whole frame and, with `compare`, bounds what differs from that frame.
    /// The bound is `None` when there is no earlier whole frame to compare
    /// with or `compare` is off (convert everything), `Some(None)` when nothing changed, and `Some(Some(r))`
    /// when only `r` changed.
    pub(crate) fn capture_wide_diffed(
        &mut self,
        compare: bool,
    ) -> Result<(WideX11Frame<'_>, Option<Option<FrameRegion>>), String> {
        let X11PixelFormat::Rgb10(layout) = self.pixel_format else {
            return Err(format!(
                "wide capture expected RGB10, got {}",
                self.pixel_format.token()
            ));
        };
        if !matches!(self.transfer, Transfer::Shm { .. }) || self.spare.is_none() {
            return Err("whole-frame differencing needs two MIT-SHM segments".to_string());
        }
        let (drawable, read_x, read_y, layout_read) = self.read_source(
            self.rect.x,
            self.rect.y,
            self.rect.width,
            self.rect.height,
            self.layout,
        )?;
        let (Transfer::Shm { segment, mapping }, Some((spare_segment, spare_mapping))) =
            (&mut self.transfer, self.spare.as_mut())
        else {
            return Err("whole-frame differencing needs two MIT-SHM segments".to_string());
        };
        let width = usize::from(self.rect.width);
        let height = usize::from(self.rect.height);
        let stride = self.layout.stride;
        let target = if self.latest_whole == Some(1) { 0 } else { 1 };
        let (read_segment, read_mapping, other_mapping) = if target == 0 {
            (*segment, mapping, spare_mapping)
        } else {
            (*spare_segment, spare_mapping, mapping)
        };
        let reply = self
            .connection
            .shm_get_image(
                drawable,
                read_x,
                read_y,
                self.rect.width,
                self.rect.height,
                u32::MAX,
                u8::from(ImageFormat::Z_PIXMAP),
                read_segment,
                0,
            )
            .map_err(|error| format!("request XShmGetImage: {error}"))?
            .reply()
            .map_err(|error| format!("XShmGetImage: {error}"))?;
        validate_image_reply(
            reply.depth,
            reply.visual,
            usize::try_from(reply.size).map_err(|_| "XShm image size overflow".to_string())?,
            layout_read,
        )?;
        let previous_valid = self.latest_whole == Some(1 - target);
        self.latest_whole = Some(target);
        let current = read_mapping.bytes();
        let bound = if compare && previous_valid {
            Some(changed_bounds_banded(
                other_mapping.bytes(),
                current,
                stride,
                width,
                height,
            )?)
        } else {
            None
        };
        Ok((
            WideX11Frame {
                bytes: current,
                width,
                height,
                stride,
                layout,
            },
            bound,
        ))
    }

    pub(crate) fn capture_wide(&mut self) -> Result<WideX11Frame<'_>, String> {
        let width = usize::from(self.rect.width);
        let height = usize::from(self.rect.height);
        let stride = self.layout.stride;
        let X11PixelFormat::Rgb10(layout) = self.pixel_format else {
            return Err(format!(
                "wide capture expected RGB10, got {}",
                self.pixel_format.token()
            ));
        };
        let bytes = self.capture_bytes()?;
        Ok(WideX11Frame {
            bytes,
            width,
            height,
            stride,
            layout,
        })
    }

    fn capture_bgra<'a>(&'a mut self, scratch: &'a mut Vec<u8>) -> Result<BgraFrame<'a>, String> {
        let width = usize::from(self.rect.width);
        let height = usize::from(self.rect.height);
        let stride = self.layout.stride;
        let pixel_format = self.pixel_format;
        let bytes = self.capture_bytes()?;
        match pixel_format {
            X11PixelFormat::Bgrx8 => BgraFrame::new(bytes, width, height, stride)
                .map_err(|error| format!("validate captured BGRA frame: {error}")),
            X11PixelFormat::Rgb10(layout) => {
                let destination_stride = width
                    .checked_mul(4)
                    .ok_or_else(|| "BGRA conversion stride overflow".to_string())?;
                let destination_len = destination_stride
                    .checked_mul(height)
                    .ok_or_else(|| "BGRA conversion length overflow".to_string())?;
                if scratch.len() != destination_len {
                    scratch.resize(destination_len, 0);
                }
                convert_packed_rgb10_to_bgra8(
                    bytes,
                    stride,
                    scratch,
                    destination_stride,
                    width,
                    height,
                    layout,
                )
                .map_err(|error| format!("convert X11 RGB10 to BGRA8: {error}"))?;
                BgraFrame::new(scratch, width, height, destination_stride)
                    .map_err(|error| format!("validate converted BGRA frame: {error}"))
            }
        }
    }

    pub(crate) fn recreate(self) -> Result<Self, String> {
        let expected = self.rect;
        let output_index = self.output_index;
        let purpose = self.purpose;
        drop(self);
        let replacement = Self::connect(output_index, purpose)?;
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
        if let Some((segment, _)) = &self.spare {
            let _ = self.connection.shm_detach(*segment);
        }
        if let Some(snapshot) = self.snapshot.take() {
            let _ = self.connection.free_gc(snapshot.gc);
            let _ = self.connection.free_pixmap(snapshot.pixmap);
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
    // `output=<name>`: a console shown as it is binds its outputs by name;
    // one that is not lit is refused, never replaced by another.
    let wanted_name = crate::requested_output_name();
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
        if let Some(name) = wanted_name {
            if info.name.as_slice() != name.as_bytes() {
                continue;
            }
        } else if connected_index != output_index {
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
    Err(wanted_name.map_or_else(
        || format!("XRandR connected output index {output_index} is unavailable"),
        |name| format!("XRandR output {name:?} is not lit"),
    ))
}

fn classify_pixel_format(
    depth: u8,
    class: VisualClass,
    red_mask: u32,
    green_mask: u32,
    blue_mask: u32,
) -> Result<X11PixelFormat, String> {
    if class != VisualClass::TRUE_COLOR {
        return Err(format!("X11 root visual class {class:?} is not TrueColor"));
    }
    if depth == 24 && red_mask == RED_MASK && green_mask == GREEN_MASK && blue_mask == BLUE_MASK {
        return Ok(X11PixelFormat::Bgrx8);
    }
    if depth == 30 {
        let layout =
            PackedRgb10Layout::from_masks(red_mask, green_mask, blue_mask).ok_or_else(|| {
                format!(
                    "X11 depth-30 visual has unsupported RGB masks \
                     {red_mask:#010x}/{green_mask:#010x}/{blue_mask:#010x}"
                )
            })?;
        return Ok(X11PixelFormat::Rgb10(layout));
    }
    Err(format!(
        "unsupported X11 root visual depth={depth} masks={red_mask:#010x}/{green_mask:#010x}/{blue_mask:#010x}"
    ))
}

fn validate_capture_geometry(rect: CaptureRect, purpose: CapturePurpose) -> Result<(), String> {
    let width = u32::from(rect.width);
    let height = u32::from(rect.height);
    let within_limits = match purpose {
        CapturePurpose::Software => width <= SOFTWARE_MAX_WIDTH && height <= SOFTWARE_MAX_HEIGHT,
        CapturePurpose::WideNvenc | CapturePurpose::WideProbe => {
            arcen_media::video::host_converted_capture_fits(width, height)
        }
    };
    if rect.x < 0
        || rect.y < 0
        || width == 0
        || height == 0
        || (width | height) & 1 != 0
        || !within_limits
    {
        return Err(format!(
            "unsupported {} capture rectangle {}x{}+{}+{} ({})",
            if purpose.is_wide() {
                "wide NVENC"
            } else {
                "software"
            },
            rect.width,
            rect.height,
            rect.x,
            rect.y,
            if purpose.is_wide() {
                format!(
                    "at most {} pixels with edges up to {}",
                    arcen_media::video::HOST_CONVERTED_CAPTURE_MAX_PIXELS,
                    arcen_media::video::HOST_CONVERTED_CAPTURE_MAX_EDGE
                )
            } else {
                format!("maximum {SOFTWARE_MAX_WIDTH}x{SOFTWARE_MAX_HEIGHT}")
            },
        ));
    }
    Ok(())
}

/// The part of an output the wide native probe captures: the whole output
/// when it is within the pixel budget, otherwise a budget-sized rectangle at
/// its origin.
fn probe_rect(rect: CaptureRect) -> CaptureRect {
    let (width, height) = arcen_media::video::fit_host_converted_capture(
        u32::from(rect.width),
        u32::from(rect.height),
    );
    CaptureRect {
        width: u16::try_from(width).unwrap_or(rect.width),
        height: u16::try_from(height).unwrap_or(rect.height),
        ..rect
    }
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

fn transfer_supports_regions(transfer: &Transfer) -> bool {
    matches!(transfer, Transfer::Shm { .. })
}

/// Most row bands compared at once. Comparing two 1440p frames is about
/// 30 MB of reads; four threads took about 2.5 ms on the Xorg lab host.
const MAX_DIFF_BANDS: usize = 12;

/// [`changed_bounds`] over row bands in parallel, unioned.
fn changed_bounds_banded(
    previous: &[u8],
    current: &[u8],
    stride: usize,
    width: usize,
    height: usize,
) -> Result<Option<FrameRegion>, String> {
    let bands = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(MAX_DIFF_BANDS)
        .min(height.max(1));
    let rows_per_band = height.div_ceil(bands);
    std::thread::scope(|scope| {
        let handles = (0..bands)
            .map(|band| {
                let top = band * rows_per_band;
                let rows = rows_per_band.min(height.saturating_sub(top));
                scope.spawn(move || {
                    if rows == 0 {
                        return Ok(None);
                    }
                    let start = top * stride;
                    changed_bounds(
                        &previous[start..],
                        &current[start..],
                        stride,
                        width,
                        rows,
                        4,
                    )
                    .map(|bound| {
                        bound.map(|region| FrameRegion {
                            y: region.y + top,
                            ..region
                        })
                    })
                    .map_err(|error| format!("compare X11 frames: {error}"))
                })
            })
            .collect::<Vec<_>>();
        handles.into_iter().try_fold(None, |union, handle| {
            let bound = handle
                .join()
                .map_err(|_| "X11 frame comparison panicked".to_string())??;
            Ok(union_regions(union, bound))
        })
    })
}

/// `region` (output pixels) as root-window `GetImage` arguments, refusing
/// anything outside the output.
fn region_on_root(
    output: CaptureRect,
    region: FrameRegion,
) -> Result<(i16, i16, u16, u16), String> {
    let inside = region.width > 0
        && region.height > 0
        && region.x + region.width <= usize::from(output.width)
        && region.y + region.height <= usize::from(output.height);
    if !inside {
        return Err(format!(
            "capture region {}x{}+{}+{} leaves the {}x{} output",
            region.width, region.height, region.x, region.y, output.width, output.height
        ));
    }
    let offset = |origin: i16, delta: usize| {
        i16::try_from(i64::from(origin) + i64::try_from(delta).unwrap_or(i64::MAX))
            .map_err(|_| "capture region origin overflow".to_string())
    };
    Ok((
        offset(output.x, region.x)?,
        offset(output.y, region.y)?,
        u16::try_from(region.width).map_err(|_| "capture region width overflow".to_string())?,
        u16::try_from(region.height).map_err(|_| "capture region height overflow".to_string())?,
    ))
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
    // The geometry is proven; the probe itself needs no capture, which is
    // held until it ends, as before.
    let code = crate::admission_probe::run_software_probe(
        color.video(arcen_media::VideoCodec::H264),
        fps,
        options,
    );
    drop(capture);
    code
}

#[allow(clippy::too_many_lines)]
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
    // legacy default, and it is this one value that reaches both
    // `software_plan`'s READY line and the conversion the full-frame session
    // applies, so the two cannot disagree.
    let color = match crate::requested_color(&args, false) {
        Ok(color) => color,
        Err(error) => return failure_code(&error, 2),
    };
    let intent = match crate::requested_intent(&args) {
        Ok(intent) => intent,
        Err(error) => return failure_code(&format!("invalid intent: {error}"), 2),
    };
    let priority = match crate::requested_motion_priority(&args) {
        Ok(priority) => priority,
        Err(error) => return failure_code(&format!("invalid priority: {error}"), 2),
    };
    let qp_map_policy = match crate::requested_qp_map(&args) {
        Ok(policy) => policy,
        Err(error) => return failure_code(&format!("invalid qp-map: {error}"), 2),
    };
    let canvas = match CanvasCapencArgs::from_args(&args) {
        Ok(canvas) => canvas,
        Err(error) => return failure_code(&error, 2),
    };
    if priority != arcen_media::video::MotionPriority::Detail {
        crate::log("software-h264 ignores priority=motion; motion preference applies to NVENC rate control");
    }
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
            "software-h264 requires h264/yuv420 8-bit, local cursor, and 1..=60 fps",
            2,
        );
    }
    let admission_probe = match crate::admission_probe::options_from_args(&args) {
        Ok(options) => options,
        Err(error) => return failure_code(&error, 2),
    };
    let framed_v1 = crate::framed_output_from_args(&args);
    let framed_v2 = args
        .iter()
        .any(|arg| arg == arcen_media::capenc_stream::FRAMED_OUTPUT_V2);
    let capture = match X11Capture::connect_software(output_index) {
        Ok(capture) => capture,
        Err(error) => {
            return failure_code(&format!("X11 capture initialization failed: {error}"), 3);
        }
    };
    if let Some(options) = admission_probe.as_ref() {
        return run_admission_probe(capture, fps, color, options);
    }
    let mut capture = capture;
    let mut width = u32::from(capture.rect.width);
    let mut height = u32::from(capture.rect.height);
    let video = color.video(arcen_media::VideoCodec::H264);
    let loop_config = |width: u32, height: u32, fps: u32| SoftwareLoopConfig {
        width,
        height,
        fps,
        video,
        canvas,
        start_bitrate_bps: None,
        keepalive: KEEPALIVE,
    };
    let SoftwareLoop {
        mut encode,
        bitrate_bounds: (start_bps, ceiling_bps),
        threading,
    } = match open_software_loop(loop_config(width, height, fps)) {
        Ok(built) => built,
        Err(error) => return failure_code(&error, 4),
    };
    let plan = match software_plan(width, height, fps, cursor, color) {
        Ok(plan) => plan,
        Err(error) => return failure_code(&error, 2),
    };
    let control = crate::spawn_control_thread("OpenH264");
    crate::log(&format!(
        "OpenH264 software bitrate policy: start={start_bps}bps ceiling={ceiling_bps}bps {threading}"
    ));
    match canvas {
        Some(canvas) => crate::log(&format!(
            "software loop: canvas=on monitor={} max_surfaces={} cache_bytes={} cpu_share={}",
            canvas.monitor_id,
            canvas.terms.max_surfaces_per_monitor,
            canvas.terms.cache_bytes,
            canvas.cpu_share.percent()
        )),
        None => crate::log("software loop: canvas=off (full frames)"),
    }
    let mut interval = crate::frame_interval_from_fps(fps);
    let mut next_tick = Instant::now();
    let mut ready = false;
    let mut stdout = std::io::stdout();
    let mut last_stats = Instant::now();
    let mut emitted = 0u64;
    let mut bytes = 0u64;
    let mut bgra_scratch = Vec::new();
    let mut damage_pending = true;
    let mut os_damage = arcen_keel::ExternalDamage::new(width as usize, height as usize).ok();
    // A serial loop: it looks at the screen only between its own ticks, so
    // the stats line carries its pacing for the Pier's frame-rate governor
    // (`arcen_media::video::SerialPacing`).
    let mut pacing = crate::linux_policy::PacingWindow::default();
    let mut busy = Duration::ZERO;
    let mut paced_fps = fps;
    let mut last_error: Option<Instant> = None;

    while !control.stop_requested() {
        match capture.poll_activity() {
            Ok(Activity::Damage) => damage_pending = true,
            Ok(Activity::Modeset) => {
                capture = match capture.recreate() {
                    Ok(capture) => capture,
                    Err(error) => {
                        return failure_code(&format!("X11 modeset recreation failed: {error}"), 3);
                    }
                };
                damage_pending = true;
                let (new_width, new_height) = (
                    u32::from(capture.rect.width),
                    u32::from(capture.rect.height),
                );
                if (new_width, new_height) == (width, height) {
                    encode.request_recovery();
                } else {
                    // A new size is a new loop: a new canvas generation or
                    // a new session at the new size.
                    (width, height) = (new_width, new_height);
                    encode = match open_software_loop(loop_config(width, height, paced_fps)) {
                        Ok(built) => built.encode,
                        Err(error) => return failure_code(&error, 4),
                    };
                    os_damage =
                        arcen_keel::ExternalDamage::new(width as usize, height as usize).ok();
                }
            }
            Ok(Activity::None) => {}
            Err(error) => return failure_code(&error, 3),
        }

        let now = Instant::now();
        if let Some(fps) = control.take_framerate_fps() {
            if encode.set_frame_rate(fps) {
                interval = crate::frame_interval_from_fps(fps);
                paced_fps = fps;
                crate::log(&format!("OpenH264 live framerate reconfigured: fps={fps}"));
            }
        }
        if let Some(bps) = control.take_bitrate_bps() {
            encode.set_link_bitrate(bps);
            crate::log(&format!("software loop live bitrate: target={bps}bps"));
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
        if now >= next_tick {
            // Without XDamage nothing is vouched for: every tick looks.
            let wanted = encode.wants_frame(now, damage_pending || !capture.has_damage());
            pacing.record_due(wanted.then_some(crate::linux_policy::SubmissionMode::Activity));
            if wanted {
                let work_started = Instant::now();
                let narrowed = os_damage
                    .as_mut()
                    .is_some_and(|damage| capture.take_damage_into(damage));
                damage_pending = false;
                let tick = {
                    let source = match capture.capture_bgra(&mut bgra_scratch) {
                        Ok(source) => source,
                        Err(error) => return failure_code(&error, 3),
                    };
                    encode.tick(
                        source,
                        if narrowed { os_damage.as_ref() } else { None },
                        now,
                    )
                };
                busy = busy.saturating_add(work_started.elapsed());
                match tick {
                    Ok(tick) => {
                        if tick.records.is_empty() {
                            // The tick found the same pixels.
                            pacing.note_unchanged_content();
                        }
                        if let Some(event) = tick.event {
                            crate::log(&format!("canvas governor: {event:?}"));
                        }
                        if let Some((generation, digest)) = tick.digest {
                            crate::log(&format!(
                                "canvas mirror digest {}",
                                arcen_media::canvas::digest_fields(generation, &digest)
                            ));
                        }
                        for record in &tick.records {
                            if !ready {
                                if let Err(error) = crate::announce_ready_from(
                                    plan,
                                    Some(arcen_media::video::CaptureBackend::XShm),
                                ) {
                                    return failure_code(&format!("emit READY: {error}"), 5);
                                }
                                ready = true;
                            }
                            if crate::write_loop_record(&mut stdout, record, framed_v1, framed_v2)
                                .is_err()
                            {
                                return 0;
                            }
                            bytes = bytes.saturating_add(record.bytes().len() as u64);
                        }
                        if !tick.records.is_empty() {
                            emitted = emitted.saturating_add(1);
                        }
                    }
                    Err(arcen_media::canvas::LoopError::Geometry) => {
                        return failure_code("software loop: capture size changed", 3);
                    }
                    Err(error) => {
                        // The loop recovers by itself at its next tick.
                        damage_pending = true;
                        if last_error.is_none_or(|at| at.elapsed() >= Duration::from_secs(1)) {
                            crate::log(&format!("software loop tick failed: {error}"));
                            last_error = Some(Instant::now());
                        }
                    }
                }
            }
            next_tick += interval;
            if next_tick < now {
                next_tick = now + interval;
            }
        }
        if last_stats.elapsed() >= Duration::from_secs(1) {
            let canvas_on = encode.is_canvas();
            let state = encode.governor_state();
            crate::log(&format!(
                "enc_fps={emitted} kbps={} backend=openh264 damage_source={} keel_enabled=true {} {}",
                bytes.saturating_mul(8) / 1000,
                if capture.has_damage() {
                    "xdamage"
                } else {
                    "full_poll"
                },
                pacing.verdict_fields(fps, paced_fps, busy, last_stats.elapsed()),
                encode.take_stats().line(canvas_on, state),
            ));
            emitted = 0;
            bytes = 0;
            pacing = crate::linux_policy::PacingWindow::default();
            busy = Duration::ZERO;
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
        let layout =
            checked_layout(1920, 1080, 24, 32, 32, SOFTWARE_MAX_CAPTURE_BYTES).expect("layout");
        assert_eq!(layout.stride, 1920 * 4);
        assert_eq!(layout.byte_len, 1920 * 1080 * 4);
        let secondary = checked_layout(1800, 1130, 24, 32, 32, SOFTWARE_MAX_CAPTURE_BYTES)
            .expect("Philips layout");
        assert_eq!(secondary.byte_len, 1800 * 1130 * 4);
        assert!(checked_layout(1920, 1080, 24, 24, 32, SOFTWARE_MAX_CAPTURE_BYTES).is_err());
    }

    /// Depth 30 is accepted and lays out exactly like depth 24, because both
    /// are 32 bits per pixel.
    ///
    /// This is the ten-bit Linux path's entry condition. `XShmGetImage`
    /// returns the screen's own framebuffer format, so ten-bit capture on
    /// X11 is reached by running the server at depth 30 and reading it --
    /// not by asking the capture for more bits. Refusing depth 30 here made
    /// a correctly configured ten-bit server fail to capture at all.
    #[test]
    fn depth_thirty_is_accepted_and_lays_out_like_depth_twenty_four() {
        let wide =
            checked_layout(1920, 1080, 30, 32, 32, WIDE_MAX_CAPTURE_BYTES).expect("depth 30");
        let narrow =
            checked_layout(1920, 1080, 24, 32, 32, WIDE_MAX_CAPTURE_BYTES).expect("depth 24");
        assert_eq!(wide.stride, narrow.stride);
        assert_eq!(wide.byte_len, narrow.byte_len);
        assert_eq!(wide.depth, 30);
    }

    #[test]
    fn live_nvidia_depth_thirty_masks_select_xbgr2101010() {
        assert_eq!(
            classify_pixel_format(
                30,
                VisualClass::TRUE_COLOR,
                0x0000_03ff,
                0x000f_fc00,
                0x3ff0_0000,
            ),
            Ok(X11PixelFormat::Rgb10(PackedRgb10Layout::XBGR2101010))
        );
        assert_eq!(
            classify_pixel_format(
                30,
                VisualClass::TRUE_COLOR,
                0x3ff0_0000,
                0x000f_fc00,
                0x0000_03ff,
            ),
            Ok(X11PixelFormat::Rgb10(PackedRgb10Layout::XRGB2101010))
        );
    }

    #[test]
    fn visual_classification_rejects_ambiguous_or_non_true_color_layouts() {
        assert!(classify_pixel_format(
            30,
            VisualClass::DIRECT_COLOR,
            0x0000_03ff,
            0x000f_fc00,
            0x3ff0_0000,
        )
        .is_err());
        assert!(classify_pixel_format(
            30,
            VisualClass::TRUE_COLOR,
            0x0000_03ff,
            0x0000_03ff,
            0x3ff0_0000,
        )
        .is_err());
    }

    #[test]
    fn rgb10_precision_stats_detect_values_the_eight_bit_grid_cannot_hold() {
        let expanded_mid = (128_u32 << 2) | (128_u32 >> 6);
        let on_grid = (0x3_u32 << 30) | (expanded_mid << 20) | (expanded_mid << 10) | expanded_mid;
        let off_grid = (0x3_u32 << 30) | (513_u32 << 20);
        let bytes = [on_grid.to_le_bytes(), off_grid.to_le_bytes()].concat();
        let frame = WideX11Frame {
            bytes: &bytes,
            width: 2,
            height: 1,
            stride: 8,
            layout: PackedRgb10Layout::XBGR2101010,
        };
        let stats = frame.precision_stats(1);
        assert_eq!(stats.sampled_components, 6);
        assert_eq!(stats.off_eight_bit_grid, 1);
        assert_eq!(stats.minimum, 0);
        assert_eq!(stats.maximum, 514);
    }

    #[test]
    fn geometry_rejects_negative_odd_and_oversize_rectangles() {
        assert!(validate_capture_geometry(
            CaptureRect {
                x: -1,
                y: 0,
                width: 1920,
                height: 1080,
            },
            CapturePurpose::Software,
        )
        .is_err());
        assert!(validate_capture_geometry(
            CaptureRect {
                x: 0,
                y: 0,
                width: 1919,
                height: 1080,
            },
            CapturePurpose::Software,
        )
        .is_err());
        assert!(validate_capture_geometry(
            CaptureRect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            CapturePurpose::Software,
        )
        .is_ok());
        assert!(validate_capture_geometry(
            CaptureRect {
                x: 0,
                y: 832,
                width: 1800,
                height: 1130,
            },
            CapturePurpose::Software,
        )
        .is_ok());
        assert!(validate_capture_geometry(
            CaptureRect {
                x: 0,
                y: 0,
                width: 2560,
                height: 1600,
            },
            CapturePurpose::Software,
        )
        .is_ok());
        assert!(validate_capture_geometry(
            CaptureRect {
                x: 0,
                y: 0,
                width: 3840,
                height: 2160,
            },
            CapturePurpose::Software,
        )
        .is_ok());
        assert!(validate_capture_geometry(
            CaptureRect {
                x: 0,
                y: 0,
                width: 3842,
                height: 2160,
            },
            CapturePurpose::Software,
        )
        .is_err());
        assert!(validate_capture_geometry(
            CaptureRect {
                x: 0,
                y: 0,
                width: 2560,
                height: 1600,
            },
            CapturePurpose::WideNvenc,
        )
        .is_ok());
    }

    #[test]
    fn wide_geometry_is_a_pixel_budget_not_a_uhd_box() {
        // The 0.17.0 refusal: a 3600x2262 Retina desktop (fewer pixels than
        // UHD, taller than 2160) at the offset the Pier armed it.
        let retina = CaptureRect {
            x: 120,
            y: 0,
            width: 3600,
            height: 2262,
        };
        assert!(validate_capture_geometry(retina, CapturePurpose::WideNvenc).is_ok());
        assert!(checked_layout(3600, 2262, 30, 32, 32, WIDE_MAX_CAPTURE_BYTES).is_ok());
        let five_k = CaptureRect {
            x: 0,
            y: 0,
            width: 5120,
            height: 2880,
        };
        let error = validate_capture_geometry(five_k, CapturePurpose::WideNvenc)
            .expect_err("over the pixel budget");
        assert!(error.contains("at most 8294400 pixels"), "{error}");
        // The probe captures a budget-sized part of a larger desktop.
        let probed = probe_rect(five_k);
        assert_eq!((probed.width, probed.height), (3840, 2160));
        assert!(validate_capture_geometry(probed, CapturePurpose::WideProbe).is_ok());
        assert_eq!(probe_rect(retina), retina);
    }

    #[test]
    fn image_reply_validation_detects_layout_drift() {
        let mut layout =
            checked_layout(1280, 720, 24, 32, 32, SOFTWARE_MAX_CAPTURE_BYTES).expect("layout");
        layout.visual = 42;
        assert!(validate_image_reply(24, 42, layout.byte_len, layout).is_ok());
        assert!(validate_image_reply(24, 43, layout.byte_len, layout).is_err());
        assert!(validate_image_reply(24, 42, layout.byte_len - 1, layout).is_err());
    }

    #[test]
    fn snapshot_reads_are_output_relative_and_carry_no_visual() {
        let output = CaptureRect {
            x: 2560,
            y: 0,
            width: 2560,
            height: 1600,
        };
        assert_eq!(read_origin(output, true, 2560 + 16, 32), (16, 32));
        assert_eq!(read_origin(output, false, 2560 + 16, 32), (2560 + 16, 32));

        let mut layout =
            checked_layout(1280, 720, 24, 32, 32, SOFTWARE_MAX_CAPTURE_BYTES).expect("layout");
        layout.visual = 42;
        // GetImage of a pixmap replies with visual None.
        let snapshot = read_layout(layout, true);
        assert!(validate_image_reply(24, x11rb::NONE, layout.byte_len, snapshot).is_ok());
        assert!(validate_image_reply(24, 42, layout.byte_len, snapshot).is_err());
        assert_eq!(read_layout(layout, false), layout);
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
    fn the_get_image_fallback_never_reads_regions() {
        assert!(!transfer_supports_regions(&Transfer::GetImage {
            storage: Vec::new()
        }));
    }

    #[test]
    fn a_capture_region_maps_onto_the_root_and_never_leaves_the_output() {
        let output = CaptureRect {
            x: 2560,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let region = |x, y, width, height| FrameRegion {
            x,
            y,
            width,
            height,
        };
        assert_eq!(
            region_on_root(output, region(10, 20, 64, 32)),
            Ok((2570, 20, 64, 32))
        );
        assert_eq!(
            region_on_root(output, region(1856, 1048, 64, 32)),
            Ok((4416, 1048, 64, 32))
        );
        for outside in [
            region(1857, 0, 64, 32),
            region(0, 1049, 64, 32),
            region(0, 0, 0, 32),
        ] {
            assert!(region_on_root(output, outside).is_err(), "{outside:?}");
        }
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
