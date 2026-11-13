// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Which interfaces of an authorized tablet a host may publish as HID devices
//! of its own (ADR 0022 §5).
//!
//! The Linux Pier presents a bridged tablet on a private seat (ADR 0021):
//! its keyboard, keypad and mouse interfaces reach that seat's session and no
//! other. A host that publishes `IOHIDUserDevice`s has no such isolation: a
//! published device is system-wide, so a keyboard interface would be a real
//! keyboard for whatever is on the console — another user's session after a
//! Fast User Switch, or the login window — beyond the reach of secure input.
//! [`HidPublishPolicy::PenAndVendorOnly`] therefore publishes only interfaces
//! that are pen digitizers or the tablet's own vendor-defined interfaces, and
//! whose inputs are pen axes, buttons, digitizer usages or usages on the
//! interface's own vendor page.
//!
//! "Vendor-defined" is not the same as "inert": macOS's `IOHIDEventDriver`
//! gives meaning to Apple's own vendor pages. `0xFF01` (Apple vendor
//! keyboard) is Spotlight, Launchpad, Fn, brightness and more; `0xFF00`
//! carries Apple vendor-message and Touch Bar collections; others are mouse,
//! motion, multitouch and filtered events (values from Apple's open-source
//! `IOHIDFamily` `AppleHIDUsageTables.h`). Those pages are refused outright, in
//! collections and in inputs ([`APPLE_EVENT_PAGES`]). Newer Apple pages whose
//! values Apple no longer publishes (sensor, HID-event phases) cannot be named
//! here; a vendor-page input must therefore be on the page of the
//! application collection it belongs to — the tablet's own page — and the
//! keyboard-like Apple pages act only on devices that carry Apple's
//! `AppleVendorSupported` property, which a host publishing bridged devices
//! never sets.

use crate::parse_report_descriptor;
use std::fmt::{Display, Formatter};

const PAGE_GENERIC_DESKTOP: u16 = 0x01;
const PAGE_BUTTON: u16 = 0x09;
const PAGE_DIGITIZER: u16 = 0x0D;
const PAGE_VENDOR_FIRST: u16 = 0xFF00;
/// Apple vendor pages macOS's HID event driver turns into events: vendor
/// (messages, Touch Bar), vendor keyboard (Spotlight, Launchpad, Fn,
/// brightness, Mission Control), vendor mouse, motion, Touch Bar and its
/// brightness, filtered events, multitouch, and the top case (Fn,
/// brightness, keyboard illumination).
pub const APPLE_EVENT_PAGES: [u16; 9] = [
    0xFF00, 0xFF01, 0xFF02, 0xFF0C, 0xFF11, 0xFF12, 0xFF50, 0xFF60, 0x00FF,
];
/// Generic Desktop X, Y, Z, Rx, Ry, Rz, Slider, Dial and Wheel: axes only.
const GENERIC_DESKTOP_AXES: std::ops::RangeInclusive<u16> = 0x30..=0x38;
/// Usages tracked between two main items; more is refused as malformed.
const MAX_PENDING_USAGES: usize = 256;
/// `Push` depth, as in the shared descriptor parser.
const MAX_PUSH_DEPTH: usize = 8;
/// Collection nesting tracked; deeper is refused as malformed.
const MAX_COLLECTION_DEPTH: usize = 32;

/// Which authorized interfaces a host publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HidPublishPolicy {
    /// Every authorized interface (a host that isolates the device, as the
    /// Linux Pier does with a private seat).
    #[default]
    AllAuthorized,
    /// Pen digitizers and vendor-defined interfaces only (a host whose
    /// published devices are system-wide, as `IOHIDUserDevice` is on macOS).
    PenAndVendorOnly,
}

/// Why an interface is not published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishRefusal {
    /// The report descriptor cannot be read with bounds.
    Malformed,
    /// An application collection is neither a digitizer nor vendor-defined
    /// (a keyboard, keypad, mouse, consumer control, system control…).
    Collection { usage_page: u16, usage: u16 },
    /// An input declares a usage outside pen axes, buttons, digitizer and
    /// the collection's own vendor page (a key, consumer, system-control or
    /// Apple vendor usage).
    Input { usage_page: u16, usage: u16 },
    /// An Apple vendor page that macOS turns into events
    /// ([`APPLE_EVENT_PAGES`]).
    ApplePage { usage_page: u16, usage: u16 },
}

impl Display for PublishRefusal {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => formatter.write_str("its report descriptor cannot be read"),
            Self::Collection { usage_page, usage } => write!(
                formatter,
                "it is a {usage_page:#06x}:{usage:#04x} device, not a pen or vendor interface"
            ),
            Self::Input { usage_page, usage } => write!(
                formatter,
                "it reports usage {usage_page:#06x}:{usage:#04x}, which is not a pen usage"
            ),
            Self::ApplePage { usage_page, usage } => write!(
                formatter,
                "it uses Apple vendor usage {usage_page:#06x}:{usage:#04x}, which macOS turns into \
                 events"
            ),
        }
    }
}

