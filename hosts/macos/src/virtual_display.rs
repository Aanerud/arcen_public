//! A display that exists only so a Deck can be sent the desktop it asked for.
//!
//! A headless Mac has one framebuffer, whatever size macOS decided to
//! synthesise, and `CGDisplaySetDisplayMode` cannot change it because there is
//! no second mode to change to — measured on the lab Mac Studio, which offers
//! exactly 1920x1080 even when asked for scaled and duplicate-resolution
//! modes. A Deck with a larger screen therefore gets a smaller desktop scaled
//! up, which is soft in a way no amount of bitrate fixes.
//!
//! The way out is to add a display. `CGVirtualDisplay` and its three companion
//! classes do that, and the reference agent this was checked against depends
//! on exactly them.
//!
//! # These classes are private
//!
//! They appear in no public SDK header. That is a real cost and it is taken
//! deliberately, so the terms are worth stating:
//!
//! - Everything here is reached **by name at runtime**. If a future macOS
//!   renames or removes any of it, every lookup returns `None`, this returns
//!   `None`, and the host serves the real display exactly as it does today.
//!   There is no version check to forget to update and no symbol to fail to
//!   link against.
//! - Nothing is called that is not needed to create a display and give it a
//!   mode. No private behaviour is relied on beyond the existence of these
//!   four classes and the selectors named below.
//! - A host that cannot create one says so, and is still a working host.
//!
//! The alternative is a desktop that is whatever size the host's framebuffer
//! happens to be, forever, on every headless machine.

#![allow(unsafe_code)]

use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{msg_send, sel};
use serde::{Deserialize, Serialize};
use std::io::{BufRead as _, Write as _};

/// A live virtual display.
///
/// Dropping it removes the display, which is why the session holds one for as
/// long as it is streaming and not a moment longer: a virtual display left
/// behind is a desktop the next person to sit at the machine has to find and
/// remove.
#[derive(Debug)]
pub struct VirtualDisplay {
    display: Retained<AnyObject>,
    width: u32,
    height: u32,
}

impl VirtualDisplayChild {
    /// Starts a fresh owner process for one session's virtual displays.
    ///
    /// Keeping `CGVirtualDisplay` objects in a child process avoids
    /// WindowServer's per-process stale virtual-display state after the first
    /// session. Dropping this lease closes stdin; the child exits and releases
    /// every display it owns.
    ///
    /// # Errors
    ///
    /// Returns a string naming spawn, protocol, creation, or arrangement
    /// failure.
    pub fn start(
        requests: &[VirtualDisplayChildRequest],
    ) -> Result<(Self, Vec<VirtualDisplayChildSnapshot>), String> {
        let exe =
            std::env::current_exe().map_err(|error| format!("current executable: {error}"))?;
        let child = std::process::Command::new(exe)
            .arg("virtual-display-child")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .map_err(|error| format!("spawn virtual display child: {error}"))?;
        // Owned from the moment it exists: every early return below drops
        // `owner`, whose `Drop` closes the pipe and reaps the child, so a
        // failed start never leaves a process behind.
        let mut owner = Self { child, stdin: None };
        let mut stdin = owner
            .child
            .stdin
            .take()
            .ok_or_else(|| "virtual display child stdin unavailable".to_owned())?;
        let stdout = owner
            .child
            .stdout
            .take()
            .ok_or_else(|| "virtual display child stdout unavailable".to_owned())?;
        serde_json::to_writer(&mut stdin, requests)
            .map_err(|error| format!("serialize virtual display request: {error}"))?;
        stdin
            .write_all(b"\n")
            .map_err(|error| format!("send virtual display request: {error}"))?;
        stdin
            .flush()
            .map_err(|error| format!("flush virtual display request: {error}"))?;

        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|error| format!("read virtual display child response: {error}"))?;
        if line.trim().is_empty() {
            let status = owner
                .child
                .try_wait()
                .map_err(|error| format!("query virtual display child: {error}"))?;
            return Err(format!(
                "virtual display child exited before response: {status:?}"
            ));
        }
        let response: Result<VirtualDisplayChildResponse, String> = serde_json::from_str(&line)
            .map_err(|error| format!("decode child response: {error}"))?;
        let response = response?;
        owner.stdin = Some(stdin);
        Ok((owner, response.displays))
    }
}

