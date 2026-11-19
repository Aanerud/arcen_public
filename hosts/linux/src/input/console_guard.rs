// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Keeps a session's uinput keyboard away from the kernel's console.
//!
//! A uinput keyboard is an ordinary kernel keyboard: the kernel's console
//! keyboard handler reads it like any other and hands its keys to the
//! foreground virtual terminal. While a display server owns that terminal
//! it is in keyboard-off mode (`K_OFF`: Xorg sets it, logind sets it for a
//! Wayland compositor) and the kernel acts on nothing. When the foreground
//! terminal is a text console instead (someone at the desk switched to one,
//! or no display server holds it), the kernel would act on the Deck's keys:
//! Ctrl+Alt+Del is the console keymap's `Boot` (systemd's
//! `ctrl-alt-del.target`, a reboot), Alt+F*n* switches consoles, and
//! anything typed goes to that console's login prompt.
//!
//! So key input reaches the session's keyboard only while the foreground
//! terminal's keyboard is off, or the host has no virtual terminals at all.
//! Every key message is gated, releases included: a release carries the
//! Deck's modifier bits and lock states, from which the Pier presses
//! Control, Alt or a lock key (Control+Scroll Lock is the console's
//! `Show_State`). When refusing starts, every held key is let go once, so
//! no modifier stays held in the console's shift state. `SysRq` is separate:
//! the kernel's SysRq filter reads every keyboard whatever the terminal, so
//! that key is never sent at all (`keymap.rs::kernel_safe_key`).

/// The foreground virtual terminal's keyboard mode, as `KDGKBMODE` reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleKeyboard {
    /// `K_OFF`: a display server owns the terminal; the kernel reads no
    /// keyboard.
    Off,
    /// A text console (`K_XLATE`, `K_UNICODE`) or a program reading raw
    /// keys (`K_RAW`, `K_MEDIUMRAW`): the kernel passes every keyboard's
    /// keys to it and acts on its key bindings. The mode.
    Reads(i32),
    /// The host has no virtual terminals.
    NoConsole,
    /// The mode could not be read.
    Unknown,
}

/// `K_OFF`.
const K_OFF: i32 = 4;

impl ConsoleKeyboard {
    /// From a `KDGKBMODE` result.
    #[must_use]
    pub const fn from_mode(mode: i32) -> Self {
        if mode == K_OFF {
            Self::Off
        } else {
            Self::Reads(mode)
        }
    }

    /// Whether key input may reach the session's keyboard. A mode that
    /// cannot be read refuses: the Pier runs as root and reads it on every
    /// host with virtual terminals.
    #[must_use]
    pub const fn admits_keys(self) -> bool {
        matches!(self, Self::Off | Self::NoConsole)
    }
}

/// Reads the foreground virtual terminal's keyboard mode (`/dev/tty0` is
/// the foreground terminal).
#[cfg(target_os = "linux")]
#[must_use]
pub fn console_keyboard() -> ConsoleKeyboard {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    // `KDGKBMODE`.
    nix::ioctl_read_bad!(kdgkbmode, 0x4B44, nix::libc::c_int);

    let console = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOCTTY | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open("/dev/tty0")
    {
        Ok(console) => console,
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(nix::libc::ENOENT | nix::libc::ENODEV | nix::libc::ENXIO)
            ) =>
        {
            return ConsoleKeyboard::NoConsole;
        }
        Err(_) => return ConsoleKeyboard::Unknown,
    };
    let mut mode: nix::libc::c_int = 0;
    // SAFETY: `console` is an open terminal for the duration of the call,
    // and `KDGKBMODE` writes one `int` to the pointer it is given.
    match unsafe { kdgkbmode(console.as_raw_fd(), &mut mode) } {
        Ok(_) => ConsoleKeyboard::from_mode(mode),
        Err(_) => ConsoleKeyboard::Unknown,
    }
}

/// Admits or refuses key input by the console's mode, and says so once per
/// change.
#[derive(Debug, Default)]
pub struct ConsoleGuard {
    refusing: bool,
}

impl ConsoleGuard {
    /// Whether key input may be injected now.
    #[cfg(target_os = "linux")]
    pub fn admit(&mut self) -> bool {
        self.admit_with(console_keyboard())
    }

    /// [`Self::admit`] for a mode already read.
    pub fn admit_with(&mut self, console: ConsoleKeyboard) -> bool {
        let admitted = console.admits_keys();
        if admitted == self.refusing {
            if admitted {
                tracing::info!(target: "input", "key input resumed: a display server owns the console again");
            } else {
                tracing::warn!(
                    target: "input",
                    ?console,
                    "key input refused: the foreground virtual terminal reads keyboards"
                );
            }
        }
        self.refusing = !admitted;
        admitted
    }

    /// Whether presses are being refused.
    #[must_use]
    pub const fn refusing(&self) -> bool {
        self.refusing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_keyboard_off_console_admits_keys() {
        assert!(ConsoleKeyboard::from_mode(4).admits_keys());
        // K_RAW, K_XLATE, K_MEDIUMRAW, K_UNICODE.
        for mode in 0..=3 {
            assert!(!ConsoleKeyboard::from_mode(mode).admits_keys(), "{mode}");
        }
        assert!(ConsoleKeyboard::NoConsole.admits_keys());
        assert!(!ConsoleKeyboard::Unknown.admits_keys());
    }

    #[test]
    fn the_guard_follows_the_console_both_ways() {
        let mut guard = ConsoleGuard::default();
        assert!(guard.admit_with(ConsoleKeyboard::Off));
        assert!(!guard.refusing());
        assert!(!guard.admit_with(ConsoleKeyboard::Reads(3)));
        assert!(guard.refusing());
        assert!(!guard.admit_with(ConsoleKeyboard::Unknown));
        assert!(guard.admit_with(ConsoleKeyboard::Off));
        assert!(!guard.refusing());
    }
}
