// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The session's keyboard layout, told to the Deck (`host_keymap`).
//!
//! Keys are injected by scan code, so the layout of the window that has
//! the keyboard decides each character, exactly as for a keyboard plugged
//! into this host. A Deck that types committed text compares its own
//! layout with this one and sends as text what this host would type
//! differently (`æ` from a Norwegian Deck to a US host). Without the notice
//! it assumes the layouts match and sends every key by position.
//!
//! What is read here, and nothing else: the foreground window's thread's
//! keyboard layout (`GetKeyboardLayout`; the user's default input language
//! while no window has the keyboard), its identifier, the layouts the user
//! has installed (`GetKeyboardLayoutList`), and what that layout types at
//! each shared printable position with and without Shift (`ToUnicodeEx`
//! on the position's scan code, keyboard state untouched). The positions,
//! the validation of every character, the layout names and when a notice
//! is due are the shared [`arcen_input::text::host_layout`] contract the
//! Linux Pier uses as well. Read every second in a dedicated OS thread,
//! like [`crate::cursor_watcher`]; the thread exits with the session.

use tokio::sync::watch;

/// How often the foreground layout is read.
#[cfg(windows)]
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Starts the watcher. The receiver holds the newest `host_keymap` notice
/// (JSON) not yet sent; the thread exits when the receiver is dropped.
pub fn spawn() -> Option<watch::Receiver<Option<String>>> {
    let (tx, rx) = watch::channel(None);
    std::thread::Builder::new()
        .name("arcen-keymap-watcher".to_string())
        .spawn(move || run_watcher(&tx))
        .ok()?;
    Some(rx)
}