impl Drop for VirtualDisplayChild {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Ok(None) | Err(_) => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return;
                }
            }
        }
    }
}

/// Runs the virtual-display owner child protocol on stdin/stdout.
///
/// # Errors
///
/// Returns a string naming the creation or arrangement failure. The parent
/// receives the same string as a JSON `Err`.
pub fn run_child_stdio() -> Result<(), String> {
    let stdin = std::io::stdin();
    let mut line = String::new();
    stdin
        .lock()
        .read_line(&mut line)
        .map_err(|error| format!("read virtual display request: {error}"))?;
    let requests: Vec<VirtualDisplayChildRequest> = serde_json::from_str(&line)
        .map_err(|error| format!("parse virtual display request: {error}"))?;
    let result = child_create_and_hold(&requests);
    let response = result
        .as_ref()
        .map(|(_, response)| response.clone())
        .map_err(Clone::clone);
    let mut stdout = std::io::stdout();
    serde_json::to_writer(&mut stdout, &response)
        .map_err(|error| format!("serialize virtual display response: {error}"))?;
    stdout
        .write_all(b"\n")
        .map_err(|error| format!("write virtual display response: {error}"))?;
    stdout
        .flush()
        .map_err(|error| format!("flush virtual display response: {error}"))?;
    let (_displays, _snapshots) = result?;
    let mut hold = String::new();
    let _ = stdin.lock().read_line(&mut hold);
    Ok(())
}

fn child_create_and_hold(
    requests: &[VirtualDisplayChildRequest],
) -> Result<(Vec<VirtualDisplay>, VirtualDisplayChildResponse), String> {
    let mut displays = Vec::with_capacity(requests.len());
    let mut snapshots = Vec::with_capacity(requests.len());
    for request in requests {
        let identity = PanelIdentity {
            size_mm: request.size_mm,
            name: request.name.clone(),
            color: None,
            serial: request.serial,
        };
        let panel = if request.hdr {
            VirtualPanel::Hdr
        } else {
            VirtualPanel::Sdr
        };
        let display = VirtualDisplay::create_panel(
            request.width,
            request.height,
            request.refresh_hz,
            panel,
            Some(&identity),
        )
        .map_err(|error| format!("{error}"))?;
        let display_id = display.display_id();
        snapshots.push(VirtualDisplayChildSnapshot {
            display_id,
            width: request.width,
            height: request.height,
            x: request.x,
            y: request.y,
        });
        displays.push(display);
    }
    let origins = snapshots
        .iter()
        .map(|display| (display.display_id, display.x, display.y))
        .collect::<Vec<_>>();
    arrange_origins(&origins).map_err(|error| error.to_string())?;
    Ok((
        displays,
        VirtualDisplayChildResponse {
            displays: snapshots,
        },
    ))
}

// SAFETY: a `CGVirtualDisplay` is a CoreFoundation-style object whose retain
// and release are atomic and whose ownership carries no thread affinity. What
// this type does with it is create it, ask it for a display id, and release it;
// none of those is documented to require the thread that created it, and the
// window server owns the display itself rather than this process.
//
// Only `Send` is asserted. `Sync` would permit two threads to use one display
// at once, which nothing here needs.
//
// Asserted because the handshake owns one and the handshake is moved between
// tasks: without this, arranging a display would quietly make every session
// unspawnable.
unsafe impl Send for VirtualDisplay {}

/// Why a virtual display could not be created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VirtualDisplayError {
    /// The private classes are not present on this macOS.
    ///
    /// Expected rather than exceptional: it is what a future release removing
    /// them looks like, and it is why every caller must have a path that does
    /// not need one.
    Unsupported,
    /// The descriptor object could not be allocated.
    DescriptorUnavailable(VirtualDisplayRequest),
    /// The requested mode could not be initialised.
    ModeRejected(VirtualDisplayRequest),
    /// The settings object could not be allocated.
    SettingsUnavailable(VirtualDisplayRequest),
    /// The display object could not be initialised from its descriptor.
    DisplayInitRefused(VirtualDisplayRequest),
    /// The display object exists, but rejected the requested settings.
    SettingsNotApplied(VirtualDisplayRequest),
    /// The requested size is not one a display can be.
    UnusableSize { width: u32, height: u32 },
}

