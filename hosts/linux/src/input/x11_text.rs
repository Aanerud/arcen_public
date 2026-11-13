// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Committed text on an Xorg session: what the Deck's input method, its
//! Characters mode and Paste as Keystrokes send as characters, typed through
//! the session's uinput keyboard.
//!
//! The X server's own keyboard mapping decides, per character, by the
//! shared [`arcen_input::text::route_char`] rule:
//!
//! - **By key** when the active group of the session's layout has the
//!   character at the base, Shift, `AltGr` or Shift+`AltGr` level (and the
//!   layout has an `ISO_Level3_Shift` key for the last two) and no key is
//!   held: exactly as a person would type it, so terminals, consoles and
//!   applications that read keys see ordinary key presses.
//! - **By a borrowed key** otherwise (a character the layout lacks, or keys
//!   are held): one of a few keys no layout uses (F13–F24 and two Japanese
//!   keys, only while the server maps nothing to them) is mapped to the
//!   character's keysym for as long as it is needed, then given back. This
//!   is the only way to type an arbitrary character into an X session
//!   without an input method; the borrowed key carries the keysym at both
//!   levels, so a held Shift does not change it.
//!
//! Caps Lock is turned off around a commit that has cased characters and
//! turned back on after, so what is typed is what was sent. Keys the
//! kernel or logind act on (power, sleep, `SysRq`) are never borrowed. The
//! text is never logged.

use std::time::{Duration, Instant};

use arcen_input::text::{route_char, CharRoute, LayoutLevel, LayoutProbe};
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::rust_connection::{DefaultStream, RustConnection};

/// `NoSymbol`.
const NO_SYMBOL: u32 = 0;
/// `Shift_L`.
const KEYSYM_SHIFT_L: u32 = 0xffe1;
/// `ISO_Level3_Shift` (`AltGr`).
const KEYSYM_ISO_LEVEL3_SHIFT: u32 = 0xfe03;
/// The core state's Lock (Caps Lock) bit.
const LOCK_MASK: u16 = 0x02;
/// `Caps_Lock`.
const KEYSYM_CAPS_LOCK: u32 = 0xffe5;
/// The Linux evdev codes a character may borrow: F13–F24 and the Japanese,
/// Korean and Yen keys (`KEY_ZENKAKUHANKAKU`, `KEY_RO`, `KEY_KATAKANA` …
/// `KEY_KPJPCOMMA`, `KEY_HANGEUL`, `KEY_HANJA`, `KEY_YEN`). Nothing below
/// the X server acts on them, and a layout that does not use them maps
/// nothing to most of them.
pub const BORROWABLE_EVDEV: [u16; 23] = [
    183, 184, 185, 186, 187, 188, 189, 190, 191, 192, 193, 194, 85, 89, 90, 91, 92, 93, 94, 95,
    122, 123, 124,
];
/// How long a borrowed key keeps its keysym after use before it may carry
/// another: an X client fetches a changed mapping lazily, so the mapping
/// must still be there when it reads the key press.
const BORROW_SETTLE: Duration = Duration::from_millis(150);
/// Borrowed keys unused this long are given back, by a watcher, whether or
/// not anything is typed again: typed characters do not linger in the
/// session's keymap. Far longer than [`BORROW_SETTLE`], so the last key-up
/// of a borrowed key has long been read when its keysym goes.
const BORROW_RETURN_AFTER: Duration = Duration::from_secs(2);
/// How often the watcher looks for borrowed keys to give back.
const RETURN_TICK: Duration = Duration::from_millis(500);

/// The X keysym naming `character` (`xkb_utf32_to_keysym`'s rule for
/// Latin-1 and Unicode keysyms); `None` for control characters.
#[must_use]
pub fn char_to_keysym(character: char) -> Option<u32> {
    let code = u32::from(character);
    if character.is_control() {
        None
    } else if (0x20..=0x7e).contains(&code) || (0xa0..=0xff).contains(&code) {
        Some(code)
    } else {
        Some(0x0100_0000 | code)
    }
}