#[cfg(windows)]
fn run_watcher(tx: &watch::Sender<Option<String>>) {
    let mut notices = arcen_input::text::host_layout::HostKeymapNotices::default();
    let mut names = imp::LayoutNames::default();
    while !tx.is_closed() {
        if let Some(notice) = imp::read_layout(&mut names).and_then(|read| notices.offer(read)) {
            if let Ok(json) = serde_json::to_string(&notice) {
                tracing::info!(
                    target: crate::logging::INPUT,
                    generation = notice.generation,
                    layouts = ?notice.layouts,
                    active_layout = ?notice.active_layout,
                    positions = notice.keys.len(),
                    "session keyboard layout read"
                );
                tx.send_replace(Some(json));
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(not(windows))]
fn run_watcher(_tx: &watch::Sender<Option<String>>) {}

#[cfg(windows)]
mod imp {
    use arcen_input::keyboard::scancode::set1_position;
    use arcen_input::text::host_layout::{
        self, parse_klid, parse_layout_id, windows_hkl_layout, WindowsHklLayout,
        WINDOWS_KEYBOARD_LAYOUT_SOURCE,
    };
    use arcen_protocol::messages::HostKeymapMsg;
    use std::collections::HashMap;
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE,
        KEY_READ, RRF_RT_REG_SZ,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetKeyboardLayout, GetKeyboardLayoutList, MapVirtualKeyExW, ToUnicodeEx, HKL,
        MAPVK_VSC_TO_VK_EX,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId, SystemParametersInfoW,
        SPI_GETDEFAULTINPUTLANG, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };

    /// `ToUnicodeEx`: leave the keyboard's dead-key state alone (Windows 10
    /// 1607 and later).
    const DO_NOT_CHANGE_STATE: u32 = 0x4;
    const VK_SHIFT: usize = 0x10;
    const VK_LSHIFT: usize = 0xA0;
    /// Most layouts a notice names.
    const MAX_LAYOUTS: usize = 16;

    /// Layout names by handle, read once per handle.
    #[derive(Default)]
    pub struct LayoutNames(HashMap<isize, String>);

    impl LayoutNames {
        fn name(&mut self, layout: HKL) -> String {
            self.0
                .entry(layout.0 as isize)
                .or_insert_with(|| layout_name(layout))
                .clone()
        }
    }

    /// The layout the next key lands on, as the Deck's notice.
    pub fn read_layout(names: &mut LayoutNames) -> Option<HostKeymapMsg> {
        let layout = keyboard_layout()?;
        let mut installed = [HKL::default(); MAX_LAYOUTS];
        // SAFETY: the buffer's length bounds what is written.
        let count = unsafe { GetKeyboardLayoutList(Some(&mut installed)) };
        let count = usize::try_from(count).unwrap_or(0).min(MAX_LAYOUTS);
        let layouts = installed[..count]
            .iter()
            .map(|installed| names.name(*installed))
            .collect();
        let active = names.name(layout);
        Some(host_layout::host_keymap_msg(
            WINDOWS_KEYBOARD_LAYOUT_SOURCE,
            layouts,
            Some(active),
            |qt_key| {
                let scan = set1_position(qt_key).filter(|scan| !scan.extended)?;
                // SAFETY: read-only layout query for a valid handle.
                let vk =
                    unsafe { MapVirtualKeyExW(u32::from(scan.code), MAPVK_VSC_TO_VK_EX, layout) };
                if vk == 0 {
                    return None;
                }
                let typed = |shift: bool| typed_text(vk, u32::from(scan.code), shift, layout);
                Some((typed(false), typed(true)))
            },
        ))
    }

    /// The foreground window's layout; the user's default input language
    /// while no window has the keyboard. Never this process's own.
    fn keyboard_layout() -> Option<HKL> {
        // SAFETY: read-only queries of the foreground window and its thread.
        unsafe {
            let window = GetForegroundWindow();
            if !window.0.is_null() {
                let thread = GetWindowThreadProcessId(window, None);
                if thread != 0 {
                    let layout = GetKeyboardLayout(thread);
                    if !layout.0.is_null() {
                        return Some(layout);
                    }
                }
            }
            let mut layout = HKL::default();
            SystemParametersInfoW(
                SPI_GETDEFAULTINPUTLANG,
                0,
                Some(std::ptr::from_mut(&mut layout).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
            .ok()?;
            (!layout.0.is_null()).then_some(layout)
        }
    }

    /// What `vk` types on `layout` with no modifier or with Shift, Caps
    /// Lock off; `None` for a dead key or no character.
    fn typed_text(vk: u32, scan: u32, shift: bool, layout: HKL) -> Option<String> {
        let mut state = [0_u8; 256];
        if shift {
            state[VK_SHIFT] = 0x80;
            state[VK_LSHIFT] = 0x80;
        }
        let mut typed = [0_u16; 8];
        // SAFETY: valid buffers; the flag keeps the kernel's keyboard state
        // (dead keys) untouched.
        let count =
            unsafe { ToUnicodeEx(vk, scan, &state, &mut typed, DO_NOT_CHANGE_STATE, layout) };
        let count = usize::try_from(count).ok().filter(|count| *count > 0)?;
        host_layout::key_text_utf16(&typed[..count.min(typed.len())])
    }

    fn layout_name(layout: HKL) -> String {
        let klid = match windows_hkl_layout(layout.0 as usize as u64) {
            WindowsHklLayout::Klid(klid) => Some(klid),
            WindowsHklLayout::Variant(id) => variant_klid(id),
        };
        klid.map_or_else(
            || format!("windows:hkl-{:08X}", layout.0 as usize as u64 & 0xFFFF_FFFF),
            host_layout::windows_layout_name,
        )
    }

    /// The KLID whose `Layout Id` is `id`, from the installed layouts.
    fn variant_klid(id: u16) -> Option<u32> {
        let mut root = HKEY::default();
        // SAFETY: a read-only open of a fixed key; closed below.
        let opened = unsafe {
            RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                w!("SYSTEM\\CurrentControlSet\\Control\\Keyboard Layouts"),
                0,
                KEY_READ,
                &mut root,
            )
        };
        if opened != ERROR_SUCCESS {
            return None;
        }
        let mut found = None;
        for index in 0..1024_u32 {
            let mut name = [0_u16; 16];
            let mut length = name.len() as u32;
            // SAFETY: the buffer and its length in characters.
            let listed = unsafe {
                RegEnumKeyExW(
                    root,
                    index,
                    windows::core::PWSTR(name.as_mut_ptr()),
                    &mut length,
                    None,
                    windows::core::PWSTR::null(),
                    None,
                    None,
                )
            };
            if listed != ERROR_SUCCESS {
                break;
            }
            let Some(klid) = parse_klid(&String::from_utf16_lossy(
                &name[..(length as usize).min(name.len())],
            )) else {
                continue;
            };
            let mut value = [0_u16; 8];
            let mut bytes = std::mem::size_of_val(&value) as u32;
            // SAFETY: `name` is NUL-terminated by RegEnumKeyExW; the value
            // buffer and its size in bytes.
            let read = unsafe {
                RegGetValueW(
                    root,
                    PCWSTR(name.as_ptr()),
                    w!("Layout Id"),
                    RRF_RT_REG_SZ,
                    None,
                    Some(value.as_mut_ptr().cast()),
                    Some(&mut bytes),
                )
            };
            if read != ERROR_SUCCESS {
                continue;
            }
            let units = (bytes as usize / 2).min(value.len());
            if parse_layout_id(&String::from_utf16_lossy(&value[..units])) == Some(id) {
                found = Some(klid);
                break;
            }
        }
        // SAFETY: opened above.
        let _ = unsafe { RegCloseKey(root) };
        found
    }
}