/// Display creation request metadata carried by every native refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualDisplayRequest {
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32,
    pub serial: u32,
}

/// One display the agent asks a short-lived child process to own.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VirtualDisplayChildRequest {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: f64,
    pub hdr: bool,
    pub size_mm: (u32, u32),
    pub name: String,
    pub serial: u32,
    pub x: i32,
    pub y: i32,
}

/// A running child process holding session virtual displays.
#[derive(Debug)]
pub struct VirtualDisplayChild {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
}

/// One display reported by the child after creation and arrangement.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct VirtualDisplayChildSnapshot {
    pub display_id: u32,
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct VirtualDisplayChildResponse {
    displays: Vec<VirtualDisplayChildSnapshot>,
}

impl VirtualDisplayRequest {
    fn new(width: u32, height: u32, refresh_hz: f64, serial: u32) -> Self {
        let refresh_millihz = if refresh_hz.is_finite() && refresh_hz > 0.0 {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            {
                (refresh_hz * 1000.0)
                    .round()
                    .clamp(1.0, f64::from(u32::MAX)) as u32
            }
        } else {
            0
        };
        Self {
            width,
            height,
            refresh_millihz,
            serial,
        }
    }

    fn refresh_hz(self) -> f64 {
        f64::from(self.refresh_millihz) / 1000.0
    }
}

impl std::fmt::Display for VirtualDisplayError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => {
                formatter.write_str("this macOS does not provide virtual displays")
            }
            Self::DescriptorUnavailable(request) => write!(
                formatter,
                "CGVirtualDisplayDescriptor new returned nil for {}x{} @ {:.3} Hz serial 0x{:08X}",
                request.width,
                request.height,
                request.refresh_hz(),
                request.serial
            ),
            Self::ModeRejected(request) => write!(
                formatter,
                "CGVirtualDisplayMode rejected {}x{} @ {:.3} Hz serial 0x{:08X}",
                request.width,
                request.height,
                request.refresh_hz(),
                request.serial
            ),
            Self::SettingsUnavailable(request) => write!(
                formatter,
                "CGVirtualDisplaySettings new returned nil for {}x{} @ {:.3} Hz serial 0x{:08X}",
                request.width,
                request.height,
                request.refresh_hz(),
                request.serial
            ),
            Self::DisplayInitRefused(request) => write!(
                formatter,
                "CGVirtualDisplay initWithDescriptor returned nil for {}x{} @ {:.3} Hz serial 0x{:08X}",
                request.width,
                request.height,
                request.refresh_hz(),
                request.serial
            ),
            Self::SettingsNotApplied(request) => write!(
                formatter,
                "CGVirtualDisplay applySettings returned false for {}x{} @ {:.3} Hz serial 0x{:08X}",
                request.width,
                request.height,
                request.refresh_hz(),
                request.serial
            ),
            Self::UnusableSize { width, height } => {
                write!(formatter, "{width}x{height} is not a usable display size")
            }
        }
    }
}

impl std::error::Error for VirtualDisplayError {}

/// The largest edge a virtual display may be asked for.
///
/// Well past any real panel, and short of a size that would make the backing
/// framebuffer allocation absurd.
const MAX_EDGE: u32 = 16_384;

/// Identifies displays this host created, so it never adopts somebody else's.
///
/// The reference tracks its own displays by serial number for the same reason;
/// its log says as much when one does not match.
const ARCEN_SERIAL: u32 = 0x4152_4345;

/// What kind of panel the virtual display presents itself as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtualPanel {
    /// An ordinary SDR panel with BT.709 primaries. The default, unchanged.
    Sdr,
    /// An HDR panel: PQ-capable, BT.2020 primaries.
    ///
    /// Measured on the lab: a mode created with transfer function 1 gives the
    /// display 5x potential EDR headroom and HDR mode enabled, while 0 and 2-6
    /// stay SDR. BT.2020 primaries are what the HDR wire contract carries, so
    /// wide-gamut content is not clipped to a narrower panel before capture.
    Hdr,
}

