// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

#![allow(unsafe_code)]

//! The session's keyboard layout, told to the Deck (`host_keymap`).
//!
//! Keys are posted by virtual key code, so the current keyboard input
//! source decides each character, exactly as for a keyboard plugged into
//! this Mac. A Deck that types committed text compares its own layout with
//! this one and sends as text what this host would type differently (`æ`
//! from a Norwegian Deck to a US host); without the notice it assumes the
//! layouts match and sends every key by position.
//!
//! What is read here, and nothing else: the current keyboard layout input
//! source (`TISCopyCurrentKeyboardLayoutInputSource`), its identifier, the
//! enabled keyboard layouts, and what the layout types at each shared
//! printable position with and without Shift (`UCKeyTranslate` on the
//! position's virtual key code; a dead key types nothing). Positions,
//! validation, layout names and when a notice is due are the shared
//! [`arcen_input::text::host_layout`] contract every Pier uses.
//!
//! Text Input Sources must be called on a main thread: off it, HIToolbox
//! may stop the process (`dispatch_assert_queue`), and the agent's main
//! thread belongs to its runtime. So the session runs `keymap-watch`, a
//! child of this executable whose main thread observes the input source
//! (the distributed `kTISNotifySelectedKeyboardInputSourceChanged`
//! notification, and a read every second) and writes one notice per change
//! as a JSON line. The child exits when its standard input closes, so it
//! never outlives the session that started it.

use std::ffi::c_void;
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::{Child, Command, ExitCode, Stdio};

use arcen_input::text::host_layout::{self, HostKeymapNotices, MACOS_INPUT_SOURCE};
use arcen_protocol::messages::{HOST_KEYMAP, HostKeymapMsg, MAX_HOST_KEYMAP_KEYS};

/// The subcommand that runs the watcher.
pub const KEYMAP_WATCH_COMMAND: &str = "keymap-watch";
/// Longest notice line the session accepts from the watcher.
const MAX_NOTICE_LINE: usize = 64 * 1024;
/// How often the watcher reads the input source without a notification.
const READ_INTERVAL_SECONDS: f64 = 1.0;
/// Most enabled layouts a notice names.
const MAX_LAYOUTS: usize = 16;

type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFArrayRef = *const c_void;
type CFDataRef = *const c_void;
type CFDictionaryRef = *const c_void;
type TISInputSourceRef = *const c_void;

/// `kUCKeyActionDown`.
const UC_KEY_ACTION_DOWN: u16 = 0;
/// `shiftKey >> 8`, the modifier state `UCKeyTranslate` takes.
const UC_SHIFT: u32 = 0x02;
/// `kCFStringEncodingUTF8`.
const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
/// `CFNotificationSuspensionBehaviorDeliverImmediately`.
const CF_NOTIFICATION_DELIVER_IMMEDIATELY: isize = 4;

type CFNotificationCallback = unsafe extern "C" fn(
    center: *const c_void,
    observer: *const c_void,
    name: CFStringRef,
    object: *const c_void,
    user_info: CFDictionaryRef,
);

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    static kTISPropertyInputSourceID: CFStringRef;
    static kTISPropertyUnicodeKeyLayoutData: CFStringRef;
    static kTISPropertyInputSourceType: CFStringRef;
    static kTISTypeKeyboardLayout: CFStringRef;
    static kTISNotifySelectedKeyboardInputSourceChanged: CFStringRef;
    fn TISCopyCurrentKeyboardLayoutInputSource() -> TISInputSourceRef;
    fn TISCreateInputSourceList(properties: CFDictionaryRef, include_all: u8) -> CFArrayRef;
    fn TISGetInputSourceProperty(source: TISInputSourceRef, key: CFStringRef) -> *const c_void;
    fn LMGetKbdType() -> u8;
    fn UCKeyTranslate(
        layout: *const c_void,
        virtual_key_code: u16,
        key_action: u16,
        modifier_key_state: u32,
        keyboard_type: u32,
        options: u32,
        dead_key_state: *mut u32,
        max_length: usize,
        actual_length: *mut usize,
        unicode: *mut u16,
    ) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    static kCFRunLoopDefaultMode: CFStringRef;
    fn CFDataGetBytePtr(data: CFDataRef) -> *const u8;
    fn CFDataGetLength(data: CFDataRef) -> isize;
    fn CFArrayGetCount(array: CFArrayRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFArrayRef, index: isize) -> *const c_void;
    fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *mut *const c_void,
        values: *mut *const c_void,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> *const c_void;
    fn CFStringGetCString(
        string: *const c_void,
        buffer: *mut u8,
        size: isize,
        encoding: u32,
    ) -> bool;
    fn CFNotificationCenterGetDistributedCenter() -> *const c_void;
    fn CFNotificationCenterAddObserver(
        center: *const c_void,
        observer: *const c_void,
        callback: CFNotificationCallback,
        name: CFStringRef,
        object: *const c_void,
        suspension_behavior: isize,
    );
    fn CFRunLoopRunInMode(mode: CFStringRef, seconds: f64, return_after_source: u8) -> i32;
}

