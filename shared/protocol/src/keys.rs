// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The key identifiers and modifier bits a `key_event` carries.
//!
//! `KeyEventMsg::scan_code` is a Qt key identifier naming a key *position*
//! by what a US layout prints on it; the host's own layout decides the
//! character (see `WIRE.md`, "`key_event` compatibility contract"). These
//! constants are the identifiers products agree on beyond letters, digits
//! and punctuation, which are their ASCII upper-case/US character codes.

/// Shift held (compact wire `modifiers` bit).
pub const MOD_SHIFT: u32 = 0x01;
/// Control held.
pub const MOD_CTRL: u32 = 0x02;
/// Alt (Option) held. Never set for [`KEY_ALTGR`] alone.
pub const MOD_ALT: u32 = 0x04;
/// Meta held: the Windows key, Super, or a Mac host's Command.
pub const MOD_META: u32 = 0x08;
/// The key is on the numeric keypad.
pub const MOD_KEYPAD: u32 = 0x10;

/// `Qt::Key_Shift`.
pub const KEY_SHIFT: u32 = 0x0100_0020;
/// `Qt::Key_Control`.
pub const KEY_CONTROL: u32 = 0x0100_0021;
/// `Qt::Key_Meta`: the Windows key, Super, or a Mac host's Command.
pub const KEY_META: u32 = 0x0100_0022;
/// `Qt::Key_Alt`: the left Alt key (Option on a Mac host).
pub const KEY_ALT: u32 = 0x0100_0023;
/// `Qt::Key_AltGr`: the right Alt key as `AltGr` (PC set-1 `E0 38`, evdev
/// `KEY_RIGHTALT`). A host whose layout has `AltGr` types its third-level
/// characters with it; it is not the Alt modifier, so [`MOD_ALT`] is never
/// set for it. Sent only to a host that advertises
/// `input_capabilities.extended_keys = available`.
pub const KEY_ALTGR: u32 = 0x0100_1103;
/// The ISO key between left Shift and Z (`<>` on most European layouts; PC
/// set-1 `0x56`, evdev `KEY_102ND`, the W3C `IntlBackslash` code).
///
/// Qt has no identifier for this position (it reports the character), so
/// Arcen uses this unassigned value in Qt's international-key range. Sent
/// only to a host that advertises `input_capabilities.extended_keys =
/// available`; older hosts receive the key as the client always sent it.
pub const KEY_INTL_BACKSLASH: u32 = 0x0100_1104;

/// `Qt::Key_Escape`.
pub const KEY_ESCAPE: u32 = 0x0100_0000;
/// `Qt::Key_Tab`.
pub const KEY_TAB: u32 = 0x0100_0001;
/// `Qt::Key_Return`: the main Return/Enter key (the keypad's is
/// `Qt::Key_Enter` with [`MOD_KEYPAD`]).
pub const KEY_RETURN: u32 = 0x0100_0004;

/// `Qt::Key_Q`.
pub const KEY_Q: u32 = 0x51;