/// What the arranged panel takes from the Deck's own display.
///
/// Its physical size, so the host's desktop has the Deck panel's density
/// rather than one invented here, and its name, so an operator can see which
/// display the session is serving. The vendor, product and serial stay
/// Arcen's: the host recognises its own displays by them, and adopting the
/// Deck panel's identity could pull that panel's stored settings onto it.
#[derive(Debug, Clone, PartialEq)]
pub struct PanelIdentity {
    /// Physical width and height in millimetres.
    pub size_mm: (u32, u32),
    /// The Deck display's name.
    pub name: String,
    /// The Deck display's colour report, interpreted by the shared rule.
    pub color: Option<arcen_media::display_color::DisplayColor>,
    /// Stable Arcen-owned serial number for this virtual display identity.
    pub serial: u32,
}

/// The mode transfer function that makes a virtual display HDR, measured.
const HDR_TRANSFER_FUNCTION: u32 = 1;

/// BT.2020 red, green, blue and D65 white chromaticities.
const BT2020_PRIMARIES: [(f64, f64); 4] = [
    (0.708, 0.292),
    (0.170, 0.797),
    (0.131, 0.046),
    (0.3127, 0.3290),
];

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        objc2::rc::autoreleasepool(|_| {
            if self
                .display
                .class()
                .instance_method(sel!(invalidate))
                .is_some()
            {
                // SAFETY: `invalidate` is an instance selector on this live
                // CGVirtualDisplay object when the runtime reports it exists.
                unsafe {
                    let _: () = msg_send![&*self.display, invalidate];
                }
            }
        });
    }
}

impl VirtualDisplay {
    /// Returns whether this macOS exposes the virtual-display classes Arcen needs.
    ///
    /// This is a non-mutating capability check used before advertising
    /// multi-monitor-v1 from a headless or single-display Pier. Actual display
    /// creation still remains the proof for a session.
    #[must_use]
    pub fn is_supported() -> bool {
        class_named("CGVirtualDisplayDescriptor").is_ok()
            && class_named("CGVirtualDisplayMode").is_ok()
            && class_named("CGVirtualDisplaySettings").is_ok()
            && class_named("CGVirtualDisplay").is_ok()
    }

    /// Creates an SDR display of exactly this size.
    ///
    /// # Errors
    ///
    /// Returns [`VirtualDisplayError::Unsupported`] when this macOS does not
    /// provide the classes, which every caller must be able to continue from.
    pub fn create(width: u32, height: u32, refresh_hz: f64) -> Result<Self, VirtualDisplayError> {
        Self::create_panel(width, height, refresh_hz, VirtualPanel::Sdr, None)
    }

    /// Creates a display of exactly this size and kind.
    ///
    /// # Errors
    ///
    /// As [`Self::create`], and [`VirtualDisplayError::Unsupported`] when an
    /// HDR panel is asked of a macOS whose modes take no transfer function.
    pub fn create_panel(
        width: u32,
        height: u32,
        refresh_hz: f64,
        panel: VirtualPanel,
        identity: Option<&PanelIdentity>,
    ) -> Result<Self, VirtualDisplayError> {
        objc2::rc::autoreleasepool(|_| {
            Self::create_panel_inner(width, height, refresh_hz, panel, identity)
        })
    }

