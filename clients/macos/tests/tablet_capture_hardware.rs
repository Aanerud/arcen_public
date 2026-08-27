//! Hardware test for experimental raw-HID tablet capture.
//!
//! `#[ignore]` by design: it needs a real tablet plugged into this Mac and
//! Input Monitoring granted to whatever runs it, so it must never run in CI or
//! in a normal `cargo test`. Run it deliberately:
//!
//! ```text
//! cargo test -p arcen-deck-macos --features experimental-raw-hid \
//!     --test tablet_capture_hardware -- --ignored --nocapture
//! ```
//!
//! It exists because the ordinary CLI path cannot reach this code:
//! `connect-smoke` returns after the handshake and never starts the IOKit run
//! loop, so before this the only way to exercise capture was a GUI session
//! with a pen in hand. Every earlier conclusion about which collections open
//! came from standalone probes that *re-implemented* the matching rather than
//! calling it -- and one of those conclusions was wrong. This drives the real
//! `HidSession`.

#![cfg(all(target_os = "macos", feature = "experimental-raw-hid"))]

use arcen_deck::hid::{HidEvent, HidSession, HID_EVENT_CHANNEL_CAPACITY};
use std::time::Duration;

/// How long to wait for enumeration. Device matching callbacks fire almost
/// immediately once the run loop is live; this is slack, not a tuning knob.
const ENUMERATION_TIMEOUT: Duration = Duration::from_secs(5);

#[test]
#[ignore = "requires a physical tablet attached to this Mac"]
fn a_real_tablet_is_captured_without_any_entitlement() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("build test runtime");

    runtime.block_on(async {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<HidEvent>(HID_EVENT_CHANNEL_CAPACITY);
        let session = HidSession::start(tx);

        let mut added = Vec::new();
        let mut errors = Vec::new();
        let deadline = tokio::time::Instant::now() + ENUMERATION_TIMEOUT;

        // Drain until the deadline rather than stopping at the first event:
        // a failure often arrives *after* a success (the manager opens, then a
        // device refuses), and stopping early would hide exactly that case.
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(HidEvent::DeviceAdded {
                    device_id,
                    vendor_id,
                    product_id,
                    descriptor,
                })) => {
                    println!(
                        "DeviceAdded id={device_id} vid={vendor_id:#06x} pid={product_id:#06x} \
                         descriptor_len={}",
                        descriptor.len()
                    );
                    assert!(
                        !descriptor.is_empty(),
                        "the host rebuilds the device from this descriptor via /dev/uhid, \
                         so an empty one would produce a virtual device with no reports"
                    );
                    added.push((vendor_id, product_id));
                }
                Ok(Some(HidEvent::Error {
                    device_id,
                    reason_class,
                })) => {
                    println!("Error device_id={device_id:?} reason_class={reason_class}");
                    errors.push(reason_class);
                }
                Ok(Some(other)) => println!("{other:?}"),
                Ok(None) => break,
                Err(_) => break,
            }
        }

        drop(session);

        assert!(
            errors.is_empty(),
            "capture reported {errors:?}; 'exclusive_access' means another process holds the \
             tablet right now (restart the vendor driver or replug), 'permission_denied' or \
             'not_permitted' means Input Monitoring is not granted to the test binary"
        );
        assert!(
            !added.is_empty(),
            "no tablet was captured within {ENUMERATION_TIMEOUT:?}; either none is attached, \
             or its usage page is not in MATCHED_USAGE_PAGES"
        );
    });
}