/// An owned Core Foundation object, released on drop.
struct Owned(CFTypeRef);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: created by a Create/Copy call this value owns.
            unsafe { super::release_core_foundation(self.0) };
        }
    }
}

/// A Core Foundation string's UTF-8 text (identifiers are short).
fn cf_string(string: CFStringRef) -> Option<String> {
    if string.is_null() {
        return None;
    }
    let mut buffer = [0_u8; 256];
    // SAFETY: a valid CFString and a buffer of the given size.
    let copied = unsafe {
        CFStringGetCString(
            string,
            buffer.as_mut_ptr(),
            buffer.len() as isize,
            CF_STRING_ENCODING_UTF8,
        )
    };
    if !copied {
        return None;
    }
    let text = std::ffi::CStr::from_bytes_until_nul(&buffer).ok()?;
    text.to_str().ok().map(str::to_owned)
}

fn source_id(source: TISInputSourceRef) -> Option<String> {
    // SAFETY: a valid input source; the property is a CFString it owns.
    cf_string(unsafe { TISGetInputSourceProperty(source, kTISPropertyInputSourceID) })
}

/// The enabled keyboard layouts' names, in the system's order.
fn enabled_layouts() -> Vec<String> {
    // SAFETY: CF constants and a one-entry dictionary built from them.
    let filter = Owned(unsafe {
        let mut keys = [kTISPropertyInputSourceType];
        let mut values = [kTISTypeKeyboardLayout];
        CFDictionaryCreate(
            std::ptr::null(),
            keys.as_mut_ptr(),
            values.as_mut_ptr(),
            1,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        )
    });
    if filter.0.is_null() {
        return Vec::new();
    }
    // SAFETY: a valid filter dictionary; the list is owned.
    let list = Owned(unsafe { TISCreateInputSourceList(filter.0, 0) });
    if list.0.is_null() {
        return Vec::new();
    }
    // SAFETY: a valid array of input sources.
    let count = unsafe { CFArrayGetCount(list.0) }.max(0);
    (0..count)
        // SAFETY: index within the array's count; the source stays alive
        // while the list does.
        .filter_map(|index| source_id(unsafe { CFArrayGetValueAtIndex(list.0, index) }))
        .map(|id| host_layout::macos_layout_name(&id))
        .take(MAX_LAYOUTS)
        .collect()
}

/// What `virtual_key` types with no modifier or with Shift, Caps Lock off;
/// `None` for a dead key or no character.
fn typed_text(
    layout: *const c_void,
    keyboard_type: u32,
    virtual_key: u16,
    shift: bool,
) -> Option<String> {
    let mut dead_key_state = 0_u32;
    let mut length = 0_usize;
    let mut typed = [0_u16; 8];
    // SAFETY: `layout` points at the input source's `uchr` data, kept alive
    // by the caller; the buffers and their lengths are valid.
    let status = unsafe {
        UCKeyTranslate(
            layout,
            virtual_key,
            UC_KEY_ACTION_DOWN,
            if shift { UC_SHIFT } else { 0 },
            keyboard_type,
            0,
            &raw mut dead_key_state,
            typed.len(),
            &raw mut length,
            typed.as_mut_ptr(),
        )
    };
    if status != 0 || dead_key_state != 0 {
        return None;
    }
    host_layout::key_text_utf16(&typed[..length.min(typed.len())])
}