    fn create_panel_inner(
        width: u32,
        height: u32,
        refresh_hz: f64,
        panel: VirtualPanel,
        identity: Option<&PanelIdentity>,
    ) -> Result<Self, VirtualDisplayError> {
        if width == 0 || height == 0 || width > MAX_EDGE || height > MAX_EDGE {
            return Err(VirtualDisplayError::UnusableSize { width, height });
        }
        let serial = identity.map_or(ARCEN_SERIAL, |identity| identity.serial);
        let request = VirtualDisplayRequest::new(width, height, refresh_hz, serial);
        let descriptor_class = class_named("CGVirtualDisplayDescriptor")?;
        let mode_class = class_named("CGVirtualDisplayMode")?;
        let settings_class = class_named("CGVirtualDisplaySettings")?;
        let display_class = class_named("CGVirtualDisplay")?;

        // SAFETY: every object below is allocated and initialised through the
        // classes looked up above, and each selector is sent to an instance of
        // the class that declares it. Failure at any step is a null back,
        // which is checked rather than assumed.
        unsafe {
            let descriptor: Option<Retained<AnyObject>> = msg_send![descriptor_class, new];
            let Some(descriptor) = descriptor else {
                return Err(VirtualDisplayError::DescriptorUnavailable(request));
            };
            let name = objc2_foundation::NSString::from_str(&identity.map_or_else(
                || "Arcen Desktop".to_owned(),
                |identity| {
                    format!(
                        "Arcen – {}",
                        identity.name.chars().take(48).collect::<String>()
                    )
                },
            ));
            let _: () = msg_send![&*descriptor, setName: &*name];
            let _: () = msg_send![&*descriptor, setMaxPixelsWide: width];
            let _: () = msg_send![&*descriptor, setMaxPixelsHigh: height];
            // A plausible physical size keeps macOS from inferring an absurd
            // DPI. 25.4 mm per inch at roughly 109 dpi, the density of the
            // panels Apple ships at these sizes.
            let millimetres = identity.map_or_else(
                || objc2_foundation::NSSize {
                    width: f64::from(width) / 109.0 * 25.4,
                    height: f64::from(height) / 109.0 * 25.4,
                },
                |identity| objc2_foundation::NSSize {
                    width: f64::from(identity.size_mm.0),
                    height: f64::from(identity.size_mm.1),
                },
            );
            let _: () = msg_send![&*descriptor, setSizeInMillimeters: millimetres];
            // A different product for the HDR panel: macOS keeps per-display
            // settings by identity, and an SDR panel's must not carry over.
            let product: u32 = match panel {
                VirtualPanel::Sdr => 0x1234,
                VirtualPanel::Hdr => 0x1235,
            };
            let _: () = msg_send![&*descriptor, setProductID: product];
            if panel == VirtualPanel::Hdr {
                let point = |(x, y): (f64, f64)| objc2_foundation::NSPoint { x, y };
                let [red, green, blue, white] = BT2020_PRIMARIES;
                let _: () = msg_send![&*descriptor, setRedPrimary: point(red)];
                let _: () = msg_send![&*descriptor, setGreenPrimary: point(green)];
                let _: () = msg_send![&*descriptor, setBluePrimary: point(blue)];
                let _: () = msg_send![&*descriptor, setWhitePoint: point(white)];
            } else {
                // The SDR wire contract is BT.709, even when the Deck panel is
                // wider-gamut; host compositing must not invent P3 SDR.
            }
            let _: () = msg_send![&*descriptor, setVendorID: 0x3456_u32];
            let _: () = msg_send![&*descriptor, setSerialNumber: serial];
            // Do not point callbacks at `_dispatch_main_q`: the Pier agent
            // does not run a main queue, and after the first virtual-display
            // teardown a second display id can be allocated but never become
            // online. Leaving the descriptor's queue unset lets CoreGraphics
            // use the private default path that keeps repeated creation live.

            let mode: Allocated<AnyObject> = msg_send![mode_class, alloc];
            let mode: Option<Retained<AnyObject>> = match panel {
                VirtualPanel::Sdr => msg_send![
                    mode,
                    initWithWidth: width,
                    height: height,
                    refreshRate: refresh_hz,
                ],
                VirtualPanel::Hdr => {
                    if mode_class
                        .instance_method(sel!(initWithWidth:height:refreshRate:transferFunction:))
                        .is_none()
                    {
                        return Err(VirtualDisplayError::Unsupported);
                    }
                    msg_send![
                        mode,
                        initWithWidth: width,
                        height: height,
                        refreshRate: refresh_hz,
                        transferFunction: HDR_TRANSFER_FUNCTION,
                    ]
                }
            };
            let Some(mode) = mode else {
                return Err(VirtualDisplayError::ModeRejected(request));
            };

            let settings: Option<Retained<AnyObject>> = msg_send![settings_class, new];
            let Some(settings) = settings else {
                return Err(VirtualDisplayError::SettingsUnavailable(request));
            };
            let modes = objc2_foundation::NSArray::from_slice(&[&*mode]);
            let _: () = msg_send![&*settings, setModes: &*modes];
            // The Deck scales what it receives; a HiDPI backing store would
            // double the pixels encoded to deliver the same picture.
            let _: () = msg_send![&*settings, setHiDPI: 0_u32];

            let display: Allocated<AnyObject> = msg_send![display_class, alloc];
            let display: Option<Retained<AnyObject>> =
                msg_send![display, initWithDescriptor: &*descriptor];
            let Some(display) = display else {
                return Err(VirtualDisplayError::DisplayInitRefused(request));
            };
            let applied: bool = msg_send![&*display, applySettings: &*settings];
            if !applied {
                return Err(VirtualDisplayError::SettingsNotApplied(request));
            }
            Ok(Self {
                display,
                width,
                height,
            })
        }
    }

