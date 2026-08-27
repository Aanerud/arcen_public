//! Bounded Deck clipboard state and the UI-thread AppKit adapter.

#[cfg(target_os = "macos")]
use arcen_media::clipboard::{
    validate_png, ClipboardFlow, ClipboardKind, EchoMarker, ImageLimits, MAX_DECODED_IMAGE_BYTES,
    MAX_IMAGE_DIMENSION,
};
use arcen_media::clipboard::{
    ClipboardContent, ClipboardDirection, ClipboardPolicy, EchoSuppressor, EchoToken,
};
use arcen_protocol::messages::{
    ClipboardContentKind, ClipboardContentMsg, ClipboardDirectionMsg, ClipboardPolicyMsg,
};
use std::fmt::{Debug, Formatter};
use std::sync::{Arc, Mutex};
#[cfg(target_os = "macos")]
use std::time::Duration;
use std::time::Instant;
use tokio::sync::Notify;
use zeroize::Zeroize;

#[cfg(target_os = "macos")]
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// One owned normalized clipboard payload.
pub struct ClipboardItem {
    pub sequence: u64,
    pub kind: ClipboardContentKind,
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

impl ClipboardItem {
    #[must_use]
    pub fn new(
        sequence: u64,
        kind: ClipboardContentKind,
        bytes: Vec<u8>,
        truncated: bool,
    ) -> Option<Self> {
        if sequence == 0
            || bytes.is_empty()
            || u32::try_from(bytes.len()).is_err()
            || (truncated && kind != ClipboardContentKind::TextUtf8)
        {
            return None;
        }
        Some(Self {
            sequence,
            kind,
            bytes,
            truncated,
        })
    }

    fn scrub(&mut self) {
        self.bytes.zeroize();
        self.bytes.clear();
    }
}

impl Debug for ClipboardItem {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClipboardItem")
            .field("sequence", &self.sequence)
            .field("kind", &self.kind)
            .field("bytes", &self.bytes.len())
            .field("truncated", &self.truncated)
            .finish()
    }
}

impl Drop for ClipboardItem {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

#[derive(Default)]
struct ClipboardSlots {
    policy: Option<ClipboardPolicyMsg>,
    inbound: Option<ClipboardItem>,
    outbound: Option<ClipboardItem>,
}

impl Drop for ClipboardSlots {
    fn drop(&mut self) {
        clear_item(&mut self.inbound);
        clear_item(&mut self.outbound);
    }
}

struct ClipboardSessionInner {
    slots: Mutex<ClipboardSlots>,
    outbound_ready: Notify,
}

/// Capacity-one inbound and outbound clipboard mailboxes for one transport generation.
#[derive(Clone)]
pub struct ClipboardSession {
    inner: Arc<ClipboardSessionInner>,
}

impl ClipboardSession {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ClipboardSessionInner {
                slots: Mutex::new(ClipboardSlots::default()),
                outbound_ready: Notify::new(),
            }),
        }
    }

    pub fn set_policy(&self, policy: Option<ClipboardPolicyMsg>) {
        let mut slots = self.lock_slots();
        slots.policy = policy;
        if policy.is_none() {
            clear_item(&mut slots.inbound);
            clear_item(&mut slots.outbound);
        }
    }

    #[must_use]
    pub fn policy(&self) -> Option<ClipboardPolicyMsg> {
        self.lock_slots().policy
    }

    /// Replaces the pending outbound item only when the sequence is newer.
    pub fn queue_outbound(&self, mut item: ClipboardItem) -> bool {
        let mut slots = self.lock_slots();
        if slots.policy.is_none()
            || slots
                .outbound
                .as_ref()
                .is_some_and(|pending| pending.sequence >= item.sequence)
        {
            item.scrub();
            return false;
        }
        clear_item(&mut slots.outbound);
        slots.outbound = Some(item);
        drop(slots);
        self.inner.outbound_ready.notify_one();
        true
    }

    pub(crate) fn take_outbound(&self) -> Option<ClipboardItem> {
        self.lock_slots().outbound.take()
    }

    pub(crate) async fn outbound_notified(&self) {
        self.inner.outbound_ready.notified().await;
    }

    /// Replaces the completed inbound item only when the sequence is newer.
    pub(crate) fn queue_inbound(&self, mut item: ClipboardItem) -> bool {
        let mut slots = self.lock_slots();
        if slots.policy.is_none()
            || slots
                .inbound
                .as_ref()
                .is_some_and(|pending| pending.sequence >= item.sequence)
        {
            item.scrub();
            return false;
        }
        clear_item(&mut slots.inbound);
        slots.inbound = Some(item);
        true
    }

    pub fn take_inbound(&self) -> Option<ClipboardItem> {
        self.lock_slots().inbound.take()
    }

    pub fn clear(&self) {
        let mut slots = self.lock_slots();
        slots.policy = None;
        clear_item(&mut slots.inbound);
        clear_item(&mut slots.outbound);
        drop(slots);
        self.inner.outbound_ready.notify_one();
    }

    fn lock_slots(&self) -> std::sync::MutexGuard<'_, ClipboardSlots> {
        self.inner
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Default for ClipboardSession {
    fn default() -> Self {
        Self::new()
    }
}