/// Whether `policy` lets a host publish an interface with this report
/// descriptor.
///
/// # Errors
///
/// Why not.
pub fn publishable(policy: HidPublishPolicy, descriptor: &[u8]) -> Result<(), PublishRefusal> {
    match policy {
        HidPublishPolicy::AllAuthorized => Ok(()),
        HidPublishPolicy::PenAndVendorOnly => pen_or_vendor(descriptor),
    }
}

const fn vendor(page: u16) -> bool {
    page >= PAGE_VENDOR_FIRST
}

fn pen_or_vendor(descriptor: &[u8]) -> Result<(), PublishRefusal> {
    let summary = parse_report_descriptor(descriptor).map_err(|_| PublishRefusal::Malformed)?;
    if summary.collections.is_empty() {
        return Err(PublishRefusal::Malformed);
    }
    for collection in &summary.collections {
        if APPLE_EVENT_PAGES.contains(&collection.usage_page) {
            return Err(PublishRefusal::ApplePage {
                usage_page: collection.usage_page,
                usage: collection.usage,
            });
        }
        if collection.usage_page != PAGE_DIGITIZER && !vendor(collection.usage_page) {
            return Err(PublishRefusal::Collection {
                usage_page: collection.usage_page,
                usage: collection.usage,
            });
        }
    }
    for InputUsage {
        page,
        first,
        last,
        application,
    } in input_usages(descriptor)?
    {
        if APPLE_EVENT_PAGES.contains(&page) {
            return Err(PublishRefusal::ApplePage {
                usage_page: page,
                usage: first,
            });
        }
        let allowed = (vendor(page) && Some(page) == application)
            || page == PAGE_DIGITIZER
            || page == PAGE_BUTTON
            || (page == PAGE_GENERIC_DESKTOP
                && GENERIC_DESKTOP_AXES.contains(&first)
                && GENERIC_DESKTOP_AXES.contains(&last));
        if !allowed {
            return Err(PublishRefusal::Input {
                usage_page: page,
                usage: first,
            });
        }
    }
    Ok(())
}

/// One usage (or usage range) of an `Input` main item.
struct InputUsage {
    page: u16,
    first: u16,
    last: u16,
    /// Usage page of the innermost Application collection around it.
    application: Option<u16>,
}