    /// The `CoreGraphics` display id to capture.
    #[must_use]
    pub fn display_id(&self) -> u32 {
        // SAFETY: `displayID` is a property on the live object held here.
        unsafe { msg_send![&*self.display, displayID] }
    }

    /// The size this display was created at.
    #[must_use]
    pub const fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// Arranges active displays at exact global origins.
///
/// The caller names only displays it owns. CoreGraphics applies the batch
/// atomically, so a failure leaves the previous desktop layout in place.
///
/// # Errors
///
/// Returns a typed CoreGraphics step error naming the display and origin that
/// failed.
pub fn arrange_origins(displays: &[(u32, i32, i32)]) -> Result<(), DisplayArrangementError> {
    #[cfg(target_os = "macos")]
    {
        arrange_origins_macos(displays)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = displays;
        Ok(())
    }
}

/// Native display-arrangement failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayArrangementError {
    Begin {
        code: i32,
    },
    Configure {
        display_id: u32,
        x: i32,
        y: i32,
        code: i32,
    },
    Complete {
        code: i32,
    },
    DisplayNotOnline {
        display_id: u32,
    },
}

impl std::fmt::Display for DisplayArrangementError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Begin { code } => write!(formatter, "CGBeginDisplayConfiguration failed: {code}"),
            Self::Configure {
                display_id,
                x,
                y,
                code,
            } => write!(
                formatter,
                "CGConfigureDisplayOrigin failed for display {display_id} at {x},{y}: {code}"
            ),
            Self::Complete { code } => {
                write!(formatter, "CGCompleteDisplayConfiguration failed: {code}")
            }
            Self::DisplayNotOnline { display_id } => {
                write!(formatter, "display {display_id} did not come online")
            }
        }
    }
}

impl std::error::Error for DisplayArrangementError {}

#[cfg(target_os = "macos")]
fn arrange_origins_macos(displays: &[(u32, i32, i32)]) -> Result<(), DisplayArrangementError> {
    wait_for_online_displays(displays)?;
    arrange_origins_once(displays).or_else(|first| {
        std::thread::sleep(std::time::Duration::from_millis(250));
        wait_for_online_displays(displays)?;
        arrange_origins_once(displays).map_err(|_| first)
    })
}

#[cfg(target_os = "macos")]
fn arrange_origins_once(displays: &[(u32, i32, i32)]) -> Result<(), DisplayArrangementError> {
    let mut config: CGDisplayConfigRef = std::ptr::null_mut();
    // SAFETY: CoreGraphics writes one opaque configuration handle to `config`.
    let begin = unsafe { CGBeginDisplayConfiguration(&raw mut config) };
    if begin != 0 {
        return Err(DisplayArrangementError::Begin { code: begin });
    }
    for (display_id, x, y) in displays {
        // SAFETY: `config` is a live configuration handle until completed or
        // cancelled, and display identifiers are plain values.
        let error = unsafe { CGConfigureDisplayOrigin(config, *display_id, *x, *y) };
        if error != 0 {
            // SAFETY: cancels the live configuration handle on failure.
            unsafe { CGCancelDisplayConfiguration(config) };
            return Err(DisplayArrangementError::Configure {
                display_id: *display_id,
                x: *x,
                y: *y,
                code: error,
            });
        }
    }
    // SAFETY: completes the live configuration for this login session.
    let complete = unsafe { CGCompleteDisplayConfiguration(config, K_CG_CONFIGURE_FOR_SESSION) };
    if complete == 0 {
        Ok(())
    } else {
        Err(DisplayArrangementError::Complete { code: complete })
    }
}