/// The current keyboard layout as the Deck's notice (generation 0).
/// Call on a main thread only (see the module documentation).
#[must_use]
pub fn read_layout() -> Option<HostKeymapMsg> {
    // SAFETY: returns an owned (copied) input source or null.
    let source = Owned(unsafe { TISCopyCurrentKeyboardLayoutInputSource() });
    if source.0.is_null() {
        return None;
    }
    // SAFETY: a valid input source; the data belongs to it.
    let data = unsafe { TISGetInputSourceProperty(source.0, kTISPropertyUnicodeKeyLayoutData) };
    // SAFETY: `data` is a CFData when non-null.
    if data.is_null() || unsafe { CFDataGetLength(data) } <= 0 {
        return None;
    }
    // SAFETY: as above; the bytes live as long as `source`.
    let layout = unsafe { CFDataGetBytePtr(data) }.cast::<c_void>();
    if layout.is_null() {
        return None;
    }
    // SAFETY: a plain query of the current keyboard type.
    let keyboard_type = u32::from(unsafe { LMGetKbdType() });
    let active = source_id(source.0).map(|id| host_layout::macos_layout_name(&id));
    let mut layouts = enabled_layouts();
    if let Some(active) = active.as_ref()
        && !layouts.contains(active)
    {
        layouts.insert(0, active.clone());
    }
    Some(host_layout::host_keymap_msg(
        MACOS_INPUT_SOURCE,
        layouts,
        active,
        |qt_key| {
            let virtual_key = super::keymap::qt_key_to_macos(qt_key, 0)?;
            Some((
                typed_text(layout, keyboard_type, virtual_key, false),
                typed_text(layout, keyboard_type, virtual_key, true),
            ))
        },
    ))
}

unsafe extern "C" fn input_source_changed(
    _center: *const c_void,
    _observer: *const c_void,
    _name: CFStringRef,
    _object: *const c_void,
    _user_info: CFDictionaryRef,
) {
    // Waking the run loop is the whole job: the loop reads the layout.
}

/// The `keymap-watch` child: on this process's main thread, writes the
/// current layout's notice, then one per change, until standard input
/// closes.
#[must_use]
pub fn run_keymap_watch() -> ExitCode {
    std::thread::spawn(|| {
        // Nothing is ever read: end of input means the session is gone.
        let mut sink = Vec::new();
        let _ = std::io::Read::read_to_end(&mut std::io::stdin().lock(), &mut sink);
        std::process::exit(0);
    });
    // SAFETY: the distributed center and a callback with the documented
    // signature; this thread runs the run loop that delivers it.
    unsafe {
        CFNotificationCenterAddObserver(
            CFNotificationCenterGetDistributedCenter(),
            std::ptr::null(),
            input_source_changed,
            kTISNotifySelectedKeyboardInputSourceChanged,
            std::ptr::null(),
            CF_NOTIFICATION_DELIVER_IMMEDIATELY,
        );
    }
    let mut notices = HostKeymapNotices::default();
    let mut out = std::io::stdout().lock();
    loop {
        if let Some(notice) = read_layout().and_then(|read| notices.offer(read))
            && let Ok(json) = serde_json::to_string(&notice)
            && writeln!(out, "{json}").and_then(|()| out.flush()).is_err()
        {
            return ExitCode::SUCCESS;
        }
        // SAFETY: runs this thread's run loop for at most the interval,
        // returning early when the notification arrives.
        unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, READ_INTERVAL_SECONDS, 1) };
    }
}

