// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Key messages, committed text and the input method.
//!
//! A key reaches here either as a window message (the system saw it) or
//! from the low-level hook ([`super::hook`], the session took it). Both go
//! through the shared reader, the shared text policy and the shared PC
//! keyboard policy; this file only reads the messages.

use std::collections::BTreeSet;

use arcen_input::keyboard::pc::{PcKey, PcModifier};
use arcen_input::keyboard::windows::{AltGrControlFilter, TimedKey, WindowsKeyReader};
use arcen_input::keyboard::{
    DeckShortcut, HostKeyEdge, PcKeyAction, PcKeyEvent, PcKeyboardInput, PcKeyboardSettings,
    SendKey, SystemChord, resolve_pc_keyboard_policy,
};
use arcen_input::text::{
    DeckTextAction, DeckTextEvent, HostTextTruth, SuppressReason, TextInputPolicy, TypingMode,
    split_commit,
};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::Ime::{
    GCS_COMPSTR, GCS_RESULTSTR, IME_COMPOSITION_STRING, ImmGetCompositionStringW, ImmGetContext,
    ImmReleaseContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, GetKeyboardLayout, MAPVK_VK_TO_VSC_EX, MapVirtualKeyW, ToUnicodeEx,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetMessageTime, MSG, PM_NOREMOVE, PM_REMOVE, PeekMessageW, TranslateMessage, WM_CHAR,
    WM_DEADCHAR, WM_IME_COMPOSITION, WM_IME_ENDCOMPOSITION, WM_INPUTLANGCHANGE, WM_KEYDOWN,
    WM_KEYFIRST, WM_KEYLAST, WM_KEYUP, WM_SYSCHAR, WM_SYSDEADCHAR, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use super::{HostInputFacts, InputEvent, hook};

/// The window's handling of one message: what to send, and the result
/// (`None`: the default procedure must see the message too).
pub(super) struct Handled {
    pub(super) result: Option<LRESULT>,
    pub(super) events: Vec<InputEvent>,
}

/// The session window's keyboard.
pub(super) struct Keyboard {
    input: PcKeyboardInput,
    reader: WindowsKeyReader,
    altgr: AltGrControlFilter,
    text: TextInputPolicy,
    /// Keys down, for repeats the hook does not mark.
    down: BTreeSet<PcKey>,
    /// Keys whose press the Deck's system kept (a system chord without the
    /// grab): their release stays there too.
    local: BTreeSet<PcKey>,
    /// The characters of a key the Deck's system keeps go to the default
    /// procedure too (Alt+Space's `WM_SYSCHAR` opens the window menu).
    pass_chars: bool,
    settings: PcKeyboardSettings,
    facts: Option<HostInputFacts>,
    typing: TypingMode,
    layout_has_altgr: bool,
    grabbed: bool,
}

impl Default for Keyboard {
    fn default() -> Self {
        Self {
            input: PcKeyboardInput::default(),
            reader: WindowsKeyReader::default(),
            altgr: AltGrControlFilter::default(),
            text: TextInputPolicy::default(),
            down: BTreeSet::new(),
            local: BTreeSet::new(),
            pass_chars: false,
            settings: PcKeyboardSettings::default(),
            facts: None,
            typing: TypingMode::default(),
            layout_has_altgr: layout_has_altgr(),
            grabbed: false,
        }
    }
}

impl Keyboard {
    /// A host's facts and the connection's settings; the releases of what
    /// the previous policy held.
    pub(super) fn set_host(
        &mut self,
        facts: HostInputFacts,
        settings: PcKeyboardSettings,
    ) -> Vec<HostKeyEdge> {
        self.facts = Some(facts);
        self.settings = settings;
        self.text.set_host_platform(facts.platform);
        self.text.set_host(HostTextTruth {
            text_commit: facts.text_commit,
            unicode: facts.unicode_text,
        });
        self.configure_text();
        self.input.set_policy(resolve_pc_keyboard_policy(
            facts.platform,
            facts.extended_keys,
            settings,
        ))
    }

    /// What the host's layout types at each key position (its keymap
    /// notice), so a key it would type differently goes as text; `None`
    /// forgets it (a new session).
    pub(super) fn set_host_keymap(
        &mut self,
        keymap: Option<&arcen_protocol::messages::HostKeymapMsg>,
    ) {
        if let Some(message) = keymap {
            tracing::info!(
                target: arcen_deck_core::log_target::INPUT,
                generation = message.generation,
                active_layout = ?message.active_layout,
                positions = message.keys.len(),
                "host keyboard layout received"
            );
        }
        self.text
            .set_host_keymap(keymap.map(arcen_input::text::HostKeyPositions::from_message));
    }

    /// Whether committed text is in use (the host types it and Typing is not
    /// Keys): only then may the window's input method compose.
    pub(super) fn text_active(&self) -> bool {
        self.text.active()
    }

    /// Whether the Deck shows a composition.
    pub(super) fn composing(&self) -> bool {
        self.text.composing()
    }

    /// Ends the composition the Deck shows (the input method's was
    /// cancelled): the events that hide it.
    pub(super) fn end_composition(&mut self) -> Vec<InputEvent> {
        let mut events = Vec::new();
        self.text_frame(&[DeckTextEvent::Preedit(String::new())], &mut events);
        events
    }

    pub(super) fn set_typing(&mut self, typing: TypingMode, host: HostTextTruth) {
        self.typing = typing;
        // A typing change keeps the host's layout notice; only a new truth
        // forgets it (the Mac changes modes with `configure` alone).
        if self.text.host() != host {
            self.text.set_host(host);
        }
        self.configure_text();
    }

    pub(super) fn set_grab(&mut self, grabbed: bool) -> Vec<HostKeyEdge> {
        self.grabbed = grabbed;
        self.input.set_grab(grabbed)
    }

    pub(super) fn held_keys(&self) -> Vec<PcKey> {
        self.input.held_keys()
    }

    pub(super) fn holds_deck_key(&self, qt_key: u32) -> bool {
        self.input.holds_deck_key(qt_key)
    }

    pub(super) fn secure_attention(&mut self) -> Vec<HostKeyEdge> {
        self.input.secure_attention()
    }

    pub(super) fn tap(&mut self, qt_key: u32) -> Vec<HostKeyEdge> {
        self.input.tap(qt_key)
    }

    pub(super) fn send_key(&mut self, key: SendKey) -> Vec<HostKeyEdge> {
        self.input.send_key(key)
    }

    /// Whether this PC has the keyboard.
    pub(super) const fn released(&self) -> bool {
        self.input.released()
    }

    /// This PC has the keyboard (`true`) or the host again: the releases of
    /// what the host held. Coming back reads the modifiers still down.
    pub(super) fn set_released(&mut self, released: bool) -> Vec<HostKeyEdge> {
        self.reader.clear();
        self.altgr.clear();
        self.pass_chars = false;
        self.down.clear();
        self.local.clear();
        if released {
            let _ = self.text.plan_frame(&[DeckTextEvent::FocusLost]);
        }
        let edges = self.input.set_released(released);
        self.focus_gained();
        edges
    }

    /// A key message while this PC has the keyboard: only the escape chord
    /// is the Deck's (`Some`); every other key goes to the window's own
    /// path (winit and egui), and nothing reaches the host.
    pub(super) fn released_key(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> bool {
        if !matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP) {
            return false;
        }
        #[allow(clippy::cast_possible_truncation, reason = "the low word is the key")]
        let vk = wparam.0 as u16;
        let bits = lparam.0;
        let (scan, extended) = key_position(
            vk,
            u32::try_from((bits >> 16) & 0xFF).unwrap_or(0),
            bits & (1 << 24) != 0,
        );
        let pressed = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
        let key = self.reader.read(vk, scan, extended, pressed);
        let repeat = pressed && !self.down.insert(key);
        if !pressed {
            self.down.remove(&key);
        }
        matches!(
            self.input.key(PcKeyEvent {
                key,
                pressed,
                repeat,
            }),
            PcKeyAction::Shortcut(DeckShortcut::ReleaseKeyboard)
        )
    }

    /// The window got focus: modifiers already held are recorded (the next
    /// key carries them), and the layout is read again.
    pub(super) fn focus_gained(&mut self) {
        const SIDES: [(i32, PcModifier); 8] = [
            (0xA0, PcModifier::LeftShift),
            (0xA1, PcModifier::RightShift),
            (0xA2, PcModifier::LeftControl),
            (0xA3, PcModifier::RightControl),
            (0xA4, PcModifier::LeftAlt),
            (0xA5, PcModifier::RightAlt),
            (0x5B, PcModifier::LeftWindows),
            (0x5C, PcModifier::RightWindows),
        ];
        // SAFETY: plain reads of this thread's key state.
        let held = SIDES
            .into_iter()
            .filter(|(vk, _)| unsafe { GetKeyState(*vk) } < 0)
            .map(|(_, modifier)| modifier);
        self.input.seed_modifiers(held);
        self.layout_has_altgr = layout_has_altgr();
        self.configure_text();
    }

    /// Releases everything held on the host and forgets every key (focus
    /// loss, Release Stuck Keys, a session ending).
    pub(super) fn release_all(&mut self) -> Vec<HostKeyEdge> {
        self.reader.clear();
        self.altgr.clear();
        self.pass_chars = false;
        self.down.clear();
        self.local.clear();
        let _ = self.text.plan_frame(&[DeckTextEvent::FocusLost]);
        self.input.release_all()
    }

    pub(super) fn handle(
        &mut self,
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<Handled> {
        let handled = |events| {
            Some(Handled {
                result: Some(LRESULT(0)),
                events,
            })
        };
        let passed = |events| {
            Some(Handled {
                result: None,
                events,
            })
        };
        match msg {
            WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
                // SAFETY: a plain read of the current message's time.
                let time = unsafe { GetMessageTime() }.cast_unsigned();
                let mut events = Vec::new();
                if self.window_key(hwnd, msg, wparam, lparam, time, &mut events) {
                    handled(events)
                } else {
                    passed(events)
                }
            }
            hook::WM_HOOK_KEY => {
                #[allow(clippy::cast_possible_truncation, reason = "packed by the hook")]
                let (vk, flags) = (wparam.0 as u16, (wparam.0 >> 16) as u32);
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "packed by the hook"
                )]
                let (scan, time) = (
                    (lparam.0 as u64 & 0xFFFF_FFFF) as u32,
                    (lparam.0 as u64 >> 32) as u32,
                );
                let mut events = Vec::new();
                // Keys the system saw before this one are still in the input
                // queue, behind this posted message: they go first.
                self.drain_keys_before(hwnd, time, &mut events);
                let pressed = flags & hook::LLKHF_UP == 0;
                let (scan, extended) = key_position(vk, scan, flags & hook::LLKHF_EXTENDED != 0);
                let key = self.reader.read(vk, scan, extended, pressed);
                self.key(key, pressed, None, &mut events);
                handled(events)
            }
            WM_CHAR | WM_SYSCHAR if std::mem::take(&mut self.pass_chars) => passed(Vec::new()),
            WM_CHAR | WM_SYSCHAR => {
                // Text no key of ours typed (VK_PACKET, an on-screen keyboard).
                #[allow(clippy::cast_possible_truncation, reason = "one UTF-16 unit")]
                let unit = wparam.0 as u16;
                let mut events = Vec::new();
                if let Some(text) = char::decode_utf16([unit]).next().and_then(Result::ok)
                    && !text.is_control()
                {
                    self.text_frame(&[DeckTextEvent::Text(text.to_string())], &mut events);
                }
                handled(events)
            }
            WM_DEADCHAR | WM_SYSDEADCHAR => handled(Vec::new()),
            WM_IME_COMPOSITION => {
                let mut events = Vec::new();
                let flags = u32::try_from(lparam.0).unwrap_or(0);
                // The result first: some input methods commit and start the
                // next composition in one message.
                let commit = if flags & GCS_RESULTSTR.0 == 0 {
                    None
                } else {
                    composition(hwnd, GCS_RESULTSTR)
                };
                if let Some(commit) = &commit {
                    self.text_frame(&[DeckTextEvent::Commit(commit.clone())], &mut events);
                }
                if flags & GCS_COMPSTR.0 != 0 {
                    let preedit = composition(hwnd, GCS_COMPSTR).unwrap_or_default();
                    self.text_frame(&[DeckTextEvent::Preedit(preedit)], &mut events);
                }
                if commit.is_some() {
                    // Taken here: no WM_IME_CHAR, so no WM_CHAR, follows.
                    return handled(events);
                }
                passed(events)
            }
            WM_IME_ENDCOMPOSITION => {
                let mut events = Vec::new();
                self.text_frame(&[DeckTextEvent::Preedit(String::new())], &mut events);
                passed(events)
            }
            WM_INPUTLANGCHANGE => {
                self.layout_has_altgr = layout_has_altgr();
                self.configure_text();
                None
            }
            _ => None,
        }
    }

    /// One key message from the window; `false` when the Deck's own system
    /// keeps it (the default procedure must see it).
    fn window_key(
        &mut self,
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        time: u32,
        events: &mut Vec<InputEvent>,
    ) -> bool {
        #[allow(clippy::cast_possible_truncation, reason = "the low word is the key")]
        let vk = wparam.0 as u16;
        let bits = lparam.0;
        let (scan, extended) = key_position(
            vk,
            u32::try_from((bits >> 16) & 0xFF).unwrap_or(0),
            bits & (1 << 24) != 0,
        );
        let pressed = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
        let key = self.reader.read(vk, scan, extended, pressed);
        let filtered = self.altgr.push(TimedKey {
            key,
            pressed,
            time_ms: time,
        });
        let count = filtered.len();
        let mut ours = true;
        for (index, timed) in filtered.into_iter().enumerate() {
            if self.window_system_chord(timed.key, timed.pressed) {
                // The Deck's system has it (Alt+Space's window menu, a
                // Windows-key chord): nothing stays held on the host.
                let edges = self.input.release_all();
                if !edges.is_empty() {
                    events.push(InputEvent::Keys(edges));
                }
                ours = false;
                self.pass_chars = timed.pressed;
                continue;
            }
            let text = if index + 1 == count && timed.pressed {
                take_chars(hwnd)
            } else {
                None
            };
            self.key(timed.key, timed.pressed, text, events);
        }
        if !more_keys_queued(hwnd)
            && let Some(held) = self.altgr.flush()
        {
            self.key(held.key, held.pressed, None, events);
        }
        ours
    }

    /// Processes the key messages queued no later than `time` (a hook key's
    /// time), translating each as the message loop would.
    fn drain_keys_before(&mut self, hwnd: HWND, time: u32, events: &mut Vec<InputEvent>) {
        loop {
            let mut msg = MSG::default();
            // SAFETY: `msg` is a valid out-parameter; nothing is removed.
            let found =
                unsafe { PeekMessageW(&raw mut msg, hwnd, WM_KEYFIRST, WM_KEYLAST, PM_NOREMOVE) }
                    .as_bool();
            let key_message = matches!(
                msg.message,
                WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP
            );
            // Message times wrap after 49.7 days; compare by difference.
            if !found || !key_message || time.wrapping_sub(msg.time) > u32::MAX / 2 {
                return;
            }
            // SAFETY: removes the message just peeked and queues its
            // characters, as TranslateMessage in the loop would have.
            unsafe {
                let _ = PeekMessageW(&raw mut msg, hwnd, msg.message, msg.message, PM_REMOVE);
                let _ = TranslateMessage(&raw const msg);
            }
            self.window_key(hwnd, msg.message, msg.wParam, msg.lParam, msg.time, events);
        }
    }

    /// Whether a window key press is a system chord the Deck's own system
    /// keeps (only without the grab), and records it so its release stays
    /// local too.
    fn window_system_chord(&mut self, key: PcKey, pressed: bool) -> bool {
        if !pressed {
            return self.local.remove(&key);
        }
        if self.grabbed || !matches!(key, PcKey::Position { .. }) {
            return false;
        }
        if SystemChord::of(key, self.input.modifiers()).is_some() {
            self.local.insert(key);
            return true;
        }
        false
    }

    /// One key edge through the text policy and the keyboard policy.
    fn key(
        &mut self,
        key: PcKey,
        pressed: bool,
        text: Option<String>,
        events: &mut Vec<InputEvent>,
    ) {
        let repeat = pressed && !self.down.insert(key);
        if !pressed {
            self.down.remove(&key);
        }
        let event = PcKeyEvent {
            key,
            pressed,
            repeat,
        };
        let mut frame = vec![DeckTextEvent::Key(self.input.text_stroke(event))];
        if let Some(text) = text {
            frame.push(DeckTextEvent::Text(text));
        }
        for action in self.text.plan_frame(&frame) {
            match action {
                DeckTextAction::ForwardKey(0) | DeckTextAction::ForwardKeyWithoutAlt(0) => {
                    match self.input.key(event) {
                        PcKeyAction::Host(edges) if !edges.is_empty() => {
                            events.push(InputEvent::Keys(edges));
                        }
                        PcKeyAction::Shortcut(action) => events.push(InputEvent::Shortcut(action)),
                        PcKeyAction::Host(_)
                        | PcKeyAction::Local
                        | PcKeyAction::Text
                        | PcKeyAction::Suppressed { .. } => {}
                    }
                }
                DeckTextAction::SuppressKey(0, SuppressReason::ComposingOption) => {
                    if let PcKey::Modifier(modifier) = key {
                        self.input.compose_option(modifier, pressed);
                    }
                }
                DeckTextAction::SendText(text) => {
                    events.extend(split_commit(&text).into_iter().map(InputEvent::Text));
                }
                DeckTextAction::Preedit(text) => events.push(InputEvent::Preedit(text)),
                DeckTextAction::ForwardKey(_)
                | DeckTextAction::ForwardKeyWithoutAlt(_)
                | DeckTextAction::SuppressKey(..) => {}
            }
        }
    }

    fn text_frame(&mut self, frame: &[DeckTextEvent], events: &mut Vec<InputEvent>) {
        for action in self.text.plan_frame(frame) {
            match action {
                DeckTextAction::SendText(text) => {
                    events.extend(split_commit(&text).into_iter().map(InputEvent::Text));
                }
                DeckTextAction::Preedit(text) => events.push(InputEvent::Preedit(text)),
                _ => {}
            }
        }
    }

    fn configure_text(&mut self) {
        let option = self.settings.altgr.option_preference(self.layout_has_altgr);
        self.text.configure(self.typing, option);
    }
}

