// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The window menu bar (PARITY §23 Q-1): the shared command model
//! (`arcen_deck_core::screens::menu`) as a Win32 menu, each item showing its
//! chord the PC way. A click queues the item's [`MenuCommand`] and wakes the
//! UI, which applies it on its own pass, like the Mac's `NSMenu` target.

use std::sync::{Mutex, OnceLock};

use arcen_deck_core::screens::menu::{MenuCommand, MenuEntry, deck_menus};
use arcen_deck_core::screens::wording::{DeckPlatform, chord};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateMenu, CreatePopupMenu, DrawMenuBar, EnableMenuItem, HMENU, MF_BYCOMMAND,
    MF_ENABLED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, SetMenu, WM_COMMAND,
};
use windows::core::HSTRING;

/// Menu item ids start here (id = `FIRST_ID` + index into the command list).
const FIRST_ID: usize = 0x4100;
const SUBCLASS_ID: usize = 0x4152_4d42; // "ARMB"

struct Installed {
    commands: Vec<MenuCommand>,
    wake: Box<dyn Fn() + Send + Sync>,
}

static INSTALLED: OnceLock<Installed> = OnceLock::new();
static QUEUE: Mutex<Vec<MenuCommand>> = Mutex::new(Vec::new());

/// The window's menu bar.
#[derive(Debug)]
pub(super) struct MenuBar {
    hwnd: HWND,
    menu: HMENU,
    session_active: Option<bool>,
}

fn wide(text: &str) -> HSTRING {
    HSTRING::from(text)
}

impl MenuBar {
    /// Builds the menus on `hwnd` and starts routing their clicks. `wake`
    /// is called (from the window's thread) after a click is queued.
    pub(super) fn install(
        hwnd: HWND,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let mut commands = Vec::new();
        // SAFETY: plain Win32 menu construction on the window's thread; the
        // menu handles are owned by the window once `SetMenu` succeeds.
        let menu = unsafe {
            let bar = CreateMenu().map_err(|error| format!("CreateMenu: {error}"))?;
            for menu in deck_menus(DeckPlatform::Pc, false) {
                let popup =
                    CreatePopupMenu().map_err(|error| format!("CreatePopupMenu: {error}"))?;
                for entry in &menu.entries {
                    match entry {
                        MenuEntry::Separator => {
                            AppendMenuW(popup, MF_SEPARATOR, 0, None)
                                .map_err(|error| format!("AppendMenuW: {error}"))?;
                        }
                        MenuEntry::Command(command) => {
                            let mut text = command.title(DeckPlatform::Pc).to_owned();
                            if let Some(shortcut) = command.shortcut() {
                                let keys = chord(DeckPlatform::Pc, shortcut);
                                if !keys.is_empty() {
                                    text.push('\t');
                                    text.push_str(&keys);
                                }
                            }
                            let id = FIRST_ID + commands.len();
                            commands.push(*command);
                            AppendMenuW(popup, MF_STRING, id, &wide(&text))
                                .map_err(|error| format!("AppendMenuW: {error}"))?;
                        }
                    }
                }
                AppendMenuW(bar, MF_POPUP, popup.0 as usize, &wide(menu.title))
                    .map_err(|error| format!("AppendMenuW: {error}"))?;
            }
            SetMenu(hwnd, bar).map_err(|error| format!("SetMenu: {error}"))?;
            bar
        };
        INSTALLED
            .set(Installed {
                commands,
                wake: Box::new(wake),
            })
            .map_err(|_| "the menu bar is already installed".to_owned())?;
        // SAFETY: `menu_proc` only reads the static command list and the
        // queue; the subclass lives as long as the window.
        if !unsafe { SetWindowSubclass(hwnd, Some(menu_proc), SUBCLASS_ID, 0) }.as_bool() {
            return Err("SetWindowSubclass(menu) failed".to_owned());
        }
        Ok(Self {
            hwnd,
            menu,
            session_active: None,
        })
    }

    /// Shows the bar (windowed) or takes it off the window (fullscreen,
    /// where the connection bar lists the same commands).
    pub(super) fn set_shown(&self, shown: bool) {
        // SAFETY: the menu belongs to this window, on its thread; taking it
        // off does not destroy it.
        let _ = unsafe { SetMenu(self.hwnd, if shown { self.menu } else { HMENU::default() }) };
    }

    /// Greys out what means nothing without a session.
    pub(super) fn set_session_active(&mut self, active: bool) {
        if self.session_active == Some(active) {
            return;
        }
        self.session_active = Some(active);
        let Some(installed) = INSTALLED.get() else {
            return;
        };
        for (index, command) in installed.commands.iter().enumerate() {
            // The Windows Deck has no Colour check yet.
            let enabled = !matches!(command, MenuCommand::ToggleColourCheck)
                && (active || command.available_without_session());
            let flags = MF_BYCOMMAND | if enabled { MF_ENABLED } else { MF_GRAYED };
            // SAFETY: the menu belongs to this window, on its thread.
            unsafe {
                let _ = EnableMenuItem(
                    self.menu,
                    u32::try_from(FIRST_ID + index).unwrap_or(0),
                    flags,
                );
            }
        }
        // SAFETY: as above.
        let _ = unsafe { DrawMenuBar(self.hwnd) };
    }
}

/// The commands clicked since the last call.
pub(super) fn drain() -> Vec<MenuCommand> {
    QUEUE
        .lock()
        .map(|mut queue| std::mem::take(&mut *queue))
        .unwrap_or_default()
}

unsafe extern "system" fn menu_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass: usize,
    _data: usize,
) -> LRESULT {
    // A menu's WM_COMMAND has 0 in the high word of wParam.
    if message == WM_COMMAND && wparam.0 & 0xffff_0000 == 0 {
        let id = wparam.0 & 0xffff;
        if let Some(installed) = INSTALLED.get()
            && let Some(command) = id
                .checked_sub(FIRST_ID)
                .and_then(|index| installed.commands.get(index))
        {
            let command = match command {
                MenuCommand::Quit { .. } => MenuCommand::Quit {
                    from_keyboard: false,
                },
                other => *other,
            };
            if let Ok(mut queue) = QUEUE.lock() {
                queue.push(command);
            }
            (installed.wake)();
            return LRESULT(0);
        }
    }
    // SAFETY: forwarding the message unchanged down the subclass chain.
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}