impl Debug for ClipboardSession {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let slots = self.lock_slots();
        formatter
            .debug_struct("ClipboardSession")
            .field("policy", &slots.policy)
            .field("inbound", &slots.inbound.as_ref().map(|item| item.sequence))
            .field(
                "outbound",
                &slots.outbound.as_ref().map(|item| item.sequence),
            )
            .finish()
    }
}

fn clear_item(slot: &mut Option<ClipboardItem>) {
    if let Some(mut item) = slot.take() {
        item.scrub();
    }
}

/// Maps a wire policy to the shared host-authoritative policy.
#[must_use]
pub fn media_policy(policy: ClipboardPolicyMsg) -> Option<ClipboardPolicy> {
    let direction = match policy.direction {
        ClipboardDirectionMsg::Both => ClipboardDirection::Both,
        ClipboardDirectionMsg::ClientToHost => ClipboardDirection::ClientToHost,
        ClipboardDirectionMsg::HostToClient => ClipboardDirection::HostToClient,
        ClipboardDirectionMsg::Disabled => ClipboardDirection::Disabled,
    };
    let content = match policy.content {
        ClipboardContentMsg::All => ClipboardContent::All,
        ClipboardContentMsg::Text => ClipboardContent::Text,
        ClipboardContentMsg::Image => ClipboardContent::Image,
    };
    ClipboardPolicy::new(direction, content, usize::try_from(policy.max_bytes).ok()?).ok()
}

#[cfg(target_os = "macos")]
fn media_kind(kind: ClipboardContentKind) -> ClipboardKind {
    match kind {
        ClipboardContentKind::TextUtf8 => ClipboardKind::TextUtf8,
        ClipboardContentKind::ImagePng => ClipboardKind::ImagePng,
    }
}

/// UI-thread controller that polls only for an enabled, negotiated transport generation.
pub struct ClipboardController {
    generation: Option<u64>,
    session: Option<ClipboardSession>,
    next_sequence: u64,
    last_poll: Option<Instant>,
    suppressor: Option<EchoSuppressor>,
    #[cfg(target_os = "macos")]
    pasteboard: Option<NativePasteboard>,
}