/// Removes and returns the characters `TranslateMessage` queued for the key
/// being handled (a surrogate pair is two messages). Control characters
/// (Ctrl+C's `0x03`) are removed and dropped: that key is a shortcut.
/// A key message's scan code and extended flag. A zero scan code (a key
/// injected by Virtual-Key alone, as on-screen keyboards and remote tools
/// do) is mapped from the Virtual-Key through the active layout, which
/// alone knows the position an `OEM` Virtual-Key is.
pub(super) fn key_position(vk: u16, scan: u32, extended: bool) -> (u32, bool) {
    if scan != 0 {
        return (scan, extended);
    }
    // SAFETY: a layout lookup with no pointers.
    let mapped = unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC_EX) };
    match mapped >> 8 {
        // Pause (E1 1D): key messages report it as 0x45, never Right Control's 0x1D.
        0xE1 => (0x45, false),
        prefix => (mapped & 0xFF, extended || prefix == 0xE0),
    }
}

fn take_chars(hwnd: HWND) -> Option<String> {
    let mut units = Vec::new();
    loop {
        let mut msg = MSG::default();
        // SAFETY: `msg` is a valid out-parameter; the filter covers only
        // the character messages, and only those are removed.
        let found = unsafe { PeekMessageW(&raw mut msg, hwnd, WM_CHAR, WM_SYSCHAR, PM_NOREMOVE) };
        if !found.as_bool() || !matches!(msg.message, WM_CHAR | WM_SYSCHAR) {
            break;
        }
        // SAFETY: as above; removes the message just peeked.
        let _ = unsafe { PeekMessageW(&raw mut msg, hwnd, msg.message, msg.message, PM_REMOVE) };
        #[allow(clippy::cast_possible_truncation, reason = "one UTF-16 unit")]
        units.push(msg.wParam.0 as u16);
        if !(0xD800..0xDC00).contains(units.last().unwrap_or(&0)) {
            break;
        }
    }
    let text: String = char::decode_utf16(units)
        .filter_map(Result::ok)
        .filter(|character| !character.is_control())
        .collect();
    (!text.is_empty()).then_some(text)
}