/// One notice line from the watcher, checked before it goes to the Deck:
/// a `host_keymap` within the wire's bounds, re-encoded.
#[must_use]
pub fn checked_notice(line: &str) -> Option<String> {
    let notice: HostKeymapMsg = serde_json::from_str(line).ok()?;
    if notice.msg_type != HOST_KEYMAP || notice.keys.len() > MAX_HOST_KEYMAP_KEYS {
        return None;
    }
    serde_json::to_string(&notice).ok()
}

/// The session's watcher: the child and the notices it wrote.
pub struct KeymapWatch {
    child: Child,
    notices: tokio::sync::mpsc::Receiver<String>,
}

impl std::fmt::Debug for KeymapWatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("KeymapWatch")
            .field("child", &self.child.id())
            .finish_non_exhaustive()
    }
}

impl KeymapWatch {
    /// Starts `program keymap-watch`.
    ///
    /// # Errors
    ///
    /// When the child cannot be started.
    pub fn start(program: &Path) -> std::io::Result<Self> {
        let mut child = Command::new(program)
            .arg(KEYMAP_WATCH_COMMAND)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("watcher has no output"))?;
        let (sender, notices) = tokio::sync::mpsc::channel(4);
        std::thread::Builder::new()
            .name("arcen-keymap-watch".to_owned())
            .spawn(move || {
                let mut reader = std::io::BufReader::new(stdout);
                let mut line = Vec::new();
                loop {
                    line.clear();
                    match std::io::Read::take(&mut reader, MAX_NOTICE_LINE as u64)
                        .read_until(b'\n', &mut line)
                    {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {}
                    }
                    let Some(notice) = std::str::from_utf8(&line).ok().and_then(checked_notice)
                    else {
                        continue;
                    };
                    if sender.blocking_send(notice).is_err() {
                        return;
                    }
                }
            })?;
        Ok(Self { child, notices })
    }

    /// The next notice for the Deck; `None` once the watcher stopped.
    pub async fn next(&mut self) -> Option<String> {
        self.notices.recv().await
    }
}

impl Drop for KeymapWatch {
    fn drop(&mut self) {
        // Closing its input ends the child; reap it so no zombie remains.
        drop(self.child.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_bounded_layout_notices_reach_the_deck() {
        let notice = host_layout::host_keymap_msg(
            MACOS_INPUT_SOURCE,
            vec!["no(mac)".into()],
            Some("no(mac)".into()),
            |qt_key| (qt_key == 0x27).then(|| (Some("æ".into()), Some("Æ".into()))),
        );
        let line = serde_json::to_string(&notice).unwrap();
        let checked = checked_notice(&line).expect("a notice passes");
        assert_eq!(
            serde_json::from_str::<HostKeymapMsg>(&checked).unwrap(),
            notice
        );
        assert!(checked_notice(r#"{"type":"text_commit","text":"x"}"#).is_none());
        assert!(checked_notice("not json").is_none());
        let mut huge = notice;
        huge.keys = vec![huge.keys[0].clone(); MAX_HOST_KEYMAP_KEYS + 1];
        assert!(checked_notice(&serde_json::to_string(&huge).unwrap()).is_none());
    }

    /// Reads this Mac's own current layout on the test's main thread...
    /// which a test harness does not provide; so through the real child.
    #[test]
    #[ignore = "starts the keymap-watch child of a built arcen-pier-macos (ARCEN_KEYMAP_WATCH_PROGRAM)"]
    fn the_watcher_reports_this_macs_layout() {
        let program = std::env::var_os("ARCEN_KEYMAP_WATCH_PROGRAM").expect("program");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let notice = runtime.block_on(async {
            let mut watch = KeymapWatch::start(Path::new(&program)).unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), watch.next())
                .await
                .unwrap()
                .unwrap()
        });
        let notice: HostKeymapMsg = serde_json::from_str(&notice).unwrap();
        assert_eq!(notice.generation, 1);
        assert_eq!(notice.source, MACOS_INPUT_SOURCE);
        assert!(notice.keys.len() > 40, "{notice:?}");
        println!("{notice:?}");
    }
}