/// The character a keysym types, for the keysyms [`char_to_keysym`] names
/// and the legacy Euro sign; `None` for anything else (such a key is never
/// used to type by key, so an unknown keysym is never a wrong guess).
#[must_use]
pub fn keysym_to_char(keysym: u32) -> Option<char> {
    match keysym {
        0x20..=0x7e | 0xa0..=0xff => char::from_u32(keysym),
        0x20ac => Some('€'),
        0x0100_0100..=0x0110_ffff => char::from_u32(keysym - 0x0100_0000),
        _ => None,
    }
}

/// The server's core keyboard mapping: per keycode, keysyms as the core
/// protocol lists them (group 1 levels 1–2, group 2 levels 1–2, group 1
/// levels 3–4, group 2 levels 3–4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreKeymap {
    min_keycode: u8,
    per_keycode: usize,
    keysyms: Vec<u32>,
}

/// One edge on the uinput keyboard, by evdev code.
pub type KeyEdge = (u16, bool);

impl CoreKeymap {
    /// From a `GetKeyboardMapping` reply for keycodes from `min_keycode`.
    #[must_use]
    pub fn new(min_keycode: u8, per_keycode: u8, keysyms: Vec<u32>) -> Self {
        Self {
            min_keycode,
            per_keycode: usize::from(per_keycode),
            keysyms,
        }
    }

    fn keycodes(&self) -> impl Iterator<Item = (u8, &[u32])> {
        let first = self.min_keycode;
        self.keysyms
            .chunks(self.per_keycode.max(1))
            .enumerate()
            .filter_map(move |(offset, syms)| {
                let keycode = u8::try_from(usize::from(first) + offset).ok()?;
                Some((keycode, syms))
            })
    }

    /// The uinput key (evdev code) for an X keycode, when the session's
    /// keyboard has it.
    fn evdev(keycode: u8) -> Option<u16> {
        let code = u16::from(keycode).checked_sub(8)?;
        (1..=247).contains(&code).then_some(code)
    }

    /// The keysym at `group` (0 or 1) and `level` (0–3) of a keycode's list.
    fn sym(syms: &[u32], group: u8, level: u8) -> u32 {
        let index = match (group, level) {
            (0, 0) => 0,
            (0, 1) => 1,
            (1, 0) => 2,
            (1, 1) => 3,
            (0, 2) => 4,
            (0, 3) => 5,
            (1, 2) => 6,
            (1, 3) => 7,
            _ => return NO_SYMBOL,
        };
        syms.get(index).copied().unwrap_or(NO_SYMBOL)
    }

    /// The first key with `keysym` as its own (unshifted, first-group)
    /// symbol, as an evdev code.
    #[must_use]
    pub fn modifier_key(&self, keysym: u32) -> Option<u16> {
        self.keycodes()
            .find(|(_, syms)| syms.first() == Some(&keysym))
            .and_then(|(keycode, _)| Self::evdev(keycode))
    }

    /// Where `group` of the layout types `character`: the first key, in
    /// keycode order, at a level this session can produce (`AltGr` levels
    /// only with an `AltGr` key).
    #[must_use]
    pub fn probe(&self, character: char, group: u8, altgr: bool) -> Option<(LayoutProbe, u16)> {
        let keysym = char_to_keysym(character)?;
        let levels: &[u8] = if altgr { &[0, 1, 2, 3] } else { &[0, 1] };
        for (keycode, syms) in self.keycodes() {
            let Some(evdev) = Self::evdev(keycode) else {
                continue;
            };
            if BORROWABLE_EVDEV.contains(&evdev) {
                continue;
            }
            for &level in levels {
                if Self::sym(syms, group, level) == keysym {
                    let level = match level {
                        0 => LayoutLevel::Base,
                        1 => LayoutLevel::Shift,
                        2 => LayoutLevel::AltGr,
                        _ => LayoutLevel::ShiftAltGr,
                    };
                    return Some((
                        LayoutProbe {
                            level,
                            types: keysym_to_char(keysym),
                        },
                        evdev,
                    ));
                }
            }
        }
        None
    }