fn more_keys_queued(hwnd: HWND) -> bool {
    let mut msg = MSG::default();
    // SAFETY: `msg` is a valid out-parameter; nothing is removed.
    unsafe { PeekMessageW(&raw mut msg, hwnd, WM_KEYFIRST, WM_KEYLAST, PM_NOREMOVE) }.as_bool()
        && matches!(
            msg.message,
            WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP
        )
}

/// The input method's composition or result string.
fn composition(hwnd: HWND, which: IME_COMPOSITION_STRING) -> Option<String> {
    // SAFETY: the context is released below; buffer sizes come from the
    // first call, in bytes.
    unsafe {
        let context = ImmGetContext(hwnd);
        if context.is_invalid() {
            return None;
        }
        let bytes = ImmGetCompositionStringW(context, which, None, 0);
        let text = usize::try_from(bytes)
            .ok()
            .filter(|bytes| *bytes > 0)
            .map(|bytes| {
                let mut units = vec![0_u16; bytes / 2];
                let read = ImmGetCompositionStringW(
                    context,
                    which,
                    Some(units.as_mut_ptr().cast()),
                    u32::try_from(bytes).unwrap_or(0),
                );
                units.truncate(usize::try_from(read).unwrap_or(0) / 2);
                String::from_utf16_lossy(&units)
            });
        let _ = ImmReleaseContext(hwnd, context);
        text
    }
}