impl ClipboardController {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            generation: None,
            session: None,
            next_sequence: 1,
            last_poll: None,
            suppressor: None,
            #[cfg(target_os = "macos")]
            pasteboard: None,
        }
    }

    pub fn attach(&mut self, generation: u64, session: ClipboardSession) {
        if self.generation == Some(generation) {
            self.session = Some(session);
            return;
        }
        self.detach();
        let mut token = [0u8; 16];
        if getrandom::getrandom(&mut token).is_err() {
            return;
        }
        self.generation = Some(generation);
        self.session = Some(session);
        self.suppressor = Some(EchoSuppressor::new(EchoToken(token)));
    }

    pub fn detach(&mut self) {
        if let Some(session) = self.session.take() {
            session.clear();
        }
        self.generation = None;
        self.next_sequence = 1;
        self.last_poll = None;
        self.suppressor = None;
        #[cfg(target_os = "macos")]
        {
            self.pasteboard = None;
        }
    }

    /// Applies one latest inbound item and polls at most once every 250 ms.
    pub fn sync(&mut self, now: Instant) {
        #[cfg(not(target_os = "macos"))]
        let _ = now;
        let Some(session) = self.session.clone() else {
            return;
        };
        let Some(wire_policy) = session.policy() else {
            #[cfg(target_os = "macos")]
            {
                self.pasteboard = None;
            }
            return;
        };
        let Some(policy) = media_policy(wire_policy) else {
            return;
        };
        if matches!(policy.direction, ClipboardDirection::Disabled) {
            return;
        }

        #[cfg(target_os = "macos")]
        {
            let Some(_) = objc2::MainThreadMarker::new() else {
                return;
            };
            if self.pasteboard.is_none() {
                self.pasteboard = Some(NativePasteboard::new());
            }
            self.apply_inbound(&session, policy);
            if self
                .last_poll
                .is_some_and(|last| now.saturating_duration_since(last) < POLL_INTERVAL)
            {
                return;
            }
            self.last_poll = Some(now);
            self.poll_outbound(&session, policy);
        }
    }

    #[cfg(target_os = "macos")]
    fn apply_inbound(&mut self, session: &ClipboardSession, policy: ClipboardPolicy) {
        let Some(mut item) = session.take_inbound() else {
            return;
        };
        if policy
            .check_size(
                ClipboardFlow::HostToClient,
                media_kind(item.kind),
                item.bytes.len(),
            )
            .is_err()
        {
            item.scrub();
            return;
        }
        let valid = match item.kind {
            ClipboardContentKind::TextUtf8 => std::str::from_utf8(&item.bytes).is_ok(),
            ClipboardContentKind::ImagePng => validate_png(
                &item.bytes,
                ImageLimits {
                    max_encoded_bytes: policy.max_bytes,
                    ..ImageLimits::default()
                },
            )
            .is_ok(),
        };
        if !valid {
            item.scrub();
            return;
        }
        let Some(marker) = self
            .suppressor
            .as_mut()
            .and_then(|suppressor| suppressor.mark_injected(item.sequence))
        else {
            item.scrub();
            return;
        };
        if let Some(pasteboard) = self.pasteboard.as_mut() {
            pasteboard.write(marker, &item);
        }
        item.scrub();
    }

    #[cfg(target_os = "macos")]
    fn poll_outbound(&mut self, session: &ClipboardSession, policy: ClipboardPolicy) {
        if !policy.allows(ClipboardFlow::ClientToHost, ClipboardKind::TextUtf8)
            && !policy.allows(ClipboardFlow::ClientToHost, ClipboardKind::ImagePng)
        {
            return;
        }
        let Some(pasteboard) = self.pasteboard.as_mut() else {
            return;
        };
        let Some(observation) = pasteboard.poll(policy) else {
            return;
        };
        if observation.marker.is_some_and(|marker| {
            self.suppressor
                .is_some_and(|suppressor| suppressor.should_suppress(marker))
        }) {
            return;
        }
        let sequence = self.next_sequence;
        let Some(next) = sequence.checked_add(1) else {
            self.detach();
            return;
        };
        self.next_sequence = next;
        if let Some(item) = ClipboardItem::new(
            sequence,
            observation.kind,
            observation.bytes,
            observation.truncated,
        ) {
            let _ = session.queue_outbound(item);
        }
    }
}

impl Default for ClipboardController {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "macos")]
struct NativeObservation {
    marker: Option<EchoMarker>,
    kind: ClipboardContentKind,
    bytes: Vec<u8>,
    truncated: bool,
}

#[cfg(target_os = "macos")]
struct NativePasteboard {
    pasteboard: objc2::rc::Retained<objc2_app_kit::NSPasteboard>,
    origin_type: objc2::rc::Retained<objc2_foundation::NSString>,
    last_change: isize,
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