/// Every usage that names an `Input` main item, with the page of the
/// Application collection it belongs to.
fn input_usages(descriptor: &[u8]) -> Result<Vec<InputUsage>, PublishRefusal> {
    let mut found = Vec::new();
    // The innermost Application collection's page at each open collection.
    let mut collections: Vec<Option<u16>> = Vec::new();
    let mut page: u16 = 0;
    let mut pages: Vec<u16> = Vec::new();
    let mut pending: Vec<(u16, u16, u16)> = Vec::new();
    let mut minimum: Option<(u16, u16)> = None;
    let mut offset = 0;
    while offset < descriptor.len() {
        let prefix = descriptor[offset];
        if prefix == 0xFE {
            // Long items carry no usages a host acts on; skip them whole.
            let size = usize::from(
                *descriptor
                    .get(offset + 1)
                    .ok_or(PublishRefusal::Malformed)?,
            );
            offset = offset.saturating_add(3 + size);
            continue;
        }
        let size = match prefix & 0x03 {
            3 => 4,
            other => usize::from(other),
        };
        let data = descriptor
            .get(offset + 1..offset + 1 + size)
            .ok_or(PublishRefusal::Malformed)?;
        let value = data
            .iter()
            .rev()
            .fold(0_u32, |value, byte| (value << 8) | u32::from(*byte));
        // A four-byte usage carries its own page in the upper half.
        let usage = |value: u32| -> (u16, u16) {
            if size == 4 {
                (
                    u16::try_from(value >> 16).unwrap_or(u16::MAX),
                    u16::try_from(value & 0xFFFF).unwrap_or(u16::MAX),
                )
            } else {
                (page, u16::try_from(value).unwrap_or(u16::MAX))
            }
        };
        match prefix & 0xFC {
            0x04 => page = u16::try_from(value).unwrap_or(u16::MAX),
            0xA4 => {
                if pages.len() >= MAX_PUSH_DEPTH {
                    return Err(PublishRefusal::Malformed);
                }
                pages.push(page);
            }
            0xB4 => page = pages.pop().ok_or(PublishRefusal::Malformed)?,
            0x08 => {
                let (usage_page, id) = usage(value);
                pending.push((usage_page, id, id));
            }
            0x18 => minimum = Some(usage(value)),
            0x28 => {
                let (usage_page, last) = usage(value);
                let first = minimum.take().map_or(last, |(_, first)| first);
                pending.push((usage_page, first.min(last), first.max(last)));
            }
            tag @ (0x80 | 0x90 | 0xA0 | 0xB0 | 0xC0) => {
                if let Some((usage_page, first)) = minimum.take() {
                    pending.push((usage_page, first, first));
                }
                let application = collections.last().copied().flatten();
                match tag {
                    0x80 => found.extend(pending.drain(..).map(|(page, first, last)| InputUsage {
                        page,
                        first,
                        last,
                        application,
                    })),
                    0xA0 => {
                        if collections.len() >= MAX_COLLECTION_DEPTH {
                            return Err(PublishRefusal::Malformed);
                        }
                        // Collection type 1 is Application; any other kind
                        // stays inside the enclosing application.
                        collections.push(if value == 1 {
                            pending.first().map(|(usage_page, _, _)| *usage_page)
                        } else {
                            application
                        });
                    }
                    0xC0 => {
                        collections.pop().ok_or(PublishRefusal::Malformed)?;
                    }
                    _ => {}
                }
                pending.clear();
            }
            _ => {}
        }
        if pending.len() > MAX_PENDING_USAGES {
            return Err(PublishRefusal::Malformed);
        }
        offset += 1 + size;
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{INTUOS5_PEN_REPORT_DESCRIPTOR, WacomIntuos5};

    const BOOT_KEYBOARD: &[u8] = &[
        0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25,
        0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0xC0,
    ];
    /// A standard pen: Digitizer/Pen with tip, in-range, X/Y and pressure.
    const STANDARD_PEN: &[u8] = &[
        0x05, 0x0D, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x20, 0xA1, 0x00, 0x09, 0x42, 0x09, 0x32, 0x15,
        0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x02, 0x81, 0x02, 0x95, 0x06, 0x81, 0x03, 0x05, 0x01,
        0x09, 0x30, 0x09, 0x31, 0x26, 0xFF, 0x7F, 0x75, 0x10, 0x95, 0x02, 0x81, 0x02, 0x05, 0x0D,
        0x09, 0x30, 0x26, 0xFF, 0x1F, 0x95, 0x01, 0x81, 0x02, 0xC0, 0xC0,
    ];

    #[test]
    fn a_host_without_seats_publishes_pens_and_vendor_interfaces_only() {
        let policy = HidPublishPolicy::PenAndVendorOnly;
        // The Intuos5: vendor pen (0xFF0D), touch on 0xFF00 (also Apple's
        // own vendor page, so withheld), boot mouse.
        assert_eq!(publishable(policy, INTUOS5_PEN_REPORT_DESCRIPTOR), Ok(()));
        assert_eq!(
            publishable(policy, WacomIntuos5::report_descriptor(1).unwrap()),
            Err(PublishRefusal::ApplePage {
                usage_page: 0xFF00,
                usage: 0x01
            })
        );
        assert_eq!(
            publishable(policy, WacomIntuos5::report_descriptor(2).unwrap()),
            Err(PublishRefusal::Collection {
                usage_page: 0x01,
                usage: 0x02
            })
        );
        assert_eq!(publishable(policy, STANDARD_PEN), Ok(()));
        assert_eq!(
            publishable(policy, BOOT_KEYBOARD),
            Err(PublishRefusal::Collection {
                usage_page: 0x01,
                usage: 0x06
            })
        );
        // A seat-isolating host publishes everything it authorized.
        assert_eq!(
            publishable(HidPublishPolicy::AllAuthorized, BOOT_KEYBOARD),
            Ok(())
        );
    }

    #[test]
    fn keys_hidden_inside_a_pen_or_vendor_collection_are_refused() {
        let policy = HidPublishPolicy::PenAndVendorOnly;
        // A digitizer collection whose input is the keyboard modifier range.
        let keys_in_pen = [
            0x05, 0x0D, 0x09, 0x02, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x75, 0x01,
            0x95, 0x08, 0x81, 0x02, 0xC0,
        ];
        assert_eq!(
            publishable(policy, &keys_in_pen),
            Err(PublishRefusal::Input {
                usage_page: 0x07,
                usage: 0xE0
            })
        );
        // A vendor collection carrying a consumer usage as an extended usage.
        let consumer_in_vendor = [
            0x06, 0xA0, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x0B, 0xE9, 0x00, 0x0C, 0x00, 0x75, 0x01,
            0x95, 0x01, 0x81, 0x02, 0xC0,
        ];
        assert_eq!(
            publishable(policy, &consumer_in_vendor),
            Err(PublishRefusal::Input {
                usage_page: 0x0C,
                usage: 0xE9
            })
        );
        // System control (power, sleep) in a vendor collection.
        let system_in_vendor = [
            0x06, 0xA0, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x05, 0x01, 0x09, 0x82, 0x75, 0x01, 0x95,
            0x01, 0x81, 0x02, 0xC0,
        ];
        assert_eq!(
            publishable(policy, &system_in_vendor),
            Err(PublishRefusal::Input {
                usage_page: 0x01,
                usage: 0x82
            })
        );
        // Keyboard usages on a feature or output item inject nothing.
        let leds_in_vendor = [
            0x06, 0xA0, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x75,
            0x01, 0x95, 0x05, 0x91, 0x02, 0xC0,
        ];
        assert_eq!(publishable(policy, &leds_in_vendor), Ok(()));
        // Truncated and unbalanced descriptors are refused, not guessed at.
        assert_eq!(
            publishable(policy, &STANDARD_PEN[..STANDARD_PEN.len() - 1]),
            Err(PublishRefusal::Malformed)
        );
        assert_eq!(publishable(policy, &[0x05]), Err(PublishRefusal::Malformed));
    }

    #[test]
    fn apple_vendor_pages_that_macos_turns_into_events_are_refused() {
        let policy = HidPublishPolicy::PenAndVendorOnly;
        // An Apple vendor keyboard collection: Spotlight (0xFF01:0x01) …
        // Launchpad (0x04).
        let apple_keyboard = [
            0x06, 0x01, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x19, 0x01, 0x29, 0x04, 0x75, 0x01, 0x95,
            0x04, 0x81, 0x02, 0xC0,
        ];
        assert_eq!(
            publishable(policy, &apple_keyboard),
            Err(PublishRefusal::ApplePage {
                usage_page: 0xFF01,
                usage: 0x01
            })
        );
        // The same keys inside a tablet's own vendor collection (0xFF0D),
        // as a page switch and as an extended usage.
        let apple_keys_in_pen = [
            0x06, 0x0D, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x06, 0x01, 0xFF, 0x09, 0x04, 0x75, 0x01,
            0x95, 0x01, 0x81, 0x02, 0xC0,
        ];
        assert_eq!(
            publishable(policy, &apple_keys_in_pen),
            Err(PublishRefusal::ApplePage {
                usage_page: 0xFF01,
                usage: 0x04
            })
        );
        let extended_brightness = [
            0x06, 0x0D, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x0B, 0x20, 0x00, 0x01, 0xFF, 0x75, 0x01,
            0x95, 0x01, 0x81, 0x02, 0xC0,
        ];
        assert_eq!(
            publishable(policy, &extended_brightness),
            Err(PublishRefusal::ApplePage {
                usage_page: 0xFF01,
                usage: 0x20
            })
        );
        // Every listed page is refused as a collection.
        for page in APPLE_EVENT_PAGES {
            let [low, high] = page.to_le_bytes();
            let collection = [
                0x06, low, high, 0x09, 0x01, 0xA1, 0x01, 0x09, 0x01, 0x75, 0x08, 0x95, 0x01, 0x81,
                0x02, 0xC0,
            ];
            assert!(
                matches!(
                    publishable(policy, &collection),
                    Err(PublishRefusal::ApplePage { usage_page, .. }) if usage_page == page
                ),
                "{page:#06x}"
            );
        }
    }

    #[test]
    fn vendor_inputs_must_be_on_the_interface_s_own_vendor_page() {
        let policy = HidPublishPolicy::PenAndVendorOnly;
        // A tablet's vendor collection (0xFFA0) with inputs on its own page.
        let own_page = [
            0x06, 0xA0, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x09, 0x02, 0x75, 0x08, 0x95, 0x04, 0x81,
            0x02, 0xC0,
        ];
        assert_eq!(publishable(policy, &own_page), Ok(()));
        // The same collection reporting on another vendor page, which this
        // host cannot vouch for (an Apple page whose value is not public).
        let other_page = [
            0x06, 0xA0, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x06, 0xA1, 0xFF, 0x09, 0x02, 0x75, 0x08,
            0x95, 0x04, 0x81, 0x02, 0xC0,
        ];
        assert_eq!(
            publishable(policy, &other_page),
            Err(PublishRefusal::Input {
                usage_page: 0xFFA1,
                usage: 0x02
            })
        );
        // Vendor inputs inside a digitizer collection are not the tablet's
        // own page either.
        let vendor_in_pen = [
            0x05, 0x0D, 0x09, 0x02, 0xA1, 0x01, 0x06, 0xA0, 0xFF, 0x09, 0x02, 0x75, 0x08, 0x95,
            0x01, 0x81, 0x02, 0xC0,
        ];
        assert_eq!(
            publishable(policy, &vendor_in_pen),
            Err(PublishRefusal::Input {
                usage_page: 0xFFA0,
                usage: 0x02
            })
        );
    }
}
