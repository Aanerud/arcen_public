// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Deck's clipboard: the shared controller and the AppKit pasteboard
//! behind its seam.
//!
//! Policy, size limits, PNG validation, echo suppression, sequence numbers
//! and the 250 ms poll are `arcen_deck_core::runtime::clipboard_controller`'s,
//! shared with the Windows Deck. This module only reads and writes
//! `NSPasteboard` for it ([`arcen_deck_core::clipboard::PlatformClipboard`]).

#[cfg(target_os = "macos")]
use arcen_media::clipboard::{
    validate_png, ImageLimits, MAX_DECODED_IMAGE_BYTES, MAX_IMAGE_DIMENSION,
};
use arcen_protocol::messages::ClipboardPolicyMsg;

pub use arcen_deck_core::runtime::clipboard_controller::ClipboardController;
pub use arcen_deck_core::runtime::clipboard_session::{
    media_policy, ClipboardItem, ClipboardSession,
};

/// The general pasteboard for the shared controller, on the main thread
/// only (AppKit's rule).
#[cfg(target_os = "macos")]
#[must_use]
pub fn open_native() -> Option<Box<dyn arcen_deck_core::clipboard::PlatformClipboard>> {
    objc2::MainThreadMarker::new()?;
    Some(Box::new(NativePasteboard::new()))
}

#[cfg(target_os = "macos")]
struct NativePasteboard {
    pasteboard: objc2::rc::Retained<objc2_app_kit::NSPasteboard>,
    origin_type: objc2::rc::Retained<objc2_foundation::NSString>,
}

#[cfg(target_os = "macos")]
impl NativePasteboard {
    fn new() -> Self {
        use objc2_app_kit::NSPasteboard;

        let pasteboard = NSPasteboard::generalPasteboard();
        Self::from_pasteboard(pasteboard)
    }

    fn from_pasteboard(pasteboard: objc2::rc::Retained<objc2_app_kit::NSPasteboard>) -> Self {
        use objc2_foundation::NSString;

        Self {
            pasteboard,
            origin_type: NSString::from_str("tech.arcen.clipboard-origin"),
        }
    }

    /// The pasteboard's PNG, or its TIFF converted to PNG when it offers
    /// no valid PNG (checked here, because an invalid one falls back to the
    /// TIFF). An empty or oversize PNG refuses the image outright.
    fn png_or_converted_tiff(&self, max_encoded_bytes: usize) -> Option<Vec<u8>> {
        use objc2::runtime::AnyObject;
        use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSBitmapImageRepPropertyKey};
        use objc2_foundation::NSDictionary;

        let limits = ImageLimits {
            max_encoded_bytes,
            ..ImageLimits::default()
        };
        if let Some(data) = self.pasteboard.dataForType(pasteboard_png_type()) {
            if data.is_empty() || data.len() > max_encoded_bytes {
                return None;
            }
            let bytes = data.to_vec();
            if validate_png(&bytes, limits).is_ok() {
                return Some(bytes);
            }
        }
        let tiff = self.pasteboard.dataForType(pasteboard_tiff_type())?;
        if tiff.is_empty() || tiff.len() > max_encoded_bytes {
            return None;
        }
        let tiff_bytes = tiff.to_vec();
        let (width, height) = tiff_dimensions(&tiff_bytes)?;
        let decoded = usize::try_from(width)
            .ok()?
            .checked_mul(usize::try_from(height).ok()?)?
            .checked_mul(4)?;
        if width == 0
            || height == 0
            || width > MAX_IMAGE_DIMENSION
            || height > MAX_IMAGE_DIMENSION
            || decoded > MAX_DECODED_IMAGE_BYTES
        {
            return None;
        }
        let tiff = objc2_foundation::NSData::with_bytes(&tiff_bytes);
        let representation = NSBitmapImageRep::imageRepWithData(&tiff)?;
        if u32::try_from(representation.pixelsWide()).ok()? != width
            || u32::try_from(representation.pixelsHigh()).ok()? != height
        {
            return None;
        }
        let properties: objc2::rc::Retained<NSDictionary<NSBitmapImageRepPropertyKey, AnyObject>> =
            NSDictionary::new();
        // SAFETY: The dictionary has the exact generated key/value types and is empty.
        let png = unsafe {
            representation
                .representationUsingType_properties(NSBitmapImageFileType::PNG, &properties)
        }?;
        // The controller validates what this returns against the policy.
        Some(png.to_vec())
    }
}

#[cfg(target_os = "macos")]
impl arcen_deck_core::clipboard::PlatformClipboard for NativePasteboard {
    fn change_token(&mut self) -> u64 {
        // `changeCount` only rises; the controller compares for equality.
        self.pasteboard.changeCount() as u64
    }

    fn read_origin_marker(&mut self) -> Option<Vec<u8>> {
        self.pasteboard
            .dataForType(&self.origin_type)
            .and_then(|data| {
                (data.len() == arcen_media::clipboard::ECHO_MARKER_BYTES).then(|| data.to_vec())
            })
    }