        let last_change = pasteboard.changeCount();
        Self {
            pasteboard,
            origin_type: NSString::from_str("tech.arcen.clipboard-origin"),
            last_change,
        }
    }

    fn poll(&mut self, policy: ClipboardPolicy) -> Option<NativeObservation> {
        use objc2::runtime::AnyObject;
        use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSBitmapImageRepPropertyKey};
        use objc2_foundation::NSDictionary;

        let change = self.pasteboard.changeCount();
        if change == self.last_change {
            return None;
        }
        self.last_change = change;
        let marker = self
            .pasteboard
            .dataForType(&self.origin_type)
            .and_then(|data| {
                (data.len() == arcen_media::clipboard::ECHO_MARKER_BYTES).then(|| data.to_vec())
            })
            .and_then(|bytes| EchoMarker::decode(&bytes));

        if policy.allows(ClipboardFlow::ClientToHost, ClipboardKind::TextUtf8) {
            if let Some(text) = self.pasteboard.stringForType(pasteboard_string_type()) {
                let byte_len = text.len();
                let pointer = text.UTF8String();
                if pointer.is_null() {
                    return None;
                }
                // SAFETY: NSString owns a valid immutable UTF-8 buffer for its
                // retained lifetime; `len` reports that buffer's byte length.
                let bytes = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), byte_len) };
                let text = std::str::from_utf8(bytes).ok()?;
                let prepared = policy.prepare_text(ClipboardFlow::ClientToHost, text);
                if !prepared.text.is_empty() {
                    return Some(NativeObservation {
                        marker,
                        kind: ClipboardContentKind::TextUtf8,
                        bytes: prepared.text.as_bytes().to_vec(),
                        truncated: prepared.truncated,
                    });
                }
            }
        }
        if !policy.allows(ClipboardFlow::ClientToHost, ClipboardKind::ImagePng) {
            return None;
        }
        if let Some(data) = self.pasteboard.dataForType(pasteboard_png_type()) {
            if data.is_empty() || data.len() > policy.max_bytes {
                return None;
            }
            let bytes = data.to_vec();
            if validate_png(
                &bytes,
                ImageLimits {
                    max_encoded_bytes: policy.max_bytes,
                    ..ImageLimits::default()
                },
            )
            .is_ok()
            {
                return Some(NativeObservation {
                    marker,
                    kind: ClipboardContentKind::ImagePng,
                    bytes,
                    truncated: false,
                });
            }
        }
        let tiff = self.pasteboard.dataForType(pasteboard_tiff_type())?;
        if tiff.is_empty() || tiff.len() > policy.max_bytes {
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
        let bytes = png.to_vec();
        validate_png(
            &bytes,
            ImageLimits {
                max_encoded_bytes: policy.max_bytes,
                ..ImageLimits::default()
            },
        )
        .ok()?;
        Some(NativeObservation {
            marker,
            kind: ClipboardContentKind::ImagePng,
            bytes,
            truncated: false,
        })
    }

    fn write(&mut self, marker: EchoMarker, item: &ClipboardItem) {
        use objc2_foundation::{NSData, NSString};

        self.pasteboard.clearContents();
        let marker_bytes = marker.encode();
        let marker = NSData::with_bytes(&marker_bytes);
        if !self
            .pasteboard
            .setData_forType(Some(&marker), &self.origin_type)
        {
            return;
        }
        match item.kind {
            ClipboardContentKind::TextUtf8 => {
                if let Ok(text) = std::str::from_utf8(&item.bytes) {
                    let text = NSString::from_str(text);
                    let _ = self
                        .pasteboard
                        .setString_forType(&text, pasteboard_string_type());
                }
            }
            ClipboardContentKind::ImagePng => {
                let png = NSData::with_bytes(&item.bytes);
                let _ = self
                    .pasteboard
                    .setData_forType(Some(&png), pasteboard_png_type());
            }
        }

        self.last_change = self.pasteboard.changeCount();
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
    use objc2_app_kit::NSPasteboard;

    #[test]
    fn unique_pasteboard_writes_marker_before_text_payload() {
        let pasteboard = NSPasteboard::pasteboardWithUniqueName();
        let mut native = NativePasteboard::from_pasteboard(pasteboard.clone());
        let marker = EchoMarker {
            token: EchoToken([3; 16]),
            sequence: 7,
        };
        let item = ClipboardItem::new(
            7,
            ClipboardContentKind::TextUtf8,
            b"clipboard test".to_vec(),
            false,
        )
        .expect("item");
        native.write(marker, &item);
        assert_eq!(
            pasteboard
                .dataForType(&native.origin_type)
                .and_then(|data| EchoMarker::decode(&data.to_vec())),
            Some(marker)
        );
        assert_eq!(
            pasteboard
                .stringForType(pasteboard_string_type())
                .map(|text| text.to_string())
                .as_deref(),
            Some("clipboard test")
        );
        pasteboard.clearContents();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_protocol::messages::CLIPBOARD_PROTOCOL_VERSION;

    fn policy() -> ClipboardPolicyMsg {
        ClipboardPolicyMsg {
            protocol_version: CLIPBOARD_PROTOCOL_VERSION,
            direction: ClipboardDirectionMsg::Both,
            content: ClipboardContentMsg::All,
            max_bytes: 1024,
        }
    }

    #[test]
    fn latest_slots_replace_and_disabled_state_scrubs() {
        let session = ClipboardSession::new();
        session.set_policy(Some(policy()));
        assert!(session.queue_outbound(
            ClipboardItem::new(1, ClipboardContentKind::TextUtf8, b"one".to_vec(), false).unwrap()
        ));
        assert!(!session.queue_outbound(
            ClipboardItem::new(1, ClipboardContentKind::TextUtf8, b"stale".to_vec(), false)
                .unwrap()
        ));
        assert!(session.queue_outbound(
            ClipboardItem::new(2, ClipboardContentKind::TextUtf8, b"two".to_vec(), false).unwrap()
        ));
        assert_eq!(session.take_outbound().unwrap().bytes, b"two");

        session.set_policy(None);
        assert!(!session.queue_outbound(
            ClipboardItem::new(3, ClipboardContentKind::TextUtf8, b"off".to_vec(), false).unwrap()
        ));
        assert!(session.take_outbound().is_none());
    }
}
