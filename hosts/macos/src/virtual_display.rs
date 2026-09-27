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
    /// The classes exist but refused to produce a display.
    Refused,
    /// The requested size is not one a display can be.
    UnusableSize { width: u32, height: u32 },
}

impl std::fmt::Display for VirtualDisplayError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => {
                formatter.write_str("this macOS does not provide virtual displays")
            }
            Self::Refused => formatter.write_str("the display was not created"),
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

impl VirtualDisplay {
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
        if width == 0 || height == 0 || width > MAX_EDGE || height > MAX_EDGE {
            return Err(VirtualDisplayError::UnusableSize { width, height });
        }
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
                return Err(VirtualDisplayError::Refused);
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
            }
            let _: () = msg_send![&*descriptor, setVendorID: 0x3456_u32];
            let _: () = msg_send![&*descriptor, setSerialNumber: ARCEN_SERIAL];
            // The descriptor wants a queue to deliver its callbacks on. Set
            // only when it will take one, because a host that cannot set it
            // still gets a display.
            if descriptor.class().responds_to(sel!(setQueue:)) {
                // A libdispatch queue is an Objective-C object, and the
                // selector is typed for one. Passing the global as a raw
                // pointer is rejected by the runtime's own type check, which
                // is a better error than whatever a mistyped send would have
                // done at the far end.
                if let Some(queue) = main_queue_object() {
                    let _: () = msg_send![&*descriptor, setQueue: queue];
                }
            }

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
                return Err(VirtualDisplayError::Refused);
            };

            let settings: Option<Retained<AnyObject>> = msg_send![settings_class, new];
            let Some(settings) = settings else {
                return Err(VirtualDisplayError::Refused);
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
                return Err(VirtualDisplayError::Refused);
            };
            let applied: bool = msg_send![&*display, applySettings: &*settings];
            if !applied {
                return Err(VirtualDisplayError::Refused);
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

/// Looks a class up by name, so absence is a value rather than a link error.
fn class_named(name: &str) -> Result<&'static AnyClass, VirtualDisplayError> {
    AnyClass::get(&std::ffi::CString::new(name).map_err(|_| VirtualDisplayError::Unsupported)?)
        .ok_or(VirtualDisplayError::Unsupported)
}

/// The main dispatch queue as the object the selector expects.
///
/// Returns `None` rather than a dangling reference if the symbol is ever not
/// where it is expected, because a descriptor without a queue still produces a
/// display.
fn main_queue_object() -> Option<&'static AnyObject> {
    // SAFETY: `_dispatch_main_q` is a process-lifetime libdispatch global, and
    // libdispatch objects are Objective-C objects on every macOS this runs on.
    unsafe {
        let queue: *mut AnyObject = std::ptr::addr_of_mut!(_dispatch_main_q).cast();
        queue.as_ref()
    }
}

unsafe extern "C" {
    static mut _dispatch_main_q: [u8; 0];
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