    /// The borrowable keys the server maps nothing to, as evdev codes.
    #[must_use]
    pub fn free_borrowable(&self) -> Vec<u16> {
        BORROWABLE_EVDEV
            .iter()
            .copied()
            .filter(|evdev| {
                self.keycodes().any(|(keycode, syms)| {
                    Self::evdev(keycode) == Some(*evdev) && syms.iter().all(|sym| *sym == NO_SYMBOL)
                })
            })
            .collect()
    }

    /// The keysyms of the key with this evdev code.
    fn syms_of_evdev(&self, evdev: u16) -> Option<&[u32]> {
        self.keycodes()
            .find(|(keycode, _)| Self::evdev(*keycode) == Some(evdev))
            .map(|(_, syms)| syms)
    }

    /// The keysyms per keycode the server uses.
    #[must_use]
    pub const fn per_keycode(&self) -> usize {
        self.per_keycode
    }
}

/// How one character is typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CharPlan {
    /// These key edges, on the session's own layout.
    Keys(Vec<KeyEdge>),
    /// A borrowed key carrying this keysym.
    Borrow(u32),
    /// Not typeable (a control character).
    Refuse,
}

/// The session layout's modifier keys, as evdev codes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LayoutModifiers {
    /// A `Shift_L` key.
    pub shift: Option<u16>,
    /// An `ISO_Level3_Shift` key.
    pub altgr: Option<u16>,
}

/// Plans `character` on `keymap`'s `group`, by the shared route rule.
#[must_use]
pub fn plan_char(
    keymap: &CoreKeymap,
    modifiers: LayoutModifiers,
    character: char,
    group: u8,
    keys_held: bool,
) -> CharPlan {
    let Some(keysym) = char_to_keysym(character) else {
        return CharPlan::Refuse;
    };
    let probe = keymap.probe(character, group, modifiers.altgr.is_some());
    match route_char(character, probe.map(|(probe, _)| probe), keys_held, false) {
        CharRoute::Key { shift, altgr } => {
            let Some((_, key)) = probe else {
                return CharPlan::Borrow(keysym);
            };
            let mut held = Vec::new();
            if shift {
                match modifiers.shift {
                    Some(code) => held.push(code),
                    None => return CharPlan::Borrow(keysym),
                }
            }
            if altgr {
                match modifiers.altgr {
                    Some(code) => held.push(code),
                    None => return CharPlan::Borrow(keysym),
                }
            }
            let mut edges: Vec<KeyEdge> = held.iter().map(|code| (*code, true)).collect();
            edges.push((key, true));
            edges.push((key, false));
            edges.extend(held.iter().rev().map(|code| (*code, false)));
            CharPlan::Keys(edges)
        }
        CharRoute::Text => CharPlan::Borrow(keysym),
    }
}

/// A borrowed key.
#[derive(Debug, Clone, Copy)]
struct Borrowed {
    evdev: u16,
    keysym: Option<u32>,
    used: Option<Instant>,
}

/// What became of a commit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct X11TextReport {
    /// Characters typed by their key.
    pub by_key: u32,
    /// Characters typed by a borrowed key.
    pub borrowed: u32,
    /// Characters not typed.
    pub refused: u32,
}

/// The borrowed keys due back at `now`: every carrying key with `all`, else
/// those unused for [`BORROW_RETURN_AFTER`].
fn due_for_return(borrowed: &[Borrowed], now: Instant, all: bool) -> Vec<u16> {
    borrowed
        .iter()
        .filter(|key| {
            key.keysym.is_some()
                && (all
                    || key
                        .used
                        .is_none_or(|used| now.duration_since(used) >= BORROW_RETURN_AFTER))
        })
        .map(|key| key.evdev)
        .collect()
}

/// The session's connection to its X server for committed text, with a
/// watcher that gives borrowed keys back once they have been idle.
pub struct X11Text {
    session: std::sync::Arc<std::sync::Mutex<Session>>,
    stop: std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    watcher: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for X11Text {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("X11Text").finish_non_exhaustive()
    }
}