#[cfg(target_os = "macos")]
fn wait_for_online_displays(displays: &[(u32, i32, i32)]) -> Result<(), DisplayArrangementError> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let online = online_displays();
        if displays
            .iter()
            .all(|(display_id, _, _)| online.contains(display_id))
        {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            if let Some((display_id, _, _)) = displays
                .iter()
                .find(|(display_id, _, _)| !online.contains(display_id))
            {
                return Err(DisplayArrangementError::DisplayNotOnline {
                    display_id: *display_id,
                });
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(target_os = "macos")]
fn online_displays() -> Vec<u32> {
    let mut count = 0_u32;
    let mut ids = [0_u32; 32];
    // SAFETY: CoreGraphics writes up to the supplied capacity into `ids` and
    // the resulting count into `count`.
    let error = unsafe { CGGetOnlineDisplayList(32, ids.as_mut_ptr(), &raw mut count) };
    if error != 0 {
        return Vec::new();
    }
    let count = usize::try_from(count).unwrap_or(0).min(ids.len());
    ids[..count].to_vec()
}

#[cfg(target_os = "macos")]
type CGDisplayConfigRef = *mut std::ffi::c_void;

#[cfg(target_os = "macos")]
const K_CG_CONFIGURE_FOR_SESSION: u32 = 1;

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGBeginDisplayConfiguration(config: *mut CGDisplayConfigRef) -> i32;
    fn CGConfigureDisplayOrigin(config: CGDisplayConfigRef, display: u32, x: i32, y: i32) -> i32;
    fn CGCompleteDisplayConfiguration(config: CGDisplayConfigRef, option: u32) -> i32;
    fn CGCancelDisplayConfiguration(config: CGDisplayConfigRef) -> i32;
    fn CGGetOnlineDisplayList(
        max_displays: u32,
        online_displays: *mut u32,
        display_count: *mut u32,
    ) -> i32;
}

/// Looks a class up by name, so absence is a value rather than a link error.
fn class_named(name: &str) -> Result<&'static AnyClass, VirtualDisplayError> {
    AnyClass::get(&std::ffi::CString::new(name).map_err(|_| VirtualDisplayError::Unsupported)?)
        .ok_or(VirtualDisplayError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::{MAX_EDGE, VirtualDisplay, VirtualDisplayError};

    #[test]
    fn an_impossible_size_is_refused_before_anything_is_allocated() {
        assert!(matches!(
            VirtualDisplay::create(0, 1080, 60.0),
            Err(VirtualDisplayError::UnusableSize {
                width: 0,
                height: 1080
            })
        ));
        assert!(matches!(
            VirtualDisplay::create(MAX_EDGE + 1, 1080, 60.0),
            Err(VirtualDisplayError::UnusableSize { .. })
        ));
    }

    #[test]
    #[ignore = "creates a real display: reconfigures every screen on the machine running it"]
    fn an_unsupported_macos_is_an_ordinary_answer() {
        // Ignored by default, and the reason is not tidiness. Creating a
        // virtual display reconfigures the desktop: windows on a working
        // machine minimise, move and restore themselves, and any remote
        // session being viewed on it is disrupted. That happened to the
        // machine this was developed on, repeatedly, because a unit test ran
        // it on every `cargo test`.
        //
        // Run deliberately, on a machine whose desktop nobody is using:
        //   cargo test -p arcen-pier-macos --lib -- --ignored virtual_display
        match VirtualDisplay::create(1280, 720, 60.0) {
            Ok(display) => {
                assert_eq!(display.size(), (1280, 720));
                assert_ne!(display.display_id(), 0, "a created display has an id");
            }
            Err(error) => {
                eprintln!("no virtual display on this machine: {error}");
            }
        }
    }
}