/// Whether the active layout types characters with `AltGr`: a few keys are
/// asked what Ctrl+Alt makes of them, without disturbing dead-key state.
fn layout_has_altgr() -> bool {
    const PROBE_KEYS: [u32; 10] = [0x32, 0x33, 0x37, 0x38, 0x39, 0x30, 0x45, 0x51, 0xBB, 0xDB];
    // SAFETY: plain reads; ToUnicodeEx with flag 4 leaves the keyboard
    // state alone (Windows 10 1607 and later), and the buffers are local.
    unsafe {
        let layout = GetKeyboardLayout(0);
        let mut state = [0_u8; 256];
        state[0x11] = 0x80;
        state[0x12] = 0x80;
        state[0xA2] = 0x80;
        state[0xA5] = 0x80;
        PROBE_KEYS.iter().any(|vk| {
            let mut out = [0_u16; 8];
            ToUnicodeEx(*vk, 0, &state, &mut out, 4, layout) > 0
                && char::decode_utf16([out[0]])
                    .next()
                    .and_then(Result::ok)
                    .is_some_and(|character| !character.is_control())
        })
    }
}

#[cfg(test)]
mod tests {
    use arcen_input::keyboard::pc::PcKey;
    use arcen_input::keyboard::windows::windows_key;

    use super::key_position;