    fn read_text(&mut self) -> Option<zeroize::Zeroizing<String>> {
        let text = self.pasteboard.stringForType(pasteboard_string_type())?;
        let byte_len = text.len();
        let pointer = text.UTF8String();
        if pointer.is_null() {
            return None;
        }
        // SAFETY: NSString owns a valid immutable UTF-8 buffer for its
        // retained lifetime; `len` reports that buffer's byte length.
        let bytes = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), byte_len) };
        let text = std::str::from_utf8(bytes).ok()?;
        Some(zeroize::Zeroizing::new(text.to_owned()))
    }

    fn read_png(&mut self, max_encoded_bytes: usize) -> Option<Vec<u8>> {
        self.png_or_converted_tiff(max_encoded_bytes)
    }

    fn write(
        &mut self,
        origin_marker: &[u8],
        kind: arcen_protocol::messages::ClipboardContentKind,
        bytes: &[u8],
    ) -> bool {
        use arcen_protocol::messages::ClipboardContentKind;
        use objc2_foundation::{NSData, NSString};

        self.pasteboard.clearContents();
        let marker = NSData::with_bytes(origin_marker);
        if !self
            .pasteboard
            .setData_forType(Some(&marker), &self.origin_type)
        {
            return false;
        }
        match kind {
            ClipboardContentKind::TextUtf8 => {
                if let Ok(text) = std::str::from_utf8(bytes) {
                    let text = NSString::from_str(text);
                    let _ = self
                        .pasteboard
                        .setString_forType(&text, pasteboard_string_type());
                }
            }
            ClipboardContentKind::ImagePng => {
                let png = NSData::with_bytes(bytes);
                let _ = self
                    .pasteboard
                    .setData_forType(Some(&png), pasteboard_png_type());
            }
        }
        true
    }
}

#[cfg(target_os = "macos")]
fn tiff_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let little_endian = match bytes.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    if tiff_u16(bytes, 2, little_endian)? != 42 {
        return None;
    }
    let directory = usize::try_from(tiff_u32(bytes, 4, little_endian)?).ok()?;
    let entries = usize::from(tiff_u16(bytes, directory, little_endian)?);
    if entries > 64 {
        return None;
    }
    let entries_start = directory.checked_add(2)?;
    let entries_bytes = entries.checked_mul(12)?;
    let next_directory = entries_start.checked_add(entries_bytes)?;
    if tiff_u32(bytes, next_directory, little_endian)? != 0 {
        return None;
    }
    let mut width = None;
    let mut height = None;
    for index in 0..entries {
        let entry = entries_start.checked_add(index.checked_mul(12)?)?;
        let tag = tiff_u16(bytes, entry, little_endian)?;
        if tag != 256 && tag != 257 {
            continue;
        }
        let field_type = tiff_u16(bytes, entry + 2, little_endian)?;
        if tiff_u32(bytes, entry + 4, little_endian)? != 1 {
            return None;
        }
        let value = match field_type {
            3 => u32::from(tiff_u16(bytes, entry + 8, little_endian)?),
            4 => tiff_u32(bytes, entry + 8, little_endian)?,
            _ => return None,
        };
        let destination = if tag == 256 { &mut width } else { &mut height };
        if destination.replace(value).is_some() {
            return None;
        }
    }
    Some((width?, height?))
}

#[cfg(target_os = "macos")]
fn tiff_u16(bytes: &[u8], offset: usize, little_endian: bool) -> Option<u16> {
    let value: [u8; 2] = bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?;
    Some(if little_endian {
        u16::from_le_bytes(value)
    } else {
        u16::from_be_bytes(value)
    })
}

#[cfg(target_os = "macos")]
fn tiff_u32(bytes: &[u8], offset: usize, little_endian: bool) -> Option<u32> {
    let value: [u8; 4] = bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?;
    Some(if little_endian {
        u32::from_le_bytes(value)
    } else {
        u32::from_be_bytes(value)
    })
}

/// Whether a host's clipboard policy lets text go from this Mac to it, which
/// Paste as Keystrokes honours too: an administrator who keeps the Mac's
/// clipboard out of a host keeps it out typed as well. A host that announced
/// no policy (an older Pier) allows it; an unreadable policy does not.
#[must_use]
pub fn policy_allows_typed_paste(policy: Option<ClipboardPolicyMsg>) -> bool {
    policy.is_none_or(|policy| {
        media_policy(policy).is_some_and(|policy| {
            policy.allows(
                arcen_media::clipboard::ClipboardFlow::ClientToHost,
                arcen_media::clipboard::ClipboardKind::TextUtf8,
            )
        })
    })
}

/// The general pasteboard's plain text, if it holds any. Wrapped so it is
/// wiped when dropped: it may be a password.
#[cfg(target_os = "macos")]
#[must_use]
pub fn read_plain_text() -> Option<zeroize::Zeroizing<String>> {
    let pasteboard = objc2_app_kit::NSPasteboard::generalPasteboard();
    let text = pasteboard.stringForType(pasteboard_string_type())?;
    Some(zeroize::Zeroizing::new(text.to_string()))
}