impl X11Text {
    /// Connects to `display` (see [`Session::connect`]) and starts the
    /// watcher.
    ///
    /// # Errors
    ///
    /// When the display cannot be reached or refuses the connection.
    pub fn connect(display: &str, xauthority: Option<&std::path::Path>) -> Result<Self, String> {
        use std::sync::{Arc, Condvar, Mutex};
        let session = Arc::new(Mutex::new(Session::connect(display, xauthority)?));
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let watcher = {
            let session = Arc::clone(&session);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("arcen-x11-text-return".into())
                .spawn(move || loop {
                    let (stopped, wake) = &*stop;
                    let Ok(guard) = stopped.lock() else { return };
                    let Ok((guard, _)) = wake.wait_timeout_while(guard, RETURN_TICK, |stop| !*stop)
                    else {
                        return;
                    };
                    if *guard {
                        return;
                    }
                    drop(guard);
                    // The commit in progress, if any, holds the session:
                    // keys are never given back in the middle of one.
                    if let Ok(mut session) = session.lock() {
                        session.return_borrowed(Instant::now(), false);
                    }
                })
                .map_err(|error| format!("committed-text watcher: {error}"))?
        };
        Ok(Self {
            session,
            stop,
            watcher: Some(watcher),
        })
    }

    /// Types `text`; see [`Session::type_text`].
    ///
    /// # Errors
    ///
    /// When the X server stops answering.
    pub fn type_text(
        &mut self,
        text: &str,
        keys_held: bool,
        emit: &mut dyn FnMut(KeyEdge) -> Result<(), String>,
    ) -> Result<X11TextReport, String> {
        self.session
            .lock()
            .map_err(|_| "committed-text session poisoned".to_owned())?
            .type_text(text, keys_held, emit)
    }
}

impl Drop for X11Text {
    fn drop(&mut self) {
        let (stopped, wake) = &*self.stop;
        if let Ok(mut stopped) = stopped.lock() {
            *stopped = true;
        }
        wake.notify_all();
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
        if let Ok(mut session) = self.session.lock() {
            session.return_borrowed(Instant::now(), true);
            let _ = session.connection.flush();
        }
    }
}

/// The X connection and the keys it has borrowed.
struct Session {
    connection: RustConnection,
    root: u32,
    borrowed: Vec<Borrowed>,
    per_keycode: u8,
}

impl Session {
    /// Connects to `display`, authenticating with the `MIT-MAGIC-COOKIE-1`
    /// in `xauthority` when given (read here, never from the environment).
    fn connect(display: &str, xauthority: Option<&std::path::Path>) -> Result<Self, String> {
        let parsed = x11rb::reexports::x11rb_protocol::parse_display::parse_display(Some(display))
            .map_err(|error| format!("X display {display:?}: {error}"))?;
        let screen = usize::from(parsed.screen);
        let mut last_error = None;
        for address in parsed.connect_instruction() {
            match DefaultStream::connect(&address) {
                Ok((stream, (family, peer))) => {
                    let (name, data) = xauthority
                        .and_then(|path| std::fs::read(path).ok())
                        .and_then(|file| cookie_for(&file, family, &peer, parsed.display))
                        .unwrap_or_default();
                    let connection = RustConnection::connect_to_stream_with_auth_info(
                        stream, screen, name, data,
                    )
                    .map_err(|error| format!("X connection setup: {error}"))?;
                    let root = connection
                        .setup()
                        .roots
                        .get(screen)
                        .ok_or("X screen missing")?
                        .root;
                    return Ok(Self {
                        connection,
                        root,
                        borrowed: Vec::new(),
                        per_keycode: 2,
                    });
                }
                Err(error) => last_error = Some(error.to_string()),
            }
        }
        Err(format!(
            "X display {display:?} unreachable: {}",
            last_error.unwrap_or_default()
        ))
    }