    #[test]
    fn a_zero_scan_code_takes_the_layouts_position() {
        assert_eq!(key_position(0x41, 0x1E, false), (0x1E, false));
        // VK_RIGHT without a scan code: Windows' table gives the keypad's
        // 6 (0x4D, no E0), and the reader still makes it the Right arrow.
        let (scan, extended) = key_position(0x27, 0, false);
        assert_eq!(scan, 0x4D);
        assert_eq!(
            windows_key(0x27, scan, extended),
            PcKey::Position {
                qt_key: 0x0100_0014,
                keypad: false
            }
        );
        assert_eq!(key_position(0x20, 0, false), (0x39, false));
        // VK_PAUSE: where its key messages put it, not on Right Control.
        assert_eq!(key_position(0x13, 0, false), (0x45, false));
    }

    #[test]
    fn the_hosts_layout_notice_outlives_a_typing_change_but_not_the_session() {
        use arcen_input::deck_events::HostInputFacts;
        use arcen_input::keyboard::PcKeyboardSettings;
        use arcen_input::text::{HostTextTruth, TypingMode};
        use arcen_protocol::messages::{HostKeyCharsMsg, HostKeymapMsg, HostPlatform};

        let facts = HostInputFacts {
            platform: HostPlatform::Linux,
            extended_keys: true,
            precise_scroll: true,
            text_commit: true,
            unicode_text: true,
        };
        let truth = HostTextTruth {
            text_commit: true,
            unicode: true,
        };
        let notice = HostKeymapMsg {
            generation: 3,
            active_layout: Some("us".to_owned()),
            keys: vec![HostKeyCharsMsg {
                qt_key: 0x27,
                base: Some("'".to_owned()),
                shift: Some("\"".to_owned()),
            }],
            ..HostKeymapMsg::default()
        };
        let mut keyboard = super::Keyboard::default();
        let _ = keyboard.set_host(facts, PcKeyboardSettings::default());
        keyboard.set_typing(TypingMode::Auto, truth);
        keyboard.set_host_keymap(Some(&notice));
        keyboard.set_typing(TypingMode::Characters, truth);
        keyboard.set_typing(TypingMode::Auto, truth);
        assert_eq!(
            keyboard
                .text
                .host_keymap()
                .map(arcen_input::text::HostKeyPositions::generation),
            Some(3)
        );
        // A new host (a new session's hello) forgets the old layout.
        let _ = keyboard.set_host(facts, PcKeyboardSettings::default());
        assert!(keyboard.text.host_keymap().is_none());
    }
}