/// No pasteboard off macOS.
#[cfg(not(target_os = "macos"))]
#[must_use]
pub fn read_plain_text() -> Option<zeroize::Zeroizing<String>> {
    None
}

#[cfg(target_os = "macos")]
fn pasteboard_string_type() -> &'static objc2_app_kit::NSPasteboardType {
    // SAFETY: AppKit exports this immutable process-lifetime NSString constant.
    unsafe { objc2_app_kit::NSPasteboardTypeString }
}

#[cfg(target_os = "macos")]
fn pasteboard_png_type() -> &'static objc2_app_kit::NSPasteboardType {
    // SAFETY: AppKit exports this immutable process-lifetime NSString constant.
    unsafe { objc2_app_kit::NSPasteboardTypePNG }
}

#[cfg(target_os = "macos")]
fn pasteboard_tiff_type() -> &'static objc2_app_kit::NSPasteboardType {
    // SAFETY: AppKit exports this immutable process-lifetime NSString constant.
    unsafe { objc2_app_kit::NSPasteboardTypeTIFF }
}

#[cfg(all(test, target_os = "macos"))]
mod native_tests {
    use super::*;
    use arcen_deck_core::clipboard::PlatformClipboard;
    use arcen_media::clipboard::{EchoMarker, EchoToken};
    use arcen_protocol::messages::{
        ClipboardContentKind, ClipboardContentMsg, ClipboardDirectionMsg,
        CLIPBOARD_PROTOCOL_VERSION,
    };
    use objc2_app_kit::NSPasteboard;
    use std::sync::{Mutex, MutexGuard};
    use std::time::{Duration, Instant};

    /// AppKit's pasteboard is not safe to drive from two threads at once:
    /// run in parallel, two tests' `pasteboardWithUniqueName` pasteboards
    /// read each other's contents, `setData:forType:` fails, and the process
    /// can crash. The product touches `NSPasteboard` from the main thread
    /// only (`open_native`); these tests take turns.
    fn pasteboard_turn() -> MutexGuard<'static, ()> {
        static TURN: Mutex<()> = Mutex::new(());
        TURN.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn unique_pasteboard_writes_marker_before_text_payload() {
        let _turn = pasteboard_turn();
        let pasteboard = NSPasteboard::pasteboardWithUniqueName();
        let mut native = NativePasteboard::from_pasteboard(pasteboard.clone());
        let marker = EchoMarker {
            token: EchoToken([3; 16]),
            sequence: 7,
        };
        assert!(native.write(
            &marker.encode(),
            ClipboardContentKind::TextUtf8,
            b"clipboard test"
        ));
        assert_eq!(
            native
                .read_origin_marker()
                .and_then(|bytes| EchoMarker::decode(&bytes)),
            Some(marker)
        );
        assert_eq!(
            native.read_text().as_deref().map(String::as_str),
            Some("clipboard test")
        );
        pasteboard.clearContents();
    }

    /// The shared controller over a real pasteboard: a host item lands with
    /// the marker and is not sent back; a local copy afterwards is sent.
    #[test]
    fn the_shared_controller_drives_a_real_pasteboard_both_ways() {
        let _turn = pasteboard_turn();
        let pasteboard = NSPasteboard::pasteboardWithUniqueName();
        let open = || {
            Some(
                Box::new(NativePasteboard::from_pasteboard(pasteboard.clone()))
                    as Box<dyn PlatformClipboard>,
            )
        };
        let session = ClipboardSession::new();
        session.set_policy(Some(ClipboardPolicyMsg {
            protocol_version: CLIPBOARD_PROTOCOL_VERSION,
            direction: ClipboardDirectionMsg::Both,
            content: ClipboardContentMsg::All,
            max_bytes: 4096,
        }));
        let mut controller = ClipboardController::new();
        controller.attach(1, session.clone());
        let start = Instant::now();
        controller.sync_with(start, open);

        let inbound = ClipboardItem::new(
            1,
            ClipboardContentKind::TextUtf8,
            b"from host".to_vec(),
            false,
        )
        .expect("item");
        assert!(session.queue_inbound(inbound));
        controller.sync_with(start + Duration::from_secs(1), open);
        assert_eq!(
            pasteboard
                .stringForType(pasteboard_string_type())
                .map(|text| text.to_string())
                .as_deref(),
            Some("from host")
        );
        controller.sync_with(start + Duration::from_secs(2), open);
        assert!(session.take_outbound().is_none(), "no echo");

        pasteboard.clearContents();
        pasteboard.setString_forType(
            &objc2_foundation::NSString::from_str("from deck"),
            pasteboard_string_type(),
        );
        controller.sync_with(start + Duration::from_secs(3), open);
        let outbound = session.take_outbound().expect("local copy sent");
        assert_eq!(outbound.bytes, b"from deck");
        controller.detach();
        pasteboard.clearContents();
    }
}