    fn keymap(&self) -> Result<CoreKeymap, String> {
        let setup = self.connection.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let reply = self
            .connection
            .get_keyboard_mapping(min, max - min + 1)
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|error| error.to_string())?;
        Ok(CoreKeymap::new(
            min,
            reply.keysyms_per_keycode,
            reply.keysyms,
        ))
    }

    /// The core state: the active group and whether Caps Lock is on.
    fn state(&self) -> Result<(u8, bool), String> {
        let reply = self
            .connection
            .query_pointer(self.root)
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|error| error.to_string())?;
        let mask = u16::from(reply.mask);
        let group = u8::try_from((mask >> 13) & 0x3).unwrap_or(0);
        Ok((group, mask & LOCK_MASK != 0))
    }

    fn sync(&self) -> Result<(), String> {
        self.connection
            .get_input_focus()
            .map_err(|error| error.to_string())?
            .reply()
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn map_key(&self, evdev: u16, keysym: u32) -> Result<(), String> {
        let keycode = u8::try_from(evdev + 8).map_err(|_| "keycode out of range")?;
        let keysyms = vec![keysym; usize::from(self.per_keycode)];
        self.connection
            .change_keyboard_mapping(1, keycode, self.per_keycode, &keysyms)
            .map_err(|error| error.to_string())?;
        self.sync()
    }

    /// Gives back borrowed keys unused for a while (all of them with
    /// `all`).
    fn return_borrowed(&mut self, now: Instant, all: bool) {
        let returning = due_for_return(&self.borrowed, now, all);
        for evdev in returning {
            if self.map_key(evdev, NO_SYMBOL).is_ok() {
                if let Some(key) = self.borrowed.iter_mut().find(|key| key.evdev == evdev) {
                    key.keysym = None;
                    key.used = None;
                }
            }
        }
    }

    /// Forgets what the server no longer holds: a desktop that reloads its
    /// layout drops borrowed keysyms, and a key it now maps is no longer
    /// free to borrow.
    fn reconcile(&mut self, keymap: &CoreKeymap) {
        self.borrowed.retain_mut(|key| {
            let Some(syms) = keymap.syms_of_evdev(key.evdev) else {
                return false;
            };
            if syms.iter().all(|sym| *sym == NO_SYMBOL) {
                key.keysym = None;
                return true;
            }
            key.keysym
                .is_some_and(|keysym| syms.first() == Some(&keysym))
        });
    }

    /// A key carrying `keysym`, borrowing one if needed; when every key was
    /// used too recently to change, waits (at most [`BORROW_SETTLE`]) for
    /// the longest unused one. `None` when there is no key to borrow.
    fn borrow_settled(&mut self, keysym: u32) -> Result<Option<u16>, String> {
        if let Some(evdev) = self.borrow(keysym, Instant::now())? {
            return Ok(Some(evdev));
        }
        let now = Instant::now();
        let Some(oldest) = self.borrowed.iter().filter_map(|key| key.used).min() else {
            return Ok(None);
        };
        std::thread::sleep(BORROW_SETTLE.saturating_sub(now.duration_since(oldest)));
        self.borrow(keysym, Instant::now())
    }

    /// A key carrying `keysym`: one already carrying it, else a free one,
    /// else the one longest unused once it has settled. `None` when none is
    /// free now.
    fn borrow(&mut self, keysym: u32, now: Instant) -> Result<Option<u16>, String> {
        if let Some(key) = self
            .borrowed
            .iter_mut()
            .find(|key| key.keysym == Some(keysym))
        {
            key.used = Some(now);
            return Ok(Some(key.evdev));
        }
        let candidate = self
            .borrowed
            .iter()
            .enumerate()
            .filter(|(_, key)| {
                key.used
                    .is_none_or(|used| now.duration_since(used) >= BORROW_SETTLE)
            })
            .min_by_key(|(_, key)| key.used)
            .map(|(index, _)| index);
        let Some(index) = candidate else {
            return Ok(None);
        };
        let evdev = self.borrowed[index].evdev;
        self.map_key(evdev, keysym)?;
        self.borrowed[index].keysym = Some(keysym);
        self.borrowed[index].used = Some(now);
        Ok(Some(evdev))
    }

    /// Types `text`, emitting key edges through `emit` one at a time.
    /// `keys_held`: the session holds keys the user pressed, so nothing is
    /// typed by its own key.
    ///
    /// # Errors
    ///
    /// When the X server stops answering; what was typed until then stays
    /// typed.
    fn type_text(
        &mut self,
        text: &str,
        keys_held: bool,
        emit: &mut dyn FnMut(KeyEdge) -> Result<(), String>,
    ) -> Result<X11TextReport, String> {
        let now = Instant::now();
        self.return_borrowed(now, false);
        let keymap = self.keymap()?;
        self.per_keycode = u8::try_from(keymap.per_keycode().clamp(1, 8)).unwrap_or(2);
        if self.borrowed.is_empty() {
            self.borrowed = keymap
                .free_borrowable()
                .into_iter()
                .map(|evdev| Borrowed {
                    evdev,
                    keysym: None,
                    used: None,
                })
                .collect();
        }
        let modifiers = LayoutModifiers {
            shift: keymap.modifier_key(KEYSYM_SHIFT_L),
            altgr: keymap.modifier_key(KEYSYM_ISO_LEVEL3_SHIFT),
        };
        self.reconcile(&keymap);
        let (group, caps) = self.state()?;
        // Caps Lock would change cased characters, typed by key or not: it
        // is turned off around them by the layout's own Caps Lock key.
        let caps_key = keymap.modifier_key(KEYSYM_CAPS_LOCK).filter(|_| {
            caps && text
                .chars()
                .any(|character| character.is_lowercase() || character.is_uppercase())
        });
        if let Some(key) = caps_key {
            emit((key, true))?;
            emit((key, false))?;
        }
        let mut report = X11TextReport::default();
        let result = (|| {
            for character in text.chars() {
                match plan_char(&keymap, modifiers, character, group, keys_held) {
                    CharPlan::Keys(edges) => {
                        for edge in edges {
                            emit(edge)?;
                        }
                        report.by_key += 1;
                    }
                    CharPlan::Borrow(keysym) => match self.borrow_settled(keysym)? {
                        Some(evdev) => {
                            emit((evdev, true))?;
                            emit((evdev, false))?;
                            report.borrowed += 1;
                        }
                        None => report.refused += 1,
                    },
                    CharPlan::Refuse => report.refused += 1,
                }
            }
            Ok::<(), String>(())
        })();
        if let Some(key) = caps_key {
            emit((key, true))?;
            emit((key, false))?;
        }
        result.map(|()| report)
    }
}

/// The `MIT-MAGIC-COOKIE-1` entry of an Xauthority file for a connection
/// (`family`, `peer`) to display `number`: as Xlib matches it (a wildcard
/// family, or the same family and address; an empty or equal display
/// number).
fn cookie_for(
    file: &[u8],
    family: x11rb::reexports::x11rb_protocol::xauth::Family,
    peer: &[u8],
    number: u16,
) -> Option<(Vec<u8>, Vec<u8>)> {
    use x11rb::reexports::x11rb_protocol::xauth::Family;
    const COOKIE: &[u8] = b"MIT-MAGIC-COOKIE-1";
    fn field<'a>(file: &'a [u8], at: &mut usize) -> Option<&'a [u8]> {
        let length = usize::from(u16::from_be_bytes([*file.get(*at)?, *file.get(*at + 1)?]));
        let value = file.get(*at + 2..*at + 2 + length)?;
        *at += 2 + length;
        Some(value)
    }
    let number = number.to_string();
    let mut at = 0;
    while at < file.len() {
        let entry_family = Family::from(u16::from_be_bytes([*file.get(at)?, *file.get(at + 1)?]));
        at += 2;
        let address = field(file, &mut at)?;
        let display = field(file, &mut at)?;
        let name = field(file, &mut at)?;
        let data = field(file, &mut at)?;
        let address_matches = entry_family == Family::WILD
            || family == Family::WILD
            || (entry_family == family && address == peer);
        if address_matches && (display.is_empty() || display == number.as_bytes()) && name == COOKIE
        {
            return Some((name.to_vec(), data.to_vec()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const PER: u8 = 4;

    fn set(keysyms: &mut [u32], per: u8, keycode: usize, syms: &[u32]) {
        for (index, sym) in syms.iter().enumerate() {
            keysyms[(keycode - 8) * usize::from(per) + index] = *sym;
        }
    }

    /// A small US-like map with Norwegian-like AltGr keys: keycodes from 8.
    fn keymap(altgr_key: bool) -> CoreKeymap {
        let mut keysyms = vec![NO_SYMBOL; 248 * usize::from(PER)];
        let k = &mut keysyms;
        set(k, PER, 38, &[0x61, 0x41, 0x61, 0x41]);
        set(k, PER, 11, &[0x32, 0x22, 0x32, 0x22]);
        set(k, PER, 50, &[KEYSYM_SHIFT_L, NO_SYMBOL, KEYSYM_SHIFT_L]);
        set(k, PER, 65, &[0x20, NO_SYMBOL, 0x20]);
        set(k, PER, 47, &[0xf8, 0xd8, 0xf8, 0xd8]); // ø Ø
        set(k, PER, 66, &[KEYSYM_CAPS_LOCK]);
        if altgr_key {
            set(k, PER, 108, &[KEYSYM_ISO_LEVEL3_SHIFT]);
        }
        // F13 (evdev 183, keycode 191) is mapped to XF86Tools; F19 is free.
        set(k, PER, 191, &[0x1008_ff81, NO_SYMBOL, 0x1008_ff81]);
        CoreKeymap::new(8, PER, keysyms)
    }

    fn modifiers(map: &CoreKeymap) -> LayoutModifiers {
        LayoutModifiers {
            shift: map.modifier_key(KEYSYM_SHIFT_L),
            altgr: map.modifier_key(KEYSYM_ISO_LEVEL3_SHIFT),
        }
    }

    #[test]
    fn keysyms_and_characters_convert_both_ways() {
        assert_eq!(char_to_keysym('a'), Some(0x61));
        assert_eq!(char_to_keysym('ø'), Some(0xf8));
        assert_eq!(char_to_keysym('€'), Some(0x0100_20ac));
        assert_eq!(char_to_keysym('😀'), Some(0x0101_f600));
        assert_eq!(char_to_keysym('\n'), None);
        for character in ['a', 'ø', '€', '😀', 'Ж'] {
            assert_eq!(
                keysym_to_char(char_to_keysym(character).unwrap()),
                Some(character)
            );
        }
        assert_eq!(keysym_to_char(0x20ac), Some('€'));
        // Keysyms with no known character are never matched.
        assert_eq!(keysym_to_char(KEYSYM_SHIFT_L), None);
        assert_eq!(keysym_to_char(0x6c1), None);
    }

    #[test]
    fn layout_characters_are_typed_by_key_with_shift() {
        let map = keymap(false);
        let modifiers = modifiers(&map);
        assert_eq!(modifiers.shift, Some(42));
        assert_eq!(
            plan_char(&map, modifiers, 'a', 0, false),
            CharPlan::Keys(vec![(30, true), (30, false)])
        );
        assert_eq!(
            plan_char(&map, modifiers, 'Ø', 0, false),
            CharPlan::Keys(vec![(42, true), (39, true), (39, false), (42, false)])
        );
        assert_eq!(
            plan_char(&map, modifiers, ' ', 0, false),
            CharPlan::Keys(vec![(57, true), (57, false)])
        );
    }

    #[test]
    fn missing_characters_and_held_keys_borrow_a_key() {
        let map = keymap(false);
        let modifiers = modifiers(&map);
        assert_eq!(
            plan_char(&map, modifiers, 'æ', 0, false),
            CharPlan::Borrow(0xe6)
        );
        assert_eq!(
            plan_char(&map, modifiers, '😀', 0, false),
            CharPlan::Borrow(0x0101_f600)
        );
        // Keys held by the user: nothing by its own key.
        assert_eq!(
            plan_char(&map, modifiers, 'a', 0, true),
            CharPlan::Borrow(0x61)
        );
        assert_eq!(plan_char(&map, modifiers, '\t', 0, false), CharPlan::Refuse);
    }

    #[test]
    fn altgr_levels_need_an_altgr_key() {
        let mut wide = vec![NO_SYMBOL; 248 * 6];
        set(&mut wide, 6, 11, &[0x32, 0x22, 0x32, 0x22, 0x40, NO_SYMBOL]);
        set(&mut wide, 6, 50, &[KEYSYM_SHIFT_L]);
        let without = CoreKeymap::new(8, 6, wide.clone());
        assert_eq!(
            plan_char(&without, modifiers(&without), '@', 0, false),
            CharPlan::Borrow(0x40)
        );
        set(&mut wide, 6, 108, &[KEYSYM_ISO_LEVEL3_SHIFT]);
        let with = CoreKeymap::new(8, 6, wide);
        assert_eq!(
            plan_char(&with, modifiers(&with), '@', 0, false),
            CharPlan::Keys(vec![(100, true), (3, true), (3, false), (100, false)])
        );
    }

    #[test]
    fn the_second_group_is_read_from_its_own_columns() {
        let map = keymap(false);
        let mut keysyms = map.keysyms.clone();
        // Group 2 types 'æ' where group 1 types 'a'.
        keysyms[(38 - 8) * usize::from(PER) + 2] = 0xe6;
        keysyms[(38 - 8) * usize::from(PER) + 3] = 0xc6;
        let map = CoreKeymap::new(8, PER, keysyms);
        let modifiers = modifiers(&map);
        assert_eq!(
            plan_char(&map, modifiers, 'æ', 1, false),
            CharPlan::Keys(vec![(30, true), (30, false)])
        );
        assert_eq!(
            plan_char(&map, modifiers, 'æ', 0, false),
            CharPlan::Borrow(0xe6)
        );
    }

    #[test]
    fn borrowed_keys_go_back_once_idle_and_never_while_recently_used() {
        let now = Instant::now();
        let key = |evdev, keysym, idle: Option<u64>| Borrowed {
            evdev,
            keysym,
            used: idle.map(|ms| now - Duration::from_millis(ms)),
        };
        let borrowed = [
            key(189, Some(0xe6), Some(100)), // just typed (key-up in flight)
            key(190, Some(0xf8), Some(1_999)),
            key(191, Some(0xe5), Some(2_000)), // idle long enough
            key(192, None, Some(5_000)),       // carries nothing
        ];
        assert_eq!(due_for_return(&borrowed, now, false), vec![191]);
        assert_eq!(due_for_return(&borrowed, now, true), vec![189, 190, 191]);
    }

    #[test]
    fn only_free_borrowable_keys_are_borrowed() {
        let map = keymap(false);
        let free = map.free_borrowable();
        assert!(!free.contains(&183), "F13 carries XF86Tools here");
        assert_eq!(map.modifier_key(KEYSYM_CAPS_LOCK), Some(58));
        assert!(free.contains(&189) && free.contains(&194));
        for evdev in free {
            // Never a key the kernel or logind acts on.
            assert!(![99, 116, 142, 143, 205, 247].contains(&evdev));
        }
    }

    #[test]
    fn the_xauthority_cookie_is_found_by_family_address_and_display() {
        use x11rb::reexports::x11rb_protocol::xauth::Family;
        fn entry(family: u16, address: &[u8], number: &[u8], name: &[u8], data: &[u8]) -> Vec<u8> {
            let mut out = family.to_be_bytes().to_vec();
            for field in [address, number, name, data] {
                out.extend(u16::try_from(field.len()).unwrap().to_be_bytes());
                out.extend(field);
            }
            out
        }
        let mut file = entry(256, b"other-host", b"1", b"MIT-MAGIC-COOKIE-1", b"wrong");
        file.extend(entry(256, b"pier", b"2", b"XDM-AUTHORIZATION-1", b"skip"));
        file.extend(entry(256, b"pier", b"2", b"MIT-MAGIC-COOKIE-1", b"right"));
        assert_eq!(
            cookie_for(&file, Family::LOCAL, b"pier", 2),
            Some((b"MIT-MAGIC-COOKIE-1".to_vec(), b"right".to_vec()))
        );
        assert_eq!(cookie_for(&file, Family::LOCAL, b"pier", 3), None);
        let wild = entry(65535, b"", b"", b"MIT-MAGIC-COOKIE-1", b"any");
        assert_eq!(
            cookie_for(&wild, Family::LOCAL, b"pier", 7).map(|(_, data)| data),
            Some(b"any".to_vec())
        );
        assert_eq!(cookie_for(&[0, 1, 0], Family::LOCAL, b"pier", 0), None);
    }
}
